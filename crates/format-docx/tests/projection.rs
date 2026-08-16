use std::io::{Cursor, Write};

use atelier_diff_core::{Confidence, FormatPackage, LineKind, diff_lines};
use atelier_format_docx::DocxPackage;
use zip::write::SimpleFileOptions;

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

/// A body in the shape Word writes: namespace soup, rsid noise, split and
/// formatted runs, proofing marks, bookmarks, and a section footprint.
fn word_document(body_sentence: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" mc:Ignorable="w14"><w:body><w:p w14:paraId="0A1B2C3D"><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="0" w:name="_Toc1"/><w:bookmarkEnd w:id="0"/><w:r><w:t>Quarterly report</w:t></w:r></w:p><w:p/><w:p w:rsidR="00AB12CD"><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">The quick brown fox </w:t></w:r><w:proofErr w:type="spellStart"/><w:r><w:t>{body_sentence} &amp; friends.</w:t></w:r><w:proofErr w:type="spellEnd"/></w:p><w:p><w:pPr><w:pStyle w:val="ListParagraph"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>First point</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="ListParagraph"/><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Nested point</w:t></w:r></w:p><w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/></w:tblPr><w:tr><w:tc><w:tcPr/><w:p><w:r><w:t>Region</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Total</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>North</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>120</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:body></w:document>"#
    )
}

const EXPECTED: &str = "# Quarterly report\n\n\n\n**The quick brown fox** jumps over the lazy dog & friends.\n\n- First point\n\n  - Nested point\n\n| Region | Total |\n| --- | --- |\n| North | 120 |\n";

/// The numbering the fixture's `numId="1"` references: bullets at the
/// levels the body uses.
const NUMBERING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;

fn docx(document_xml: &str) -> Vec<u8> {
    docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", document_xml.as_bytes()),
        ("word/numbering.xml", NUMBERING.as_bytes()),
    ])
}

fn docx_with(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, content) in parts {
        writer
            .start_file(*name, options)
            .expect("start fixture part");
        writer.write_all(content).expect("write fixture part");
    }
    writer
        .finish()
        .expect("finish fixture archive")
        .into_inner()
}

#[test]
fn body_projects_to_golden_markdown() {
    let projection = DocxPackage
        .project(&docx(&word_document("jumps over the lazy dog")))
        .unwrap();

    assert_eq!(projection.text, EXPECTED);
    assert_eq!(projection.package.to_string(), "format-docx@0.3.0");
}

#[test]
fn same_bytes_project_to_byte_identical_markdown() {
    let bytes = docx(&word_document("jumps over the lazy dog"));

    let first = DocxPackage.project(&bytes).unwrap();
    let second = DocxPackage.project(&bytes).unwrap();

    assert_eq!(first, second);
}

#[test]
fn an_edited_sentence_shows_at_the_text_rung() {
    let before = DocxPackage
        .project(&docx(&word_document("jumps over the lazy dog")))
        .unwrap();
    let after = DocxPackage
        .project(&docx(&word_document("leaps over the lazy dog")))
        .unwrap();

    let lines = diff_lines(&before.text, &after.text);

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].kind, LineKind::Removed);
    assert_eq!(
        lines[0].text,
        "**The quick brown fox** jumps over the lazy dog & friends."
    );
    assert_eq!(lines[1].kind, LineKind::Added);
    assert_eq!(
        lines[1].text,
        "**The quick brown fox** leaps over the lazy dog & friends."
    );
}

#[test]
fn any_prefix_bound_to_the_wordprocessingml_namespace_projects() {
    let renamed = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<x:document xmlns:x="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><x:body><x:p><x:r><x:t>Same namespace, other prefix</x:t></x:r></x:p></x:body></x:document>"#;

    let projection = DocxPackage.project(&docx(renamed)).unwrap();

    assert_eq!(projection.text, "Same namespace, other prefix\n");
}

