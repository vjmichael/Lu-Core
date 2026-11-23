use lu_core::reader::{classify, extract, Format, ReaderError, DEFAULT_MAX_CHARS};
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;

fn zipped(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in parts {
        w.start_file(*name, SimpleFileOptions::default()).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}
fn read(name: &str, bytes: &[u8]) -> String {
    extract(name, bytes, None, DEFAULT_MAX_CHARS).unwrap_or_else(|e| panic!("{name}: {e}")).text
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
fn docx() -> Vec<u8> {
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W}><w:body>
        <w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>Installer notes</w:t></w:r></w:p>
        <w:p><w:r><w:t xml:space="preserve">Use </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>makensis</w:t></w:r><w:r><w:tab/><w:t>3.10 &amp; up</w:t></w:r></w:p>
        <w:p><w:r><w:instrText>HYPERLINK "http://x"</w:instrText><w:t>Line one</w:t><w:br/><w:t>Line two</w:t></w:r></w:p>
        <w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Cell B</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
        <w:p><w:del><w:r><w:delText>removed words</w:delText></w:r></w:del><w:r><w:t>caf&#233;</w:t></w:r></w:p>
        </w:body></w:document>"#
    );
    zipped(&[("[Content_Types].xml", "<Types/>"), ("word/document.xml", &body)])
}

#[test]
fn word_text_keeps_paragraphs_tabs_and_breaks_and_drops_field_codes() {
    let text = read("notes.docx", &docx());
    assert!(text.starts_with("Installer notes\nUse makensis\t3.10 & up\n"), "{text}");
    assert!(text.contains("Line one\nLine two"));
    assert!(text.contains("Cell A\nCell B"));
    assert!(text.contains("café"));
    assert!(!text.contains("HYPERLINK") && !text.contains("removed words"), "{text}");
}

