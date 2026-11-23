//! Document text extraction for the universal read.
//!
//! A plain file read cannot open a PDF, Word, Excel, PowerPoint,
//! OpenDocument, EPUB or RTF file; this module extracts their text so the
//! model gets words, not a hex dump. It never runs macros, scripts or
//! embedded objects and never follows links; it only reads the parts that
//! hold text.
//!
//! Rewritten for Lu-Core rather than ported. The old repo
//! (Lu-AI-GUI src-tauri/lu-core/src/native_reader*.rs) hand-parsed PDF and
//! refused any PDF with object streams or font maps, which is most PDFs
//! written by Word or a browser. Here PDF goes through `pdf-extract`
//! (lopdf), spreadsheets through `calamine`, and the zipped XML formats
//! through `zip` + `quick-xml`, with Lu's own size limits around them.
//!
//! What is not covered, and says so instead of guessing: scanned pages with
//! no text layer (OCR is not built in), encrypted PDFs, legacy binary
//! `.doc` and `.ppt`.

use std::{
    collections::HashMap,
    io::{Cursor, Read},
    panic::{catch_unwind, AssertUnwindSafe},
};

use quick_xml::{events::Event, Reader};

/// Largest document Lu extracts text from.
pub const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Largest single part (one XML file) read out of a zipped document. Guards
/// against zip bombs: a tiny file that inflates to gigabytes.
pub const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
/// Default number of characters returned.
pub const DEFAULT_MAX_CHARS: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Docx,
    Xlsx,
    Xls,
    Xlsb,
    Pptx,
    Odt,
    Ods,
    Odp,
    Epub,
    Rtf,
    /// Legacy binary Word / PowerPoint: recognized, not readable.
    LegacyOffice,
}
impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Xls => "xls",
            Self::Xlsb => "xlsb",
            Self::Pptx => "pptx",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::Epub => "epub",
            Self::Rtf => "rtf",
            Self::LegacyOffice => "legacy-office",
        }
    }
    /// What one section is called in the output and in selectors.
    fn section(self) -> &'static str {
        match self {
            Self::Pdf => "page",
            Self::Xlsx | Self::Xls | Self::Xlsb | Self::Ods => "sheet",
            Self::Pptx | Self::Odp => "slide",
            Self::Epub => "chapter",
            _ => "part",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderError {
    TooLarge { size: u64 },
    /// The file is not a valid document of its kind.
    Invalid(String),
    /// A real document Lu cannot read, with the reason (encrypted, no text
    /// layer, legacy format).
    Unsupported(String),
    /// The selector names no page, sheet, slide or chapter in the file.
    NoSuchSection(String),
}
impl std::fmt::Display for ReaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { size } => write!(f, "document too large ({size} bytes)"),
            Self::Invalid(why) => write!(f, "not a valid document: {why}"),
            Self::Unsupported(why) => write!(f, "cannot read this document: {why}"),
            Self::NoSuchSection(s) => write!(f, "no such section: {s}"),
        }
    }
}
impl std::error::Error for ReaderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// `page 3`, `sheet Budget`, `slide 2`, `chapter 4`.
    pub label: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub format: Format,
    /// Sections in the document (pages, sheets, slides, chapters).
    pub total_sections: usize,
    /// The text, each section headed `[page 1]` and so on.
    pub text: String,
    pub truncated: bool,
}

/// Which document format a file is, from its first bytes and its name. The
/// content signature decides; the extension only tells zipped formats
/// apart. `None` means not a document (plain text or other binary).
pub fn classify(name: &str, head: &[u8]) -> Option<Format> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if head.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }
    if head.starts_with(b"{\\rtf") {
        return Some(Format::Rtf);
    }
    if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        // OLE compound file: old Excel is readable, old Word/PowerPoint not.
        return Some(if ext == "xls" { Format::Xls } else { Format::LegacyOffice });
    }
    if head.starts_with(b"PK\x03\x04") {
        return match ext.as_str() {
            "docx" | "docm" | "dotx" => Some(Format::Docx),
            "xlsx" | "xlsm" | "xltx" => Some(Format::Xlsx),
            "xlsb" => Some(Format::Xlsb),
            "pptx" | "pptm" | "potx" => Some(Format::Pptx),
            "odt" => Some(Format::Odt),
            "ods" => Some(Format::Ods),
            "odp" => Some(Format::Odp),
            "epub" => Some(Format::Epub),
            _ => None,
        };
    }
    None
}