#[test]
fn a_zip_without_the_document_part_is_an_error() {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file("[Content_Types].xml", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(CONTENT_TYPES.as_bytes()).unwrap();
    let bytes = writer.finish().unwrap().into_inner();

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert_eq!(
        error.to_string(),
        "format-docx@0.3.0: word/document.xml is missing"
    );
}

#[test]
fn bytes_that_are_not_a_zip_are_an_error() {
    let error = DocxPackage
        .project(b"plain text, not an archive")
        .unwrap_err();

    assert!(
        error.to_string().contains("not a zip archive"),
        "unexpected error: {error}"
    );
}

#[test]
fn truncated_document_xml_is_an_error() {
    let error = DocxPackage
        .project(&docx("<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p>"))
        .unwrap_err();

    assert!(
        error.to_string().contains("document ended mid-element"),
        "unexpected error: {error}"
    );
}

#[test]
fn detection_wants_the_docx_extension_and_zip_magic() {
    let bytes = docx(&word_document("jumps"));

    assert_eq!(
        DocxPackage.detect("report.docx", &bytes),
        Some(Confidence::Content)
    );
    assert_eq!(
        DocxPackage.detect("Deep/Path/REPORT.DOCX", &bytes),
        Some(Confidence::Content)
    );
    assert_eq!(DocxPackage.detect("report.txt", &bytes), None);
    // A .docx without zip magic is still claimed — by extension — so its
    // projection failure is journaled instead of the ladder misreading it.
    assert_eq!(
        DocxPackage.detect("report.docx", b"not a zip"),
        Some(Confidence::Extension)
    );
}

#[test]
fn a_second_root_element_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>first</w:t></w:r></w:p></w:body></w:document><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("content after the document root"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_undeclared_attribute_prefix_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:outlineLvl bad:val="0"/></w:pPr><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("undeclared namespace prefix"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_truncated_styles_part_is_an_error() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="FancyTitle"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert!(
        error.to_string().contains("styles ended mid-element"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_list_applied_through_a_style_keeps_its_marker() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="ListNumbered"><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr></w:style></w:styles>"#;
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="ListNumbered"/></w:pPr><w:r><w:t>styled item</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="ListNumbered"/><w:numPr><w:numId w:val="0"/></w:numPr></w:pPr><w:r><w:t>numbering removed</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "1. styled item\n\nnumbering removed\n");
}

#[test]
fn an_unmarked_numbering_level_renders_without_an_invented_marker() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="none"/></w:lvl></w:abstractNum><w:num w:numId="5"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="5"/></w:numPr></w:pPr><w:r><w:t>markerless item</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "markerless item\n");
}

#[test]
fn a_numbering_level_associated_to_a_style_names_its_level() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="NestedBullet"><w:pPr><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr></w:style></w:styles>"#;
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/><w:pStyle w:val="NestedBullet"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="NestedBullet"/></w:pPr><w:r><w:t>nested style item</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "   - nested style item\n");
}

#[test]
fn a_cr_break_projects_as_a_line_break() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>before</w:t><w:cr/><w:t>after</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "before\nafter\n");
}

#[test]
fn plain_text_that_looks_like_markdown_is_escaped() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>1. Scope</w:t></w:r></w:p><w:p><w:r><w:t># Not a heading</w:t></w:r></w:p><w:p><w:r><w:t>- not a bullet</w:t></w:r></w:p><w:p><w:r><w:t>1.5 is a number</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(
        projection.text,
        "1\\. Scope\n\n\\# Not a heading\n\n\\- not a bullet\n\n1.5 is a number\n"
    );
}

#[test]
fn a_real_list_and_text_that_says_the_same_thing_project_differently() {
    let plain = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>1. Scope</w:t></w:r></w:p></w:body></w:document>"#;
    let listed = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>Scope</w:t></w:r></w:p></w:body></w:document>"#;
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let listed_bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", listed.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let plain_projection = DocxPackage.project(&docx(plain)).unwrap();
    let listed_projection = DocxPackage.project(&listed_bytes).unwrap();

    assert_eq!(plain_projection.text, "1\\. Scope\n");
    assert_eq!(listed_projection.text, "1. Scope\n");
    assert_ne!(plain_projection.text, listed_projection.text);
}

#[test]
fn an_undeclared_attribute_prefix_on_any_element_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body bad:marker="1"><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("undeclared namespace prefix"),
        "unexpected error: {error}"
    );
}

#[test]
fn character_data_after_the_root_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>trailing garbage"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("character data outside the document root"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_self_closing_styles_root_is_valid() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        (
            "word/styles.xml",
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#,
        ),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "text\n");
}

#[test]
fn literal_backslashes_keep_escaping_injective() {
    // A paragraph that literally says "1\. Scope" must never project the
    // same as one saying "1. Scope" (whose marker the projection escapes).
    let with_backslash = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>1\. Scope</w:t></w:r></w:p></w:body></w:document>"#;
    let without = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>1. Scope</w:t></w:r></w:p></w:body></w:document>"#;

    let first = DocxPackage.project(&docx(with_backslash)).unwrap();
    let second = DocxPackage.project(&docx(without)).unwrap();

    assert_eq!(first.text, "1\\\\. Scope\n");
    assert_eq!(second.text, "1\\. Scope\n");
    assert_ne!(first.text, second.text);
}