fn pptx() -> Vec<u8> {
    let a = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let slide = |t: &str| format!(r#"<p:sld {a}><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{t}</a:t></a:r></a:p><a:p><a:r><a:t>Second line</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#);
    // File numbering differs from the presentation order on purpose.
    let pres = format!(r#"<p:presentation {a}><p:sldIdLst><p:sldId id="256" r:id="rId7"/><p:sldId id="257" r:id="rId3"/></p:sldIdLst></p:presentation>"#);
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/></Relationships>"#;
    let slide_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#;
    let notes = format!(r#"<p:notes {a}><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Say the version out loud</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#);
    zipped(&[
        ("ppt/presentation.xml", &pres),
        ("ppt/_rels/presentation.xml.rels", rels),
        ("ppt/slides/slide1.xml", &slide("Second slide")),
        ("ppt/slides/slide2.xml", &slide("Opening slide")),
        ("ppt/slides/_rels/slide2.xml.rels", slide_rels),
        ("ppt/notesSlides/notesSlide1.xml", &notes),
    ])
}

#[test]
fn slides_follow_presentation_order_and_notes_are_selectable() {
    let deck = pptx();
    let all = extract("deck.pptx", &deck, None, DEFAULT_MAX_CHARS).unwrap();
    assert_eq!(all.total_sections, 2);
    let first = all.text.find("[slide 1]\nOpening slide\nSecond line").unwrap_or_else(|| panic!("{}", all.text));
    assert!(first < all.text.find("[slide 2]\nSecond slide").unwrap());
    let two = extract("deck.pptx", &deck, Some("slide:2"), DEFAULT_MAX_CHARS).unwrap();
    assert!(two.text.contains("Second slide") && !two.text.contains("Opening"));
    let notes = extract("deck.pptx", &deck, Some("notes:1"), DEFAULT_MAX_CHARS).unwrap();
    assert!(notes.text.contains("Say the version out loud"), "{}", notes.text);
    assert!(matches!(extract("deck.pptx", &deck, Some("slide:3"), 100), Err(ReaderError::NoSuchSection(_))));
    assert!(matches!(extract("deck.pptx", &deck, Some("page:1"), 100), Err(ReaderError::NoSuchSection(_))));
}

fn xlsx() -> Vec<u8> {
    let ns = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let sheet = |rows: &str| format!(r#"<?xml version="1.0" encoding="UTF-8"?><worksheet {ns}><sheetData>{rows}</sheetData></worksheet>"#);
    let s = |r: &str, t: &str| format!(r#"<c r="{r}" t="inlineStr"><is><t>{t}</t></is></c>"#);
    let budget = sheet(&format!(
        r#"<row r="1">{}{}</row><row r="2">{}<c r="B2"><v>1250.5</v></c></row><row r="3">{}<c r="B3"><v>42</v></c></row>"#,
        s("A1", "Item"), s("B1", "Cost"), s("A2", "GPU"), s("A3", "Cable")
    ));
    let notes = sheet(&format!(r#"<row r="1">{}</row>"#, s("A1", "Bought in November")));
    zipped(&[
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#),
        ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
        ("xl/workbook.xml", &format!(r#"<?xml version="1.0" encoding="UTF-8"?><workbook {ns}><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#)),
        ("xl/_rels/workbook.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#),
        ("xl/worksheets/sheet1.xml", &budget),
        ("xl/worksheets/sheet2.xml", &notes),
    ])
}

#[test]
fn spreadsheets_come_out_as_tab_separated_sheets() {
    let book = xlsx();
    let all = extract("budget.xlsx", &book, None, DEFAULT_MAX_CHARS).unwrap();
    assert_eq!(all.total_sections, 2);
    assert!(all.text.contains("[sheet Budget]\nItem\tCost\nGPU\t1250.5\nCable\t42"), "{}", all.text);
    let notes = extract("budget.xlsx", &book, Some("sheet:Notes"), DEFAULT_MAX_CHARS).unwrap();
    assert_eq!(notes.text, "[sheet Notes]\nBought in November\n");
}

#[test]
fn opendocument_text_and_presentations() {
    let office = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0""#;
    let odt = zipped(&[(
        "content.xml",
        &format!(r#"<office:document-content {office}><office:automatic-styles><text:p>style junk</text:p></office:automatic-styles><office:body><office:text>
        <text:h>Heading</text:h>
        <text:p>First<text:s/>para<text:tab/>tabbed<text:line-break/>broken</text:p>
        </office:text></office:body></office:document-content>"#),
    )]);
    let text = read("notes.odt", &odt);
    assert_eq!(text, "Heading\nFirst para\ttabbed\nbroken\n", "{text:?}");

    let odp = zipped(&[(
        "content.xml",
        &format!(r#"<office:document-content {office}><office:body><office:presentation>
        <draw:page draw:name="a"><draw:frame><draw:text-box><text:p>Slide one</text:p></draw:text-box></draw:frame><presentation:notes><text:p>hidden notes</text:p></presentation:notes></draw:page>
        <draw:page draw:name="b"><draw:frame><draw:text-box><text:p>Slide two</text:p></draw:text-box></draw:frame></draw:page>
        </office:presentation></office:body></office:document-content>"#),
    )]);
    let deck = extract("deck.odp", &odp, None, DEFAULT_MAX_CHARS).unwrap();
    assert_eq!(deck.total_sections, 2);
    assert_eq!(deck.text, "[slide 1]\nSlide one\n[slide 2]\nSlide two\n");
}

#[test]
fn epub_chapters_follow_the_spine() {
    let epub = zipped(&[
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#),
        ("OEBPS/book.opf", r#"<package xmlns="http://www.idpf.org/2007/opf"><manifest><item id="c1" href="text/one.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="text/two.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c2"/><itemref idref="c1"/></spine></package>"#),
        ("OEBPS/text/one.xhtml", r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title><style>p{}</style></head><body><h1>Chapter B</h1><p>Body of   <em>chapter</em> B.</p></body></html>"#),
        ("OEBPS/text/two.xhtml", r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>T</title></head><body><h1>Chapter A</h1><p>First&#8212;and only.</p><script>alert(1)</script></body></html>"#),
    ]);
    let book = extract("book.epub", &epub, None, DEFAULT_MAX_CHARS).unwrap();
    assert_eq!(book.text, "[chapter 1]\nChapter A\nFirst—and only.\n[chapter 2]\nChapter B\nBody of chapter B.\n", "{:?}", book.text);
}

#[test]
fn rtf_text_without_font_tables_or_pictures() {
    let rtf = br"{\rtf1\ansi\deff0{\fonttbl{\f0 Arial;}}{\colortbl;\red255\green0\blue0;}{\*\generator Riched20;}
\pard Hello \b world\b0 !\par
Caf\'e9 \u8364?5 and a \{brace\}\tab tabbed\par
{\pict\pngblip 89504e47}Done}";
    let text = read("letter.rtf", rtf);
    assert_eq!(text, "Hello world!\nCafé €5 and a {brace}\ttabbed\nDone\n", "{text:?}");
}

fn pdf(pages: &[&str], object_streams: bool) -> Vec<u8> {
    use lopdf::{content::{Content, Operation}, dictionary, Document, Object, Stream};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let resources = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font } });
    let mut kids = Vec::new();
    for text in pages {
        let ops = if text.is_empty() {
            vec![]
        } else {
            vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal(*text)]),
                Operation::new("ET", vec![]),
            ]
        };
        let content = doc.add_object(Stream::new(dictionary! {}, Content { operations: ops }.encode().unwrap()));
        kids.push(doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content }).into());
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count, "Resources" => resources, "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()] }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    doc.compress();
    let mut out = Vec::new();
    if object_streams {
        doc.save_modern(&mut out).unwrap();
    } else {
        doc.save_to(&mut out).unwrap();
    }
    out
}

#[test]
fn pdf_pages_including_compressed_and_object_stream_files() {
    for modern in [false, true] {
        let file = pdf(&["Quarterly report", "Second page text"], modern);
        let all = extract("report.pdf", &file, None, DEFAULT_MAX_CHARS).unwrap();
        assert_eq!(all.total_sections, 2);
        assert!(all.text.contains("[page 1]\nQuarterly report"), "{:?}", all.text);
        let two = extract("report.pdf", &file, Some("page:2"), DEFAULT_MAX_CHARS).unwrap();
        assert!(two.text.contains("Second page text") && !two.text.contains("Quarterly"));
    }
    // A scanned PDF (no text layer) says so instead of returning nothing.
    assert!(matches!(extract("scan.pdf", &pdf(&["", ""], false), None, 100), Err(ReaderError::Unsupported(why)) if why.contains("OCR")));
}

#[test]
fn bad_files_fail_cleanly_and_limits_hold() {
    // Damaged containers are errors, never panics.
    assert!(matches!(extract("x.pdf", b"%PDF-1.7\ngarbage", None, 100), Err(ReaderError::Invalid(_))));
    assert!(matches!(extract("x.docx", b"PK\x03\x04garbage", None, 100), Err(ReaderError::Invalid(_))));
    assert!(matches!(extract("x.xlsx", b"PK\x03\x04garbage", None, 100), Err(ReaderError::Invalid(_))));
    // Legacy .doc is recognized and refused with a way forward.
    let ole = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0];
    assert_eq!(classify("old.doc", &ole), Some(Format::LegacyOffice));
    assert!(matches!(extract("old.doc", &ole, None, 100), Err(ReaderError::Unsupported(_))));
    // The signature decides: a PDF named .txt is a PDF; a zip named .zip is not a document.
    assert_eq!(classify("a.txt", b"%PDF-1.4"), Some(Format::Pdf));
    assert_eq!(classify("a.zip", b"PK\x03\x04"), None);
    // Output is cut at max_chars.
    let cut = extract("notes.docx", &docx(), None, 10).unwrap();
    assert!(cut.truncated && cut.text.chars().count() == 10);
}
