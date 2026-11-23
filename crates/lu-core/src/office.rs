//! Create Word, PowerPoint and Excel files, and check what was created.
//!
//! These are helper utilities: a model with a proven record makes such files
//! with the basic tools; a model that needs the help gets one structured
//! call instead of a page of generated code. The owner's Qwen test produced
//! a zero-byte spreadsheet before rebuilding it by hand. Here a file is
//! written only after its bytes pass the check, and checked again on disk:
//!
//! 1. the spec is validated (sheet names, sizes, text Office can store);
//! 2. the package is built and inspected: it unzips, every part the
//!    content types promise exists, every XML part is well formed, and
//!    reading it back gives the expected paragraphs, slides or sheets;
//! 3. it goes through the staged write (origin gate, snapshot, read-back
//!    hash, atomic swap);
//! 4. the file on disk is inspected again.
//!
//! Written for Lu-Core; the old repo's `native_docx_tool.rs`,
//! `native_pptx_tool.rs` and `native_spreadsheet_create.rs` were not ported.
//! Office's acceptance of the files can only be confirmed on a machine with
//! Office (owner test `scripts/owner-tests/office-files.ps1`).

use std::{
    fmt::Write as _,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use quick_xml::{events::Event, Reader};
use serde::Deserialize;
use zip::write::SimpleFileOptions;

use crate::files::{Call, Content, FileAccess, FileError, WriteMode, WriteReceipt, WriteRequest};
use crate::reader::{self, Format};

pub const MAX_BLOCKS: usize = 10_000;
pub const MAX_SLIDES: usize = 500;
pub const MAX_CELLS: usize = 1_000_000;
/// Excel's own limit for one cell's text.
pub const MAX_TEXT_CHARS: usize = 32_767;

// ---------------------------------------------------------------- specs
// JSON from the model is parsed into these at the boundary.

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    Heading { level: u8, text: String },
    Paragraph { text: String },
    Bullet { text: String },
    Table { rows: Vec<Vec<String>> },
    PageBreak,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentSpec {
    #[serde(default)]
    pub title: Option<String>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slide {
    pub title: String,
    #[serde(default)]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationSpec {
    pub slides: Vec<Slide>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Cell {
    Number(f64),
    Bool(bool),
    Text(String),
    Formula { formula: String },
    Empty(()),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkbookSpec {
    pub sheets: Vec<Sheet>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OfficeSpec {
    Document(DocumentSpec),
    Presentation(PresentationSpec),
    Workbook(WorkbookSpec),
}
impl OfficeSpec {
    fn format(&self) -> Format {
        match self {
            Self::Document(_) => Format::Docx,
            Self::Presentation(_) => Format::Pptx,
            Self::Workbook(_) => Format::Xlsx,
        }
    }
    fn extension(&self) -> &'static str {
        match self {
            Self::Document(_) => "docx",
            Self::Presentation(_) => "pptx",
            Self::Workbook(_) => "xlsx",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficeError {
    /// The spec cannot become a valid file; nothing was written.
    Invalid(String),
    /// The built file failed its check; nothing was written (or, for the
    /// on-disk check, the write is reported and the reason given).
    CheckFailed(String),
    File(FileError),
}
impl From<FileError> for OfficeError {
    fn from(e: FileError) -> Self {
        Self::File(e)
    }
}
impl std::fmt::Display for OfficeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "invalid: {why}"),
            Self::CheckFailed(why) => write!(f, "check failed: {why}"),
            Self::File(e) => write!(f, "{e:?}"),
        }
    }
}
impl std::error::Error for OfficeError {}

/// One utility call from the model: where to write, whether to replace an
/// existing file, and what to put in it.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateCall {
    pub path: PathBuf,
    pub replace: bool,
    pub spec: OfficeSpec,
}

/// Parse the model's JSON arguments for one of the three utilities. Unknown
/// fields are refused, so a misspelt field is an error, not silently lost.
pub fn parse_call(utility_id: &str, json: &str) -> Result<CreateCall, OfficeError> {
    let bad = |e: serde_json::Error| OfficeError::Invalid(format!("arguments: {e}"));
    let mut args: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json).map_err(bad)?;
    let path = match args.remove("path") {
        Some(serde_json::Value::String(p)) if !p.trim().is_empty() => PathBuf::from(p),
        _ => return Err(OfficeError::Invalid("arguments: \"path\" (a string) is required".into())),
    };
    let replace = match args.remove("replace") {
        None => false,
        Some(serde_json::Value::Bool(b)) => b,
        Some(_) => return Err(OfficeError::Invalid("arguments: \"replace\" must be true or false".into())),
    };
    let rest = serde_json::Value::Object(args);
    let spec = match utility_id {
        "lu_create_word_document" => OfficeSpec::Document(serde_json::from_value(rest).map_err(bad)?),
        "lu_create_presentation" => OfficeSpec::Presentation(serde_json::from_value(rest).map_err(bad)?),
        "lu_create_spreadsheet" => OfficeSpec::Workbook(serde_json::from_value(rest).map_err(bad)?),
        other => return Err(OfficeError::Invalid(format!("unknown utility {other}"))),
    };
    Ok(CreateCall { path, replace, spec })
}

// ---------------------------------------------------------------- validation

fn check_text(what: &str, s: &str) -> Result<(), OfficeError> {
    if s.chars().count() > MAX_TEXT_CHARS {
        return Err(OfficeError::Invalid(format!("{what} is longer than {MAX_TEXT_CHARS} characters")));
    }
    if let Some(c) = s.chars().find(|c| (c.is_control() && !matches!(c, '\t' | '\n' | '\r')) || matches!(c, '\u{FFFE}' | '\u{FFFF}')) {
        return Err(OfficeError::Invalid(format!("{what} contains character U+{:04X}, which Office cannot store", c as u32)));
    }
    Ok(())
}

fn validate(spec: &OfficeSpec) -> Result<(), OfficeError> {
    match spec {
        OfficeSpec::Document(d) => {
            if d.blocks.is_empty() {
                return Err(OfficeError::Invalid("a document needs at least one block".into()));
            }
            if d.blocks.len() > MAX_BLOCKS {
                return Err(OfficeError::Invalid(format!("more than {MAX_BLOCKS} blocks")));
            }
            if let Some(t) = &d.title {
                check_text("title", t)?;
            }
            for (i, b) in d.blocks.iter().enumerate() {
                let at = format!("block {}", i + 1);
                match b {
                    Block::Heading { level, text } => {
                        if !(1..=3).contains(level) {
                            return Err(OfficeError::Invalid(format!("{at}: heading level must be 1, 2 or 3")));
                        }
                        check_text(&at, text)?;
                    }
                    Block::Paragraph { text } | Block::Bullet { text } => check_text(&at, text)?,
                    Block::Table { rows } => {
                        let width = rows.first().map_or(0, Vec::len);
                        if width == 0 || rows.iter().any(|r| r.len() != width) {
                            return Err(OfficeError::Invalid(format!("{at}: a table needs rows of the same, non-zero length")));
                        }
                        for cell in rows.iter().flatten() {
                            check_text(&at, cell)?;
                        }
                    }
                    Block::PageBreak => {}
                }
            }
        }
        OfficeSpec::Presentation(p) => {
            if p.slides.is_empty() || p.slides.len() > MAX_SLIDES {
                return Err(OfficeError::Invalid(format!("a presentation needs 1 to {MAX_SLIDES} slides")));
            }
            for (i, s) in p.slides.iter().enumerate() {
                let at = format!("slide {}", i + 1);
                check_text(&at, &s.title)?;
                for b in &s.bullets {
                    check_text(&at, b)?;
                }
                if let Some(n) = &s.notes {
                    check_text(&format!("{at} notes"), n)?;
                }
            }
        }
        OfficeSpec::Workbook(w) => {
            if w.sheets.is_empty() {
                return Err(OfficeError::Invalid("a workbook needs at least one sheet".into()));
            }
            let mut names: Vec<String> = Vec::new();
            let mut cells = 0usize;
            for s in &w.sheets {
                let n = s.name.trim();
                if n.is_empty() || n.chars().count() > 31 || n.contains(['[', ']', ':', '*', '?', '/', '\\']) || n.starts_with('\'') || n.ends_with('\'') {
                    return Err(OfficeError::Invalid(format!(
                        "sheet name {:?}: 1-31 characters, none of [ ] : * ? / \\, not starting or ending with '",
                        s.name
                    )));
                }
                if names.iter().any(|m| m.eq_ignore_ascii_case(n)) {
                    return Err(OfficeError::Invalid(format!("two sheets are named {n:?}")));
                }
                names.push(n.to_owned());
                if s.rows.len() > 1_048_576 || s.rows.iter().any(|r| r.len() > 16_384) {
                    return Err(OfficeError::Invalid(format!("sheet {n:?} is larger than Excel allows")));
                }
                for c in s.rows.iter().flatten() {
                    cells += 1;
                    match c {
                        Cell::Text(t) => check_text(&format!("sheet {n:?}"), t)?,
                        Cell::Formula { formula } => {
                            check_text(&format!("sheet {n:?} formula"), formula)?;
                            if formula.trim().is_empty() {
                                return Err(OfficeError::Invalid(format!("sheet {n:?}: empty formula")));
                            }
                        }
                        Cell::Number(x) if !x.is_finite() => {
                            return Err(OfficeError::Invalid(format!("sheet {n:?}: {x} is not a number Excel can store")))
                        }
                        _ => {}
                    }
                }
            }
            if cells > MAX_CELLS {
                return Err(OfficeError::Invalid(format!("more than {MAX_CELLS} cells")));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- building

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

fn package(parts: &[(String, String)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        w.start_file(name.as_str(), options).expect("zip entry");
        w.write_all(body.as_bytes()).expect("zip write to memory");
    }
    w.finish().expect("zip finish in memory").into_inner()
}

const XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const OFFICE_DOC: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn rels(items: &[(String, &str, String)]) -> String {
    let mut s = format!(r#"{XML}<Relationships xmlns="{PKG_RELS}">"#);
    for (id, kind, target) in items {
        let _ = write!(s, r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{}"/>"#, esc(target));
    }
    s.push_str("</Relationships>");
    s
}
fn root_rels(target: &str) -> String {
    format!(r#"{XML}<Relationships xmlns="{PKG_RELS}"><Relationship Id="rId1" Type="{OFFICE_DOC}" Target="{target}"/></Relationships>"#)
}
fn content_types(overrides: &[(String, &str)]) -> String {
    let mut s = format!(
        r#"{XML}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>"#
    );
    for (part, ct) in overrides {
        let _ = write!(s, r#"<Override PartName="/{part}" ContentType="{ct}"/>"#);
    }
    s.push_str("</Types>");
    s
}

// ---- Word

const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn w_runs(text: &str) -> String {
    let mut s = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            s.push_str("<w:br/>");
        }
        for (j, part) in line.split('\t').enumerate() {
            if j > 0 {
                s.push_str("<w:tab/>");
            }
            if !part.is_empty() {
                let _ = write!(s, r#"<w:t xml:space="preserve">{}</w:t>"#, esc(part));
            }
        }
    }
    format!("<w:r>{s}</w:r>")
}
fn w_para(style: Option<&str>, extra_ppr: &str, text: &str) -> String {
    let style = style.map(|s| format!(r#"<w:pStyle w:val="{s}"/>"#)).unwrap_or_default();
    format!("<w:p><w:pPr>{style}{extra_ppr}</w:pPr>{}</w:p>", w_runs(text))
}

fn build_docx(d: &DocumentSpec) -> Vec<u8> {
    let mut body = String::new();
    if let Some(t) = &d.title {
        body.push_str(&w_para(Some("Title"), "", t));
    }
    for b in &d.blocks {
        match b {
            Block::Heading { level, text } => body.push_str(&w_para(Some(&format!("Heading{level}")), "", text)),
            Block::Paragraph { text } => body.push_str(&w_para(None, "", text)),
            Block::Bullet { text } => body.push_str(&w_para(Some("ListParagraph"), r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr>"#, text)),
            Block::Table { rows } => {
                let cols = rows[0].len();
                body.push_str(r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="5000" w:type="pct"/></w:tblPr><w:tblGrid>"#);
                for _ in 0..cols {
                    let _ = write!(body, r#"<w:gridCol w:w="{}"/>"#, 9000 / cols.max(1));
                }
                body.push_str("</w:tblGrid>");
                for row in rows {
                    body.push_str("<w:tr>");
                    for cell in row {
                        let _ = write!(body, r#"<w:tc><w:tcPr><w:tcW w:w="0" w:type="auto"/></w:tcPr>{}</w:tc>"#, w_para(None, "", cell));
                    }
                    body.push_str("</w:tr>");
                }
                // Word needs a paragraph between consecutive tables.
                body.push_str("</w:tbl><w:p/>");
            }
            Block::PageBreak => body.push_str(r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#),
        }
    }
    let document = format!(
        r#"{XML}<w:document {W_NS}><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr></w:body></w:document>"#
    );
    let heading = |id: &str, name: &str, size: u32| {
        format!(r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{name}"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before="240" w:after="80"/></w:pPr><w:rPr><w:b/><w:sz w:val="{size}"/></w:rPr></w:style>"#)
    };
    let styles = format!(
        r#"{XML}<w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:eastAsia="Calibri" w:cs="Calibri"/><w:sz w:val="22"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="259" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:qFormat/><w:rPr><w:sz w:val="56"/></w:rPr></w:style>{}{}{}<w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:pPr><w:ind w:left="720"/></w:pPr></w:style><w:style w:type="table" w:default="1" w:styleId="TableNormal"><w:name w:val="Normal Table"/><w:tblPr><w:tblInd w:w="0" w:type="dxa"/><w:tblCellMar><w:top w:w="0" w:type="dxa"/><w:left w:w="108" w:type="dxa"/><w:bottom w:w="0" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style><w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:basedOn w:val="TableNormal"/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:tblPr><w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:left w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:right w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="auto"/></w:tblBorders></w:tblPr></w:style></w:styles>"#,
        heading("Heading1", "heading 1", 32),
        heading("Heading2", "heading 2", 28),
        heading("Heading3", "heading 3", 24),
    );
    let numbering = format!(
        r#"{XML}<w:numbering {W_NS}><w:abstractNum w:abstractNumId="0"><w:multiLevelType w:val="hybridMultilevel"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    );
    package(&[
        (
            "[Content_Types].xml".into(),
            content_types(&[
                ("word/document.xml".into(), "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"),
                ("word/styles.xml".into(), "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"),
                ("word/numbering.xml".into(), "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"),
            ]),
        ),
        ("_rels/.rels".into(), root_rels("word/document.xml")),
        (
            "word/_rels/document.xml.rels".into(),
            rels(&[("rId1".into(), "styles", "styles.xml".into()), ("rId2".into(), "numbering", "numbering.xml".into())]),
        ),
        ("word/document.xml".into(), document),
        ("word/styles.xml".into(), styles),
        ("word/numbering.xml".into(), numbering),
    ])
}

// ---- PowerPoint

const P_NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;
const SLIDE_W: u64 = 12_192_000;
const SLIDE_H: u64 = 6_858_000;

fn theme(name: &str) -> String {
    let fill3 = r#"<a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill>"#;
    let line = r#"<a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>"#;
    let effect = "<a:effectStyle><a:effectLst/></a:effectStyle>";
    let color = |n: &str, v: &str| format!(r#"<a:{n}><a:srgbClr val="{v}"/></a:{n}>"#);
    format!(
        r#"{XML}<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="{name}"><a:themeElements><a:clrScheme name="Lu"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>{}{}{}{}{}{}{}{}{}{}</a:clrScheme><a:fontScheme name="Lu"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Lu"><a:fillStyleLst>{fill3}</a:fillStyleLst><a:lnStyleLst>{line}{line}{line}</a:lnStyleLst><a:effectStyleLst>{effect}{effect}{effect}</a:effectStyleLst><a:bgFillStyleLst>{fill3}</a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#,
        color("dk2", "44546A"),
        color("lt2", "E7E6E6"),
        color("accent1", "4472C4"),
        color("accent2", "ED7D31"),
        color("accent3", "A5A5A5"),
        color("accent4", "FFC000"),
        color("accent5", "5B9BD5"),
        color("accent6", "70AD47"),
        color("hlink", "0563C1"),
        color("folHlink", "954F72"),
    )
}

const GROUP: &str = r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#;
const CLR_MAP: &str = r#"<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>"#;

/// A placeholder shape; `xfrm` only on master and layout.
fn placeholder(id: u32, name: &str, ph: &str, xfrm: Option<(u64, u64, u64, u64)>, paragraphs: &str) -> String {
    let sp_pr = xfrm.map_or("<p:spPr/>".to_owned(), |(x, y, cx, cy)| {
        format!(r#"<p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>"#)
    });
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{name}"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr>{sp_pr}<p:txBody><a:bodyPr/><a:lstStyle/>{paragraphs}</p:txBody></p:sp>"#
    )
}
fn a_para(text: &str, bullet: bool) -> String {
    let ppr = if bullet { r#"<a:pPr marL="342900" indent="-342900"><a:buFont typeface="Arial"/><a:buChar char="•"/></a:pPr>"# } else { "" };
    let runs: String = text
        .split('\n')
        .enumerate()
        .map(|(i, l)| format!("{}<a:r><a:rPr lang=\"en-US\" dirty=\"0\"/><a:t>{}</a:t></a:r>", if i > 0 { "<a:br><a:rPr lang=\"en-US\"/></a:br>" } else { "" }, esc(l)))
        .collect();
    format!("<a:p>{ppr}{runs}</a:p>")
}
const TITLE_BOX: (u64, u64, u64, u64) = (838_200, 365_125, 10_515_600, 1_325_563);
const BODY_BOX: (u64, u64, u64, u64) = (838_200, 1_825_625, 10_515_600, 4_351_338);
const PH_TITLE: &str = r#"<p:ph type="title"/>"#;
const PH_BODY: &str = r#"<p:ph idx="1"/>"#;

fn build_pptx(p: &PresentationSpec) -> Vec<u8> {
    let any_notes = p.slides.iter().any(|s| s.notes.as_deref().is_some_and(|n| !n.trim().is_empty()));
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut overrides: Vec<(String, &str)> = vec![
        ("ppt/presentation.xml".into(), "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"),
        ("ppt/slideMasters/slideMaster1.xml".into(), "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"),
        ("ppt/slideLayouts/slideLayout1.xml".into(), "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"),
        ("ppt/theme/theme1.xml".into(), "application/vnd.openxmlformats-officedocument.theme+xml"),
    ];
    let master_shapes = format!(
        "{}{}",
        placeholder(2, "Title Placeholder 1", PH_TITLE, Some(TITLE_BOX), &a_para("Title", false)),
        placeholder(3, "Text Placeholder 2", r#"<p:ph type="body" idx="1"/>"#, Some(BODY_BOX), &a_para("Text", true)),
    );
    parts.push((
        "ppt/slideMasters/slideMaster1.xml".into(),
        format!(
            r#"{XML}<p:sldMaster {P_NS}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree>{GROUP}{master_shapes}</p:spTree></p:cSld>{CLR_MAP}<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr sz="4400"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr marL="228600" indent="-228600"><a:buFont typeface="Arial"/><a:buChar char="•"/><a:defRPr sz="2800"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>"#
        ),
    ));
    parts.push((
        "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
        rels(&[("rId1".into(), "slideLayout", "../slideLayouts/slideLayout1.xml".into()), ("rId2".into(), "theme", "../theme/theme1.xml".into())]),
    ));
    let layout_shapes = format!(
        "{}{}",
        placeholder(2, "Title 1", PH_TITLE, None, "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>"),
        placeholder(3, "Content Placeholder 2", PH_BODY, None, "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>"),
    );
    parts.push((
        "ppt/slideLayouts/slideLayout1.xml".into(),
        format!(r#"{XML}<p:sldLayout {P_NS} type="obj" preserve="1"><p:cSld name="Title and Content"><p:spTree>{GROUP}{layout_shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#),
    ));
    parts.push(("ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(), rels(&[("rId1".into(), "slideMaster", "../slideMasters/slideMaster1.xml".into())])));
    parts.push(("ppt/theme/theme1.xml".into(), theme("Lu")));

    let mut pres_rels: Vec<(String, &str, String)> = vec![
        ("rId1".into(), "slideMaster", "slideMasters/slideMaster1.xml".into()),
        ("rId2".into(), "theme", "theme/theme1.xml".into()),
    ];
    let mut notes_master_list = String::new();
    if any_notes {
        pres_rels.push(("rId3".into(), "notesMaster", "notesMasters/notesMaster1.xml".into()));
        notes_master_list = r#"<p:notesMasterIdLst><p:notesMasterId r:id="rId3"/></p:notesMasterIdLst>"#.into();
        overrides.push(("ppt/notesMasters/notesMaster1.xml".into(), "application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"));
        overrides.push(("ppt/theme/theme2.xml".into(), "application/vnd.openxmlformats-officedocument.theme+xml"));
        let shapes = placeholder(2, "Notes Placeholder 1", r#"<p:ph type="body" idx="1"/>"#, Some((685_800, 4_400_550, 5_486_400, 3_600_450)), "<a:p><a:endParaRPr lang=\"en-US\"/></a:p>");
        parts.push((
            "ppt/notesMasters/notesMaster1.xml".into(),
            format!(r#"{XML}<p:notesMaster {P_NS}><p:cSld><p:spTree>{GROUP}{shapes}</p:spTree></p:cSld>{CLR_MAP}</p:notesMaster>"#),
        ));
        parts.push(("ppt/notesMasters/_rels/notesMaster1.xml.rels".into(), rels(&[("rId1".into(), "theme", "../theme/theme2.xml".into())])));
        parts.push(("ppt/theme/theme2.xml".into(), theme("Lu Notes")));
    }
    let mut slide_list = String::new();
    for (i, s) in p.slides.iter().enumerate() {
        let n = i + 1;
        let rid = format!("rId{}", 10 + i);
        pres_rels.push((rid.clone(), "slide", format!("slides/slide{n}.xml")));
        let _ = write!(slide_list, r#"<p:sldId id="{}" r:id="{rid}"/>"#, 256 + i);
        overrides.push((format!("ppt/slides/slide{n}.xml"), "application/vnd.openxmlformats-officedocument.presentationml.slide+xml"));
        let mut shapes = placeholder(2, "Title 1", PH_TITLE, None, &a_para(&s.title, false));
        if !s.bullets.is_empty() {
            let body: String = s.bullets.iter().map(|b| a_para(b, false)).collect();
            shapes.push_str(&placeholder(3, "Content Placeholder 2", PH_BODY, None, &body));
        }
        parts.push((
            format!("ppt/slides/slide{n}.xml"),
            format!(r#"{XML}<p:sld {P_NS}><p:cSld><p:spTree>{GROUP}{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#),
        ));
        let mut slide_rels = vec![("rId1".into(), "slideLayout", "../slideLayouts/slideLayout1.xml".into())];
        if let Some(notes) = s.notes.as_deref().filter(|n| !n.trim().is_empty()) {
            slide_rels.push(("rId2".into(), "notesSlide", format!("../notesSlides/notesSlide{n}.xml")));
            overrides.push((format!("ppt/notesSlides/notesSlide{n}.xml"), "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"));
            let body: String = notes.split('\n').map(|l| a_para(l, false)).collect();
            let shapes = placeholder(2, "Notes Placeholder 1", r#"<p:ph type="body" idx="1"/>"#, None, &body);
            parts.push((
                format!("ppt/notesSlides/notesSlide{n}.xml"),
                format!(r#"{XML}<p:notes {P_NS}><p:cSld><p:spTree>{GROUP}{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>"#),
            ));
            parts.push((
                format!("ppt/notesSlides/_rels/notesSlide{n}.xml.rels"),
                rels(&[("rId1".into(), "notesMaster", "../notesMasters/notesMaster1.xml".into()), ("rId2".into(), "slide", format!("../slides/slide{n}.xml"))]),
            ));
        }
        parts.push((format!("ppt/slides/_rels/slide{n}.xml.rels"), rels(&slide_rels)));
    }
    parts.push((
        "ppt/presentation.xml".into(),
        format!(
            r#"{XML}<p:presentation {P_NS} saveSubsetFonts="1"><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>{notes_master_list}<p:sldIdLst>{slide_list}</p:sldIdLst><p:sldSz cx="{SLIDE_W}" cy="{SLIDE_H}"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#
        ),
    ));
    parts.push(("ppt/_rels/presentation.xml.rels".into(), rels(&pres_rels)));
    parts.push(("_rels/.rels".into(), root_rels("ppt/presentation.xml")));
    parts.insert(0, ("[Content_Types].xml".into(), content_types(&overrides)));
    package(&parts)
}

// ---- Excel

const S_NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn column(mut n: usize) -> String {
    let mut s = Vec::new();
    n += 1;
    while n > 0 {
        n -= 1;
        s.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    s.reverse();
    String::from_utf8(s).expect("ascii")
}

fn build_xlsx(w: &WorkbookSpec) -> Vec<u8> {
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut overrides: Vec<(String, &str)> = vec![
        ("xl/workbook.xml".into(), "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"),
        ("xl/styles.xml".into(), "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"),
    ];
    let mut sheets = String::new();
    let mut book_rels: Vec<(String, &str, String)> = vec![("rId1".into(), "styles", "styles.xml".into())];
    for (i, s) in w.sheets.iter().enumerate() {
        let n = i + 1;
        let rid = format!("rId{}", n + 1);
        let _ = write!(sheets, r#"<sheet name="{}" sheetId="{n}" r:id="{rid}"/>"#, esc(s.name.trim()));
        book_rels.push((rid, "worksheet", format!("worksheets/sheet{n}.xml")));
        overrides.push((format!("xl/worksheets/sheet{n}.xml"), "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"));
        let mut data = String::new();
        for (r, row) in s.rows.iter().enumerate() {
            let _ = write!(data, r#"<row r="{}">"#, r + 1);
            for (c, cell) in row.iter().enumerate() {
                let at = format!("{}{}", column(c), r + 1);
                match cell {
                    Cell::Empty(()) => {}
                    Cell::Number(x) => {
                        let _ = write!(data, r#"<c r="{at}"><v>{x}</v></c>"#);
                    }
                    Cell::Bool(b) => {
                        let _ = write!(data, r#"<c r="{at}" t="b"><v>{}</v></c>"#, u8::from(*b));
                    }
                    Cell::Text(t) => {
                        let _ = write!(data, r#"<c r="{at}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#, esc(t));
                    }
                    Cell::Formula { formula } => {
                        let f = formula.trim().strip_prefix('=').unwrap_or(formula.trim());
                        let _ = write!(data, r#"<c r="{at}"><f>{}</f></c>"#, esc(f));
                    }
                }
            }
            data.push_str("</row>");
        }
        parts.push((format!("xl/worksheets/sheet{n}.xml"), format!(r#"{XML}<worksheet {S_NS}><sheetData>{data}</sheetData></worksheet>"#)));
    }
    parts.push((
        "xl/workbook.xml".into(),
        // fullCalcOnLoad: Excel computes formulas on open (Lu stores none).
        format!(r#"{XML}<workbook {S_NS}><sheets>{sheets}</sheets><calcPr calcId="191029" fullCalcOnLoad="1"/></workbook>"#),
    ));
    parts.push((
        "xl/styles.xml".into(),
        format!(
            r#"{XML}<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/><family val="2"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#
        ),
    ));
    parts.push(("xl/_rels/workbook.xml.rels".into(), rels(&book_rels)));
    parts.push(("_rels/.rels".into(), root_rels("xl/workbook.xml")));
    parts.insert(0, ("[Content_Types].xml".into(), content_types(&overrides)));
    package(&parts)
}

/// Build the file's bytes after validating the spec. Nothing is written.
pub fn build(spec: &OfficeSpec) -> Result<Vec<u8>, OfficeError> {
    validate(spec)?;
    Ok(match spec {
        OfficeSpec::Document(d) => build_docx(d),
        OfficeSpec::Presentation(p) => build_pptx(p),
        OfficeSpec::Workbook(w) => build_xlsx(w),
    })
}

// ---------------------------------------------------------------- the check

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    pub format: Format,
    pub size: u64,
    pub parts: usize,
    /// Pages, slides or sheets the reader found.
    pub sections: usize,
    pub chars: usize,
}

fn well_formed(name: &str, xml: &str) -> Result<(), String> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0i64;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => depth -= 1,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("{name} is not well-formed XML: {e}")),
        }
    }
    if depth != 0 {
        return Err(format!("{name} has unclosed elements"));
    }
    Ok(())
}

/// Every text a spec promises, to be found when the file is read back.
fn expected_texts(spec: &OfficeSpec) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |s: &str| {
        for line in s.split(['\n', '\t']) {
            if !line.trim().is_empty() {
                out.push(line.trim().to_owned());
            }
        }
    };
    match spec {
        OfficeSpec::Document(d) => {
            if let Some(t) = &d.title {
                push(t);
            }
            for b in &d.blocks {
                match b {
                    Block::Heading { text, .. } | Block::Paragraph { text } | Block::Bullet { text } => push(text),
                    Block::Table { rows } => rows.iter().flatten().for_each(|c| push(c)),
                    Block::PageBreak => {}
                }
            }
        }
        OfficeSpec::Presentation(p) => {
            for s in &p.slides {
                push(&s.title);
                s.bullets.iter().for_each(|b| push(b));
            }
        }
        OfficeSpec::Workbook(w) => {
            for s in &w.sheets {
                for c in s.rows.iter().flatten() {
                    if let Cell::Text(t) = c {
                        push(t);
                    }
                }
            }
        }
    }
    out
}

fn name_for(format: Format) -> String {
    format!("check.{}", format.as_str())
}

/// Check that bytes are a sound package of the format, and (with a spec)
/// that reading them back gives what the spec asked for.
pub fn inspect(format: Format, bytes: &[u8], spec: Option<&OfficeSpec>) -> Result<Inspection, OfficeError> {
    let fail = |s: String| OfficeError::CheckFailed(s);
    if bytes.is_empty() {
        return Err(fail("the file is empty (0 bytes)".into()));
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| fail(format!("not a zip package: {e}")))?;
    let mut xml_parts: Vec<(String, String)> = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| fail(format!("zip entry {i}: {e}")))?;
        let name = entry.name().map_err(|e| fail(format!("zip entry {i}: {e}")))?.into_owned();
        if name.ends_with(".xml") || name.ends_with(".rels") {
            let mut s = String::new();
            entry.by_ref().take(reader::MAX_PART_BYTES + 1).read_to_string(&mut s).map_err(|e| fail(format!("{name}: {e}")))?;
            well_formed(&name, &s).map_err(fail)?;
            xml_parts.push((name, s));
        }
    }
    let types = xml_parts
        .iter()
        .find(|(n, _)| n == "[Content_Types].xml")
        .ok_or_else(|| fail("missing [Content_Types].xml".into()))?;
    // Every part the content types promise must exist.
    let mut r = Reader::from_str(&types.1);
    loop {
        match r.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.local_name().as_ref() == "Override" => {
                for a in e.attributes().flatten() {
                    if a.key.as_ref() == "PartName" {
                        let v = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).unwrap_or_default();
                        let part = v.trim_start_matches('/');
                        if !xml_parts.iter().any(|(n, _)| n == part) {
                            return Err(fail(format!("content types list {part}, which is missing")));
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(fail(format!("[Content_Types].xml: {e}"))),
            _ => {}
        }
    }
    if !xml_parts.iter().any(|(n, _)| n == "_rels/.rels") {
        return Err(fail("missing _rels/.rels".into()));
    }
    let read = reader::extract(&name_for(format), bytes, None, usize::MAX).map_err(|e| fail(format!("reading it back: {e}")))?;
    if read.format != format {
        return Err(fail(format!("reads back as {}, not {}", read.format.as_str(), format.as_str())));
    }
    if let Some(spec) = spec {
        let want = match spec {
            OfficeSpec::Document(_) => 1,
            OfficeSpec::Presentation(p) => p.slides.len(),
            OfficeSpec::Workbook(w) => w.sheets.len(),
        };
        if read.total_sections != want {
            return Err(fail(format!("expected {want} sections, read back {}", read.total_sections)));
        }
        if let OfficeSpec::Workbook(w) = spec {
            for s in &w.sheets {
                let name = s.name.trim();
                // One section is printed without a header; ask for it by name.
                let found = if w.sheets.len() == 1 {
                    reader::extract(&name_for(format), bytes, Some(&format!("sheet:{name}")), 1).is_ok()
                } else {
                    read.text.contains(&format!("[sheet {name}]"))
                };
                if !found {
                    return Err(fail(format!("sheet {:?} missing when read back", s.name)));
                }
            }
        }
        let mut from = 0;
        for t in expected_texts(spec) {
            // In order, so a dropped or reordered block is caught.
            match read.text[from..].find(&t) {
                Some(at) => from += at + t.len(),
                None => return Err(fail(format!("{t:?} missing or out of order when read back"))),
            }
        }
    }
    Ok(Inspection { format, size: bytes.len() as u64, parts: zip.len(), sections: read.total_sections, chars: read.text.chars().count() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub receipt: WriteReceipt,
    pub inspection: Inspection,
}

/// Create the file: validate, build, check, staged write, check on disk.
/// The extension must match the kind (`.docx`, `.pptx`, `.xlsx`).
pub fn create(files: &FileAccess<'_>, call: &Call, path: &Path, spec: &OfficeSpec, replace: bool) -> Result<Created, OfficeError> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    if ext != spec.extension() {
        return Err(OfficeError::Invalid(format!("the file must end in .{} (got {:?})", spec.extension(), path.display().to_string())));
    }
    let bytes = build(spec)?;
    inspect(spec.format(), &bytes, Some(spec))?;
    let receipt = files.write(
        call,
        &WriteRequest {
            path: PathBuf::from(path),
            content: Content::Base64(STANDARD.encode(&bytes)),
            mode: if replace { WriteMode::CreateOrReplace } else { WriteMode::CreateNew },
            intended_lines: None,
        },
    )?;
    let on_disk = std::fs::read(&receipt.path).map_err(|e| OfficeError::CheckFailed(format!("reading the written file: {e}")))?;
    let inspection = inspect(spec.format(), &on_disk, Some(spec))?;
    Ok(Created { receipt, inspection })
}