#[test]
fn alternate_content_projects_only_the_fallback_branch() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><w:body><mc:AlternateContent><mc:Choice Requires="w14"><w:p><w:r><w:t>choice text</w:t></w:r></w:p></mc:Choice><mc:Fallback><w:p><w:r><w:t>fallback text</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "fallback text\n");
}

#[test]
fn word_encoded_hyphens_project_as_characters() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>co</w:t><w:noBreakHyphen/><w:t>operate</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "co\u{2011}operate\n");
}

#[test]
fn style_level_associations_stay_scoped_to_their_abstract_definition() {
    // The style's own numId selects abstract 0, which associates the style
    // with level 1 (bullet). A decoy abstract 1 associates the same style
    // with level 0 — it must not apply, its definition is not selected.
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="NestedBullet"><w:pPr><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr></w:style></w:styles>"#;
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/><w:pStyle w:val="NestedBullet"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:pStyle w:val="NestedBullet"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="8"><w:abstractNumId w:val="1"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="NestedBullet"/></w:pPr><w:r><w:t>nested style item</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "   - nested style item\n");
}

#[test]
fn choice_branches_in_the_styles_part_do_not_leak_properties() {
    // The unsupported Choice branch carries outlineLvl 0; the Fallback
    // carries none. The style must not become a heading.
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><w:style w:type="paragraph" w:styleId="Plain"><mc:AlternateContent><mc:Choice Requires="w14"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></mc:Choice><mc:Fallback><w:pPr/></mc:Fallback></mc:AlternateContent></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Plain"/></w:pPr><w:r><w:t>plain fallback</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "plain fallback\n");
}

#[test]
fn non_xml_whitespace_after_the_root_is_an_error() {
    let body = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>\u{a0}";

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("character data outside the document root"),
        "unexpected error: {error}"
    );
}

#[test]
fn hard_breaks_in_table_cells_stay_distinct_from_spaces() {
    let with_break = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>two</w:t><w:br/><w:t>lines</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let with_space = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>two lines</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;

    let broken = DocxPackage.project(&docx(with_break)).unwrap();
    let spaced = DocxPackage.project(&docx(with_space)).unwrap();

    assert_eq!(broken.text, "| two\\nlines |\n| --- |\n");
    assert_eq!(spaced.text, "| two lines |\n| --- |\n");
    assert_ne!(broken.text, spaced.text);
}

#[test]
fn symbol_runs_project_their_character() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>check </w:t><w:sym w:font="Wingdings" w:char="F0FC"/></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "check \u{f0fc}\n");
}

#[test]
fn a_based_on_cycle_in_styles_is_an_error() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="A"><w:basedOn w:val="B"/></w:style><w:style w:type="paragraph" w:styleId="B"><w:basedOn w:val="A"/></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert!(
        error.to_string().contains("basedOn cycle"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_numbered_heading_keeps_its_marker_and_stays_distinct() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let numbered_heading = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>Scope</w:t></w:r></w:p></w:body></w:document>"#;
    let literal_heading = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>1. Scope</w:t></w:r></w:p></w:body></w:document>"#;
    let numbered_bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", numbered_heading.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let numbered = DocxPackage.project(&numbered_bytes).unwrap();
    let literal = DocxPackage.project(&docx(literal_heading)).unwrap();

    assert_eq!(numbered.text, "# 1. Scope\n");
    assert_eq!(literal.text, "# 1\\. Scope\n");
    assert_ne!(numbered.text, literal.text);
}

#[test]
fn a_multiline_list_item_stays_distinct_from_two_items() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let one_item = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>A</w:t><w:br/><w:br/><w:t>1. B</w:t></w:r></w:p></w:body></w:document>"#;
    let two_items = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>A</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>1. B</w:t></w:r></w:p></w:body></w:document>"#;
    let one = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", one_item.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);
    let two = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", two_items.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let first = DocxPackage.project(&one).unwrap();
    let second = DocxPackage.project(&two).unwrap();

    assert_eq!(first.text, "1. A\n\\\n1\\. B\n");
    assert_eq!(second.text, "1. A\n\n2. 1\\. B\n");
    assert_ne!(first.text, second.text);
}

#[test]
fn instance_level_overrides_replace_the_abstract_definition() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:lvlOverride></w:num><w:num w:numId="8"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>overridden</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="8"/></w:numPr></w:pPr><w:r><w:t>abstract</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "- overridden\n\n1. abstract\n");
}

#[test]
fn ordered_lists_carry_their_start_value() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="5"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>starts at five</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "5. starts at five\n");
}