/// Extract the text of a document. `selector` picks one section:
/// `page:3` (PDF), `sheet:Budget` (spreadsheets), `slide:2` or `notes:2`
/// (presentations), `chapter:4` (EPUB). Output is cut at `max_chars`.
pub fn extract(name: &str, bytes: &[u8], selector: Option<&str>, max_chars: usize) -> Result<Extracted, ReaderError> {
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(ReaderError::TooLarge { size: bytes.len() as u64 });
    }
    let format = classify(name, &bytes[..bytes.len().min(16)])
        .ok_or_else(|| ReaderError::Invalid("not a recognized document format".into()))?;
    let mut sections = match format {
        Format::Pdf => pdf(bytes)?,
        Format::Docx => vec![Section { label: "document".into(), text: docx(bytes)? }],
        Format::Xlsx | Format::Xls | Format::Xlsb | Format::Ods => spreadsheet(bytes)?,
        Format::Pptx => pptx(bytes, selector.is_some_and(|s| s.starts_with("notes:")))?,
        Format::Odt => vec![Section { label: "document".into(), text: odt(bytes)? }],
        Format::Odp => odp(bytes)?,
        Format::Epub => epub(bytes)?,
        Format::Rtf => vec![Section { label: "document".into(), text: rtf(bytes)? }],
        Format::LegacyOffice => {
            return Err(ReaderError::Unsupported(
                "legacy binary .doc/.ppt; save it as .docx/.pptx, or convert it with an installed office suite".into(),
            ))
        }
    };
    let total_sections = sections.len();
    if let Some(sel) = selector {
        let (kind, value) = sel.split_once(':').ok_or_else(|| ReaderError::NoSuchSection(sel.into()))?;
        let kind = if kind == "notes" { "slide" } else { kind };
        if kind != format.section() {
            return Err(ReaderError::NoSuchSection(format!("{sel} (this {} has {}s)", format.as_str(), format.section())));
        }
        let wanted = format!("{kind} {value}");
        sections.retain(|s| s.label == wanted);
        if sections.is_empty() {
            return Err(ReaderError::NoSuchSection(format!("{sel} (the file has {total_sections})")));
        }
    }
    if format == Format::Pdf && sections.iter().all(|s| s.text.trim().is_empty()) {
        return Err(ReaderError::Unsupported("no text layer (a scanned PDF needs OCR, which is not built in)".into()));
    }
    let mut text = String::new();
    for s in &sections {
        if total_sections > 1 || selector.is_some() {
            text.push_str(&format!("[{}]\n", s.label));
        }
        text.push_str(s.text.trim_end());
        text.push('\n');
    }
    let truncated = text.chars().count() > max_chars;
    if truncated {
        text = text.chars().take(max_chars).collect();
    }
    Ok(Extracted { format, total_sections, text, truncated })
}

// ---------------------------------------------------------------- PDF

fn pdf(bytes: &[u8]) -> Result<Vec<Section>, ReaderError> {
    // pdf-extract can panic on malformed files; a bad file must not take
    // Lu down.
    let pages = catch_unwind(AssertUnwindSafe(|| pdf_extract::extract_text_from_mem_by_pages(bytes)))
        .map_err(|_| ReaderError::Invalid("the PDF parser failed on this file".into()))?
        .map_err(|e| {
            let why = e.to_string();
            if why.to_ascii_lowercase().contains("encrypt") {
                ReaderError::Unsupported("the PDF is encrypted".into())
            } else {
                ReaderError::Invalid(why)
            }
        })?;
    Ok(pages
        .into_iter()
        .enumerate()
        .map(|(i, text)| Section { label: format!("page {}", i + 1), text: tidy(&text) })
        .collect())
}

/// Collapse the runs of blank lines PDF extraction tends to produce.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_owned()
}

// ---------------------------------------------------------------- zipped XML