#[test]
fn cdata_line_endings_normalize_like_xml_text() {
    let crlf = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t><![CDATA[a\r\nb]]></w:t></w:r></w:p></w:body></w:document>";
    let lf = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t><![CDATA[a\nb]]></w:t></w:r></w:p></w:body></w:document>";

    let first = DocxPackage.project(&docx(crlf)).unwrap();
    let second = DocxPackage.project(&docx(lf)).unwrap();

    assert_eq!(first.text, "a\nb\n");
    assert_eq!(first, second);
}

#[test]
fn forbidden_xml_characters_are_an_error() {
    let body = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t><![CDATA[a\u{1}b]]></w:t></w:r></w:p></w:body></w:document>";

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_tab_after_a_marker_lookalike_still_escapes() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>1.</w:t><w:tab/><w:t>Scope</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "1\\.\tScope\n");
}

#[test]
fn children_indent_to_their_parents_marker_column() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="10"/><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>parent</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>child</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    // "10. " occupies four columns; the child must indent to all of them
    // or CommonMark breaks the hierarchy.
    assert_eq!(projection.text, "10. parent\n\n    - child\n");
}

#[test]
fn forbidden_characters_in_attribute_values_are_an_error() {
    let body = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body data=\"a\u{1}b\"><w:p><w:r><w:t>body</w:t></w:r></w:p></w:body></w:document>";

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn forbidden_characters_in_auxiliary_parts_are_an_error() {
    let styles = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:style w:type=\"paragraph\" w:styleId=\"X\"><![CDATA[a\u{1}b]]></w:style></w:styles>";
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_cell_deleted_under_track_changes_leaves_the_accepted_table() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>kept</w:t></w:r></w:p></w:tc><w:tc><w:tcPr><w:cellDel w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:tcPr><w:p><w:r><w:t>going away</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "| kept |\n| --- |\n");
}

#[test]
fn cell_paragraph_boundaries_stay_distinct_from_spaces() {
    let two_paragraphs = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let one_paragraph = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>a b</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;

    let split = DocxPackage.project(&docx(two_paragraphs)).unwrap();
    let joined = DocxPackage.project(&docx(one_paragraph)).unwrap();

    assert_eq!(split.text, "| a\\pb |\n| --- |\n");
    assert_eq!(joined.text, "| a b |\n| --- |\n");
    assert_ne!(split.text, joined.text);
}

#[test]
fn consecutive_hard_breaks_stay_distinct_from_a_paragraph_boundary() {
    let one_paragraph = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>A</w:t><w:br/><w:br/><w:t>B</w:t></w:r></w:p></w:body></w:document>"#;
    let two_paragraphs = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>A</w:t></w:r></w:p><w:p><w:r><w:t>B</w:t></w:r></w:p></w:body></w:document>"#;

    let joined = DocxPackage.project(&docx(one_paragraph)).unwrap();
    let split = DocxPackage.project(&docx(two_paragraphs)).unwrap();

    assert_eq!(joined.text, "A\n\\\nB\n");
    assert_eq!(split.text, "A\n\nB\n");
    assert_ne!(joined.text, split.text);
}

#[test]
fn forbidden_characters_behind_attribute_references_are_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body data="a&#1;b"><w:p><w:r><w:t>body</w:t></w:r></w:p></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn cell_paragraphs_keep_their_structure() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let bulleted = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>item</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let plain = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>item</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let bulleted_bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", bulleted.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let marked = DocxPackage.project(&bulleted_bytes).unwrap();
    let unmarked = DocxPackage.project(&docx(plain)).unwrap();

    assert_eq!(marked.text, "| - item |\n| --- |\n");
    assert_eq!(unmarked.text, "| item |\n| --- |\n");
    assert_ne!(marked.text, unmarked.text);
}

#[test]
fn forbidden_references_inside_choice_branches_are_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><w:body><mc:AlternateContent><mc:Choice Requires="w14"><w:p><w:r><w:t>&#1;</w:t></w:r></w:p></mc:Choice><mc:Fallback><w:p><w:r><w:t>fallback</w:t></w:r></w:p></mc:Fallback></mc:AlternateContent></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn parts_resolve_through_package_relationships() {
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="content/main.xml"/></Relationships>"#;
    let document_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="my-styles.xml"/></Relationships>"#;
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="FancyTitle"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="FancyTitle"/></w:pPr><w:r><w:t>relocated heading</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("content/_rels/main.xml.rels", document_rels.as_bytes()),
        ("content/main.xml", body.as_bytes()),
        ("content/my-styles.xml", styles.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "# relocated heading\n");
}