struct Zip(zip::ZipArchive<Cursor<Vec<u8>>>);
impl Zip {
    fn open(bytes: &[u8]) -> Result<Self, ReaderError> {
        zip::ZipArchive::new(Cursor::new(bytes.to_vec()))
            .map(Self)
            .map_err(|e| ReaderError::Invalid(format!("zip: {e}")))
    }
    fn has(&mut self, path: &str) -> bool {
        self.0.by_name(path).is_ok()
    }
    fn count(&self) -> usize {
        self.0.len()
    }
    /// One part as text, bounded.
    fn part(&mut self, path: &str) -> Result<String, ReaderError> {
        let entry = self.0.by_name(path).map_err(|_| ReaderError::Invalid(format!("missing part {path}")))?;
        let mut out = Vec::new();
        entry
            .take(MAX_PART_BYTES + 1)
            .read_to_end(&mut out)
            .map_err(|e| ReaderError::Invalid(format!("{path}: {e}")))?;
        if out.len() as u64 > MAX_PART_BYTES {
            return Err(ReaderError::TooLarge { size: out.len() as u64 });
        }
        String::from_utf8(out).map_err(|_| ReaderError::Invalid(format!("{path} is not UTF-8")))
    }
}

/// How to turn one kind of XML into text.
struct XmlText<'a> {
    /// Elements whose text is kept; empty keeps all text.
    text: &'a [&'a str],
    /// Elements that end a paragraph (a newline after them).
    para: &'a [&'a str],
    /// Elements that stand for a tab.
    tab: &'a [&'a str],
    /// Elements that stand for a line break.
    brk: &'a [&'a str],
    /// Elements whose whole subtree is dropped.
    skip: &'a [&'a str],
    /// Elements that start a new section (ODP pages); none means one.
    section: Option<&'a str>,
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, l)| l)
}
fn entity(name: &str) -> Option<char> {
    Some(match name {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => return None,
    })
}

impl XmlText<'_> {
    fn sections(&self, xml: &str) -> Result<Vec<String>, ReaderError> {
        let mut reader = Reader::from_str(xml);
        let mut sections = Vec::new();
        let mut out = String::new();
        let mut stack: Vec<String> = Vec::new();
        let mut skipping = 0usize; // depth inside a skipped subtree
        let keep = |stack: &[String]| self.text.is_empty() || stack.iter().any(|t| self.text.contains(&t.as_str()));
        loop {
            let event = reader.read_event().map_err(|e| ReaderError::Invalid(format!("xml: {e}")))?;
            match event {
                Event::Start(e) => {
                    let name = local(e.name().as_ref()).to_owned();
                    if skipping > 0 || self.skip.contains(&name.as_str()) {
                        skipping += 1;
                    } else if self.section == Some(name.as_str()) {
                        out.clear(); // text between sections is not content
                    }
                    stack.push(name);
                }
                Event::Empty(e) => {
                    if skipping > 0 {
                        continue;
                    }
                    let qname = e.name();
                    let name = local(qname.as_ref());
                    if self.tab.contains(&name) {
                        out.push('\t');
                    } else if self.brk.contains(&name) || self.para.contains(&name) {
                        out.push('\n');
                    } else if name == "s" && self.text.is_empty() {
                        out.push(' '); // ODF text:s, a run of spaces
                    }
                }
                Event::End(e) => {
                    let name = local(e.name().as_ref()).to_owned();
                    stack.pop();
                    if skipping > 0 {
                        skipping -= 1;
                        continue;
                    }
                    if self.para.contains(&name.as_str()) && !out.ends_with('\n') {
                        out.push('\n');
                    } else if self.tab.contains(&name.as_str()) {
                        out.push('\t');
                    } else if self.brk.contains(&name.as_str()) {
                        out.push('\n');
                    }
                    if self.section == Some(name.as_str()) {
                        sections.push(std::mem::take(&mut out));
                    }
                }
                Event::Text(t) if skipping == 0 && keep(&stack) => {
                    let text = t.xml10_content();
                    if self.text.is_empty() {
                        // Markup whitespace between elements is not content.
                        if text.trim().is_empty() {
                            if !out.ends_with(char::is_whitespace) && !out.is_empty() && text.contains(' ') {
                                out.push(' ');
                            }
                            continue;
                        }
                        if text.starts_with(char::is_whitespace) && !out.is_empty() && !out.ends_with(char::is_whitespace) {
                            out.push(' ');
                        }
                        out.push_str(&text.split_whitespace().collect::<Vec<_>>().join(" "));
                        if text.ends_with(char::is_whitespace) {
                            out.push(' ');
                        }
                    } else {
                        out.push_str(&text);
                    }
                }
                Event::CData(t) if skipping == 0 && keep(&stack) => {
                    out.push_str(&t);
                }
                Event::GeneralRef(r) if skipping == 0 && keep(&stack) => {
                    if r.is_char_ref() {
                        if let Ok(Some(c)) = r.resolve_char_ref() {
                            out.push(c);
                        }
                    } else if let Some(c) = entity(&r.xml10_content()) {
                        out.push(c);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            if out.len() > MAX_PART_BYTES as usize {
                return Err(ReaderError::TooLarge { size: out.len() as u64 });
            }
        }
        if self.section.is_none() {
            sections.push(out);
        }
        Ok(sections.into_iter().map(|s| clean_lines(&s)).collect())
    }
    fn text_of(&self, xml: &str) -> Result<String, ReaderError> {
        Ok(self.sections(xml)?.join("\n"))
    }
}

fn clean_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let mut out = String::new();
    let mut blank = 0;
    for l in lines {
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_owned()
}

const WORD: XmlText<'static> = XmlText {
    text: &["t"],
    para: &["p"],
    tab: &["tab"],
    brk: &["br", "cr"],
    skip: &["instrText", "delText", "rPr", "pPr"],
    section: None,
};
const DRAWING: XmlText<'static> = XmlText { text: &["t"], para: &["p"], tab: &[], brk: &["br"], skip: &[], section: None };
const ODF: XmlText<'static> = XmlText {
    text: &[],
    para: &["p", "h"],
    tab: &["tab"],
    brk: &["line-break"],
    skip: &["tracked-changes", "annotation", "forms", "scripts", "font-face-decls", "automatic-styles"],
    section: None,
};
const XHTML: XmlText<'static> = XmlText {
    text: &[],
    para: &["p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "li", "tr", "blockquote", "pre", "section"],
    tab: &["td", "th"],
    brk: &["br"],
    skip: &["head", "script", "style", "svg"],
    section: None,
};

fn docx(bytes: &[u8]) -> Result<String, ReaderError> {
    let mut z = Zip::open(bytes)?;
    WORD.text_of(&z.part("word/document.xml")?)
}

/// Relationship ID -> target, from a `.rels` part.
fn relationships(xml: &str) -> Result<Vec<(String, String, String)>, ReaderError> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event().map_err(|e| ReaderError::Invalid(format!("rels: {e}")))? {
            Event::Empty(e) | Event::Start(e) if local(e.name().as_ref()) == "Relationship" => {
                let mut id = String::new();
                let mut target = String::new();
                let mut kind = String::new();
                for a in e.attributes().flatten() {
                    let v = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default();
                    match local(a.key.as_ref()) {
                        "Id" => id = v,
                        "Target" => target = v,
                        "Type" => kind = v,
                        _ => {}
                    }
                }
                out.push((id, target, kind));
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Resolve a relationship target against the folder of the part it is in.
fn resolve_part(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_owned();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Values of one attribute on every element with this local name, in order.
fn attribute_values(xml: &str, element: &str, attribute: &str) -> Result<Vec<String>, ReaderError> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event().map_err(|e| ReaderError::Invalid(format!("xml: {e}")))? {
            Event::Empty(e) | Event::Start(e) if local(e.name().as_ref()) == element => {
                for a in e.attributes().flatten() {
                    if a.key.as_ref() == attribute
                        || (attribute.starts_with(':') && a.key.as_ref().contains(':') && local(a.key.as_ref()) == &attribute[1..])
                    {
                        out.push(a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default());
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn pptx(bytes: &[u8], notes: bool) -> Result<Vec<Section>, ReaderError> {
    let mut z = Zip::open(bytes)?;
    let presentation = z.part("ppt/presentation.xml")?;
    let rels = relationships(&z.part("ppt/_rels/presentation.xml.rels")?)?;
    // Slide order is the order of sldId entries, not the file numbering.
    let ids = attribute_values(&presentation, "sldId", ":id")?;
    let mut sections = Vec::new();
    for (n, rid) in ids.iter().enumerate() {
        let Some((_, target, _)) = rels.iter().find(|(id, _, _)| id == rid) else { continue };
        let slide = resolve_part("ppt", target);
        let mut part = slide.clone();
        if notes {
            let (dir, file) = slide.rsplit_once('/').unwrap_or(("", &slide));
            let rels_path = format!("{dir}/_rels/{file}.rels");
            part = String::new();
            if z.has(&rels_path) {
                if let Some((_, t, _)) = relationships(&z.part(&rels_path)?)?.into_iter().find(|(_, _, k)| k.ends_with("/notesSlide")) {
                    part = resolve_part(dir, &t);
                }
            }
        }
        let text = if part.is_empty() { String::new() } else { DRAWING.text_of(&z.part(&part)?)? };
        sections.push(Section { label: format!("slide {}", n + 1), text });
    }
    Ok(sections)
}

fn odt(bytes: &[u8]) -> Result<String, ReaderError> {
    let mut z = Zip::open(bytes)?;
    ODF.text_of(&z.part("content.xml")?)
}

fn odp(bytes: &[u8]) -> Result<Vec<Section>, ReaderError> {
    let mut z = Zip::open(bytes)?;
    let rules = XmlText { section: Some("page"), skip: &["notes", "automatic-styles", "font-face-decls", "scripts", "forms"], ..ODF };
    Ok(rules
        .sections(&z.part("content.xml")?)?
        .into_iter()
        .enumerate()
        .map(|(i, text)| Section { label: format!("slide {}", i + 1), text })
        .collect())
}

fn epub(bytes: &[u8]) -> Result<Vec<Section>, ReaderError> {
    let mut z = Zip::open(bytes)?;
    let container = z.part("META-INF/container.xml")?;
    let opf_path = attribute_values(&container, "rootfile", "full-path")?
        .into_iter()
        .next()
        .ok_or_else(|| ReaderError::Invalid("epub: no rootfile".into()))?;
    let opf = z.part(&opf_path)?;
    let base = opf_path.rsplit_once('/').map_or("", |(d, _)| d);
    let ids = attribute_values(&opf, "item", "id")?;
    let hrefs = attribute_values(&opf, "item", "href")?;
    let manifest: HashMap<String, String> = ids.into_iter().zip(hrefs).collect();
    let mut sections = Vec::new();
    for idref in attribute_values(&opf, "itemref", "idref")? {
        let Some(href) = manifest.get(&idref) else { continue };
        let path = resolve_part(base, href.split('#').next().unwrap_or(href));
        let text = XHTML.text_of(&z.part(&path)?)?;
        if !text.trim().is_empty() {
            sections.push(Section { label: format!("chapter {}", sections.len() + 1), text });
        }
    }
    if sections.is_empty() {
        let known = z.count();
        return Err(ReaderError::Invalid(format!("epub: no readable chapters among {known} parts")));
    }
    Ok(sections)
}

// ---------------------------------------------------------------- spreadsheets

fn spreadsheet(bytes: &[u8]) -> Result<Vec<Section>, ReaderError> {
    use calamine::{open_workbook_auto_from_rs, Data, Reader as _};
    let mut book = catch_unwind(AssertUnwindSafe(|| open_workbook_auto_from_rs(Cursor::new(bytes.to_vec()))))
        .map_err(|_| ReaderError::Invalid("the spreadsheet parser failed on this file".into()))?
        .map_err(|e| {
            let why = e.to_string();
            if why.to_ascii_lowercase().contains("password") || why.to_ascii_lowercase().contains("encrypt") {
                ReaderError::Unsupported("the workbook is password-protected".into())
            } else {
                ReaderError::Invalid(why)
            }
        })?;
    let mut sections = Vec::new();
    for name in book.sheet_names() {
        let range = book.worksheet_range(&name).map_err(|e| ReaderError::Invalid(e.to_string()))?;
        let mut text = String::new();
        let (row0, col0) = range.start().unwrap_or((0, 0));
        if (row0, col0) != (0, 0) {
            text.push_str(&format!("(starts at row {}, column {})\n", row0 + 1, col0 + 1));
        }
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|c| match c {
                    Data::Empty => String::new(),
                    Data::Error(e) => format!("#{e:?}"),
                    other => other.to_string().replace(['\t', '\n', '\r'], " "),
                })
                .collect();
            text.push_str(cells.join("\t").trim_end());
            text.push('\n');
            if text.len() > MAX_PART_BYTES as usize {
                return Err(ReaderError::TooLarge { size: text.len() as u64 });
            }
        }
        sections.push(Section { label: format!("sheet {name}"), text });
    }
    Ok(sections)
}

// ---------------------------------------------------------------- RTF

/// Windows-1252 bytes 0x80..0x9F; the rest of the high half is Latin-1.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“',
        '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    if (0x80..0xA0).contains(&b) { HIGH[(b - 0x80) as usize] } else { b as char }
}

fn rtf(bytes: &[u8]) -> Result<String, ReaderError> {
    // Destinations that hold no document text.
    const SKIP: [&str; 14] = [
        "fonttbl", "colortbl", "stylesheet", "info", "pict", "object", "header", "footer", "listtable",
        "listoverridetable", "rsidtbl", "generator", "xmlnstbl", "themedata",
    ];
    struct Group {
        skip: bool,
        uc: usize,
    }
    let mut out = String::new();
    let mut stack = vec![Group { skip: false, uc: 1 }];
    let mut pending_skip = 0usize; // fallback chars to drop after \uN
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'{' => {
                let parent = stack.last().is_some_and(|g| g.skip);
                let uc = stack.last().map_or(1, |g| g.uc);
                stack.push(Group { skip: parent, uc });
                if stack.len() > 512 {
                    return Err(ReaderError::Invalid("rtf: groups nested too deep".into()));
                }
                i += 1;
            }
            b'}' => {
                stack.pop();
                if stack.is_empty() {
                    break;
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                let Some(&c) = bytes.get(i) else { break };
                if c == b'\'' {
                    let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
                    i += 3;
                    if let Some(v) = hex {
                        if pending_skip > 0 {
                            pending_skip -= 1;
                        } else if !stack.last().is_some_and(|g| g.skip) {
                            out.push(cp1252(v));
                        }
                    }
                    continue;
                }
                if c == b'*' {
                    if let Some(g) = stack.last_mut() {
                        g.skip = true;
                    }
                    i += 1;
                    continue;
                }
                if !c.is_ascii_alphabetic() {
                    // Control symbol: \\ \{ \} \~ \- \_ and line breaks.
                    let skip = stack.last().is_some_and(|g| g.skip);
                    if !skip {
                        match c {
                            b'\\' | b'{' | b'}' => out.push(c as char),
                            b'~' => out.push('\u{a0}'),
                            b'_' => out.push('-'),
                            b'\n' | b'\r' => out.push('\n'),
                            _ => {}
                        }
                    }
                    i += 1;
                    continue;
                }
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
                let num_start = i;
                if i < bytes.len() && (bytes[i] == b'-' || bytes[i].is_ascii_digit()) {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let num: Option<i32> = std::str::from_utf8(&bytes[num_start..i]).ok().and_then(|n| n.parse().ok());
                if bytes.get(i) == Some(&b' ') {
                    i += 1;
                }
                let g = stack.last_mut().expect("stack not empty");
                if SKIP.contains(&word) {
                    g.skip = true;
                    continue;
                }
                if g.skip {
                    continue;
                }
                match word {
                    "par" | "line" | "sect" | "page" | "row" => out.push('\n'),
                    "tab" | "cell" => out.push('\t'),
                    "emdash" => out.push('—'),
                    "endash" => out.push('–'),
                    "lquote" => out.push('‘'),
                    "rquote" => out.push('’'),
                    "ldblquote" => out.push('“'),
                    "rdblquote" => out.push('”'),
                    "bullet" => out.push('•'),
                    "uc" => g.uc = num.unwrap_or(1).max(0) as usize,
                    "u" => {
                        let n = num.unwrap_or(0);
                        let code = if n < 0 { (n + 65536) as u32 } else { n as u32 };
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                        pending_skip = g.uc;
                    }
                    _ => {}
                }
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                if pending_skip > 0 {
                    pending_skip -= 1;
                } else if !stack.last().is_some_and(|g| g.skip) {
                    out.push(if b < 0x80 { b as char } else { cp1252(b) });
                }
                i += 1;
            }
        }
    }
    Ok(clean_lines(&out))
}