#[test]
fn a_decoy_relationship_type_cannot_spoof_the_document() {
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://evil.example/relationships/officeDocument" Target="decoy.xml"/><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let decoy = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>decoy</w:t></w:r></w:p></w:body></w:document>"#;
    let real = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>real document</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("decoy.xml", decoy.as_bytes()),
        ("word/document.xml", real.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "real document\n");
}

#[test]
fn package_relationships_without_an_office_document_are_an_error() {
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://evil.example/relationships/other" Target="whatever.xml"/></Relationships>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>unreferenced</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("word/document.xml", body.as_bytes()),
    ]);

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("names no officeDocument relationship"),
        "unexpected error: {error}"
    );
}

#[test]
fn orphan_canonical_parts_do_not_apply_when_relationships_exist() {
    // The document's rels name no styles relationship, so the canonical
    // word/styles.xml — an orphan in the OPC graph — must not resolve.
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let document_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="FancyTitle"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="FancyTitle"/></w:pPr><w:r><w:t>not a heading</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("word/_rels/document.xml.rels", document_rels.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "not a heading\n");
}

#[test]
fn a_dangling_styles_relationship_is_an_error() {
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let document_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="missing-styles.xml"/></Relationships>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("word/_rels/document.xml.rels", document_rels.as_bytes()),
        ("word/document.xml", body.as_bytes()),
    ]);

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert!(
        error.to_string().contains("targets a missing part"),
        "unexpected error: {error}"
    );
}

#[test]
fn forbidden_characters_in_relationship_parts_are_an_error() {
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml" Note="&#1;"/></Relationships>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("word/document.xml", body.as_bytes()),
    ]);

    let error = DocxPackage.project(&bytes).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn inserting_an_empty_paragraph_is_a_visible_edit() {
    let without = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>A</w:t></w:r></w:p><w:p><w:r><w:t>B</w:t></w:r></w:p></w:body></w:document>"#;
    let with = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>A</w:t></w:r></w:p><w:p/><w:p><w:r><w:t>B</w:t></w:r></w:p></w:body></w:document>"#;

    let plain = DocxPackage.project(&docx(without)).unwrap();
    let spaced = DocxPackage.project(&docx(with)).unwrap();

    assert_eq!(plain.text, "A\n\nB\n");
    assert_eq!(spaced.text, "A\n\n\n\nB\n");
    assert_ne!(plain.text, spaced.text);
}

#[test]
fn fence_markers_in_plain_text_are_escaped() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>```rust</w:t></w:r></w:p><w:p><w:r><w:t>~~~</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "\\```rust\n\n\\~\\~\\~\n");
}

#[test]
fn ordered_items_advance_within_their_instance_and_restart_across_instances() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="8"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let one_list = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>Alpha</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>Beta</w:t></w:r></w:p></w:body></w:document>"#;
    let two_lists = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>Alpha</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="8"/></w:numPr></w:pPr><w:r><w:t>Beta</w:t></w:r></w:p></w:body></w:document>"#;
    let continued = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", one_list.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);
    let restarted = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", two_lists.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let same = DocxPackage.project(&continued).unwrap();
    let split = DocxPackage.project(&restarted).unwrap();

    assert_eq!(same.text, "1. Alpha\n\n2. Beta\n");
    assert_eq!(split.text, "1. Alpha\n\n1. Beta\n");
    assert_ne!(same.text, split.text);
}

#[test]
fn start_override_prevails_over_a_nested_level_start() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="7"/><w:lvl w:ilvl="0"><w:start w:val="2"/><w:numFmt w:val="decimal"/></w:lvl></w:lvlOverride></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>restart</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "7. restart\n");
}

#[test]
fn an_omitted_start_numbers_from_zero_per_the_schema() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>zero based</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "0. zero based\n");
}

#[test]
fn qualified_relationship_attributes_cannot_spoof_the_graph() {
    // w:Type/w:Target lookalikes on the first relationship must not
    // count; the second, properly unqualified relationship wins.
    let package_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><Relationship Id="rId9" w:Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" w:Target="decoy.xml" Type="http://evil.example/relationships/other" Target="other.xml"/><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let decoy = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>decoy selected</w:t></w:r></w:p></w:body></w:document>"#;
    let real = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>real document</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("_rels/.rels", package_rels.as_bytes()),
        ("decoy.xml", decoy.as_bytes()),
        ("word/document.xml", real.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "real document\n");
}

#[test]
fn indentation_never_leaks_across_numbering_instances() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="10"/><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="8"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>parent</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="8"/></w:numPr></w:pPr><w:r><w:t>orphan</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    // The orphan belongs to numId 8; numId 7's "10. " column must not
    // apply, so it takes the three-space fallback.
    assert_eq!(projection.text, "10. parent\n\n   - orphan\n");
}

#[test]
fn forbidden_characters_in_comments_are_an_error() {
    let body = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><!--bad\u{1}comment--><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document>";

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("outside the XML character set"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_bullet_parent_restarts_ordered_children() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>bullet A</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>child</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>bullet B</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>child again</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(
        projection.text,
        "- bullet A\n\n  1. child\n\n- bullet B\n\n  1. child again\n"
    );
}

#[test]
fn deleted_rows_do_not_advance_list_ordinals() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>first</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:trPr><w:del w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:trPr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>gone</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>second</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "| 1. first |\n| --- |\n| 2. second |\n");
}

#[test]
fn a_custom_heading_ten_style_is_not_a_built_in_heading() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading10"/></w:pPr><w:r><w:t>Plain custom style</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "Plain custom style\n");
}

#[test]
fn a_declaration_after_the_root_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>text</w:t></w:r></w:p></w:body></w:document><?xml version="1.0"?>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("declaration outside the"),
        "unexpected error: {error}"
    );
}

#[test]
fn content_inside_a_deleted_row_never_advances_ordinals_even_nested() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>first</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:trPr><w:del w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:trPr><w:tc><w:tbl><w:tr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>nested gone</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:r><w:t>filler</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>second</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "| 1. first |\n| --- |\n| 2. second |\n");
}

#[test]
fn a_heading_zero_one_style_is_not_a_built_in_heading() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading01"/></w:pPr><w:r><w:t>custom</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "custom\n");
}

#[test]
fn a_deleted_paragraph_mark_merges_into_the_next_paragraph() {
    let tracked = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:rPr><w:del w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:rPr></w:pPr><w:r><w:t xml:space="preserve">Hello </w:t></w:r></w:p><w:p><w:r><w:t>world</w:t></w:r></w:p></w:body></w:document>"#;
    let untracked = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r></w:p><w:p><w:r><w:t>world</w:t></w:r></w:p></w:body></w:document>"#;

    let merged = DocxPackage.project(&docx(tracked)).unwrap();
    let split = DocxPackage.project(&docx(untracked)).unwrap();

    assert_eq!(merged.text, "Hello world\n");
    assert_eq!(split.text, "Hello \n\nworld\n");
    assert_ne!(merged.text, split.text);
}

#[test]
fn the_default_paragraph_style_applies_to_unstyled_paragraphs() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:default="1" w:styleId="TitleByDefault"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>implicit heading</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "# implicit heading\n");
}

#[test]
fn lvl_restart_zero_keeps_child_counters_across_parent_advances() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlRestart w:val="0"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>bullet A</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>child</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>bullet B</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>child again</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(
        projection.text,
        "- bullet A\n\n  1. child\n\n- bullet B\n\n  2. child again\n"
    );
}

#[test]
fn a_deleted_mark_inside_a_deleted_row_never_leaks_text() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:trPr><w:del w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:trPr><w:tc><w:p><w:pPr><w:rPr><w:del w:id="2" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:rPr></w:pPr><w:r><w:t>DELETED</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>kept</w:t></w:r></w:p></w:tc></w:tr></w:tbl><w:p><w:r><w:t>after</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "| kept |\n| --- |\n\nafter\n");
}

#[test]
fn a_default_on_style_applies_to_unstyled_paragraphs() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:default="on" w:styleId="TitleByDefault"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>implicit heading</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "# implicit heading\n");
}

#[test]
fn extension_namespace_text_boxes_stay_out_of_the_body_flow() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wne="http://schemas.microsoft.com/office/word/2006/wordml"><w:body><w:p><w:r><w:t xml:space="preserve">body </w:t></w:r><w:r><wne:txbxContent><w:p><w:r><w:t>boxed away</w:t></w:r></w:p></wne:txbxContent></w:r><w:r><w:t>continues</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "body continues\n");
}

#[test]
fn text_box_content_stays_out_of_the_body_flow() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>body text </w:t></w:r><w:r><w:pict><w:txbxContent><w:p><w:r><w:t>floating box text</w:t></w:r></w:p></w:txbxContent></w:pict></w:r><w:r><w:t>continues</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "body text continues\n");
}

#[test]
fn an_overinflating_document_part_is_an_error() {
    let mut oversized = String::with_capacity(65 * 1024 * 1024);
    oversized.push_str(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>"#,
    );
    for _ in 0..(64 * 1024 * 1024 / 16) {
        oversized.push_str("aaaaaaaaaaaaaaaa");
    }
    oversized.push_str("</w:t></w:r></w:p></w:body></w:document>");

    let error = DocxPackage.project(&docx(&oversized)).unwrap_err();

    assert!(
        error.to_string().contains("inflates past"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_out_of_range_list_level_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="9999999999999999"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>item</w:t></w:r></w:p></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("list level"),
        "unexpected error: {error}"
    );
}

#[test]
fn truncation_between_closed_elements_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>complete paragraph</w:t></w:r></w:p>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("document ended mid-element"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_undeclared_namespace_prefix_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><oops:t>text</oops:t></w:r></w:p></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("undeclared namespace prefix"),
        "unexpected error: {error}"
    );
}

#[test]
fn cdata_text_projects_like_plain_text() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t><![CDATA[wrapped <in> cdata]]></w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "wrapped <in> cdata\n");
}

#[test]
fn a_utf16_document_part_projects() {
    let body = "<?xml version=\"1.0\" encoding=\"UTF-16\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>utf-16 text</w:t></w:r></w:p></w:body></w:document>";
    let mut encoded = vec![0xff, 0xfe];
    for unit in body.encode_utf16() {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", &encoded),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "utf-16 text\n");
}

#[test]
fn historical_properties_and_deleted_content_stay_out_of_the_accepted_body() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pPrChange w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"><w:pPr><w:pStyle w:val="Heading1"/></w:pPr></w:pPrChange></w:pPr><w:r><w:t>accepted plain paragraph</w:t></w:r></w:p><w:p><w:del w:id="2" w:author="a" w:date="2026-01-01T00:00:00Z"><w:r><w:delText>pending deletion</w:delText></w:r></w:del><w:ins w:id="3" w:author="a" w:date="2026-01-01T00:00:00Z"><w:r><w:t>pending insertion</w:t></w:r></w:ins></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(
        projection.text,
        "accepted plain paragraph\n\npending insertion\n"
    );
}

#[test]
fn a_row_deleted_under_track_changes_leaves_the_accepted_table() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:tbl><w:tr><w:tc><w:p><w:r><w:t>kept</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:trPr><w:del w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"/></w:trPr><w:tc><w:p><w:r><w:t>going away</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "| kept |\n| --- |\n");
}

#[test]
fn a_direct_outline_level_renders_as_a_heading() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:outlineLvl w:val="1"/></w:pPr><w:r><w:t>direct outline heading</w:t></w:r></w:p><w:p><w:pPr><w:outlineLvl w:val="9"/></w:pPr><w:r><w:t>body text outline</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(
        projection.text,
        "## direct outline heading\n\nbody text outline\n"
    );
}

#[test]
fn a_custom_style_with_an_outline_level_renders_as_a_heading() {
    let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="FancyTitle"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="FancySub"><w:basedOn w:val="FancyTitle"/></w:style></w:styles>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="FancyTitle"/></w:pPr><w:r><w:t>styled heading</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="FancySub"/></w:pPr><w:r><w:t>inherited heading</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/styles.xml", styles.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(projection.text, "# styled heading\n\n# inherited heading\n");
}

#[test]
fn numbering_definitions_decide_ordered_against_bullet_lists() {
    let numbering = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#;
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>first ordered</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>nested bullet</w:t></w:r></w:p><w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="99"/></w:numPr></w:pPr><w:r><w:t>undefined numbering</w:t></w:r></w:p></w:body></w:document>"#;
    let bytes = docx_with(&[
        ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
        ("_rels/.rels", RELS.as_bytes()),
        ("word/document.xml", body.as_bytes()),
        ("word/numbering.xml", numbering.as_bytes()),
    ]);

    let projection = DocxPackage.project(&bytes).unwrap();

    assert_eq!(
        projection.text,
        "1. first ordered\n\n   - nested bullet\n\nundefined numbering\n"
    );
}

#[test]
fn bold_italic_and_strike_runs_project_markdown_emphasis() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r></w:p><w:p><w:r><w:rPr><w:i/></w:rPr><w:t>italic</w:t></w:r></w:p><w:p><w:r><w:rPr><w:strike/></w:rPr><w:t>struck</w:t></w:r></w:p><w:p><w:r><w:rPr><w:strike/><w:b/><w:i/></w:rPr><w:t>all three</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(
        projection.text,
        "**bold**\n\n*italic*\n\n~~struck~~\n\n~~***all three***~~\n"
    );
}

#[test]
fn a_bold_only_edit_shows_at_the_text_rung() {
    let plain = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>a critical clause</w:t></w:r></w:p></w:body></w:document>"#;
    let bolded = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>a critical clause</w:t></w:r></w:p></w:body></w:document>"#;

    let before = DocxPackage.project(&docx(plain)).unwrap();
    let after = DocxPackage.project(&docx(bolded)).unwrap();

    let lines = diff_lines(&before.text, &after.text);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].kind, LineKind::Removed);
    assert_eq!(lines[0].text, "a critical clause");
    assert_eq!(lines[1].kind, LineKind::Added);
    assert_eq!(lines[1].text, "**a critical clause**");
}

#[test]
fn literal_markers_and_real_emphasis_project_differently() {
    // The injectivity pin: a document that literally says **x** and one
    // whose x is actually bold must never print the same projection.
    let literal = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>**x**</w:t></w:r></w:p></w:body></w:document>"#;
    let bolded = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>"#;

    let first = DocxPackage.project(&docx(literal)).unwrap();
    let second = DocxPackage.project(&docx(bolded)).unwrap();

    assert_eq!(first.text, "\\*\\*x\\*\\*\n");
    assert_eq!(second.text, "**x**\n");
    assert_ne!(first.text, second.text);
}

#[test]
fn literal_tildes_and_real_strikethrough_project_differently() {
    let literal = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>~~x~~</w:t></w:r></w:p></w:body></w:document>"#;
    let struck = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:strike/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>"#;

    let first = DocxPackage.project(&docx(literal)).unwrap();
    let second = DocxPackage.project(&docx(struck)).unwrap();

    assert_eq!(first.text, "\\~\\~x\\~\\~\n");
    assert_eq!(second.text, "~~x~~\n");
    assert_ne!(first.text, second.text);
}

#[test]
fn literal_underscores_escape_inline() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>snake_case_name and _emphasis_</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "snake\\_case\\_name and \\_emphasis\\_\n");
}

#[test]
fn split_runs_with_equal_emphasis_project_as_one_span() {
    // Word splits visually identical text into arbitrary runs (proofing
    // state does); the projection must not depend on where the splits
    // fell, or equal documents would diff.
    let split = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bo</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>ld</w:t></w:r></w:p></w:body></w:document>"#;
    let whole = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r></w:p></w:body></w:document>"#;

    let first = DocxPackage.project(&docx(split)).unwrap();
    let second = DocxPackage.project(&docx(whole)).unwrap();

    assert_eq!(first.text, "**bold**\n");
    assert_eq!(first.text, second.text);
}

#[test]
fn emphasis_changes_mid_paragraph_close_the_span() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t xml:space="preserve">plain </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t xml:space="preserve"> then </w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>italic</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "plain **bold** then *italic*\n");
}

#[test]
fn emphasis_edge_whitespace_renders_outside_the_markers() {
    // Markdown emphasis cannot flank whitespace; the bold run's trailing
    // space renders after the closing marker.
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">bold </w:t></w:r><w:r><w:t>plain</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "**bold** plain\n");
}

#[test]
fn emphasis_on_whitespace_alone_projects_plain_whitespace() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>a</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve"> </w:t></w:r><w:r><w:t>b</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "a b\n");
}

#[test]
fn an_off_toggle_disables_inherited_emphasis() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b w:val="0"/></w:rPr><w:t>off zero</w:t></w:r><w:r><w:rPr><w:b w:val="false"/></w:rPr><w:t xml:space="preserve"> off false</w:t></w:r><w:r><w:rPr><w:b w:val="true"/></w:rPr><w:t xml:space="preserve"> on</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "off zero off false **on**\n");
}

#[test]
fn an_on_off_value_outside_the_schema_is_an_error() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b w:val="maybe"/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>"#;

    let error = DocxPackage.project(&docx(body)).unwrap_err();

    assert!(
        error.to_string().contains("outside ST_OnOff"),
        "unexpected error: {error}"
    );
}

#[test]
fn paragraph_mark_formatting_never_emphasizes_text() {
    // w:pPr/w:rPr formats the paragraph mark, not any run: its bold must
    // not leak into the paragraph's text.
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:rPr><w:b/></w:rPr></w:pPr><w:r><w:t>plain text</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "plain text\n");
}

#[test]
fn emphasis_markers_never_cross_a_hard_break() {
    let body = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>first</w:t><w:br/><w:t>second</w:t></w:r></w:p></w:body></w:document>"#;

    let projection = DocxPackage.project(&docx(body)).unwrap();

    assert_eq!(projection.text, "**first**\n**second**\n");
}
