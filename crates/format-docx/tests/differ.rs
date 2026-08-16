//! The rich docx differ: what markdown cannot express arrives as scoped
//! delta lines — exact output pinned, distinct inputs pinned distinct.

use std::io::{Cursor, Write};

use atelier_diff_core::{DeltaKind, Fidelity, FormatPackage};
use atelier_format_docx::DocxPackage;
use zip::write::SimpleFileOptions;

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

fn docx(body: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, content) in [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", RELS),
        ("word/document.xml", document.as_str()),
    ] {
        writer
            .start_file(name, options)
            .expect("start fixture part");
        writer
            .write_all(content.as_bytes())
            .expect("write fixture part");
    }
    writer
        .finish()
        .expect("finish fixture archive")
        .into_inner()
}

/// One paragraph whose single run carries `rpr` (an `w:rPr` body).
fn paragraph(rpr: &str, text: &str) -> String {
    if rpr.is_empty() {
        return format!("<w:p><w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>");
    }
    format!("<w:p><w:r><w:rPr>{rpr}</w:rPr><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>")
}

fn rich(before_body: &str, after_body: &str) -> Vec<atelier_diff_core::Delta> {
    DocxPackage
        .diff(&docx(before_body), &docx(after_body))
        .expect("the docx package ships a differ")
        .expect("both fixtures are well formed")
}

#[test]
fn a_font_size_change_names_the_text_and_the_property() {
    let deltas = rich(
        &paragraph(r#"<w:sz w:val="22"/>"#, "a critical clause"),
        &paragraph(r#"<w:sz w:val="28"/>"#, "a critical clause"),
    );

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].address.as_str(), "paragraph 1");
    assert_eq!(deltas[0].kind, DeltaKind::Changed);
    assert_eq!(deltas[0].fidelity, Fidelity::Rich);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"a critical clause\" font size 11 → 14")
    );
    assert_eq!(
        deltas[0].package.map(|package| package.to_string()),
        Some("format-docx@0.3.0".to_owned())
    );
}

#[test]
fn the_same_pair_diffs_to_identical_deltas_every_time() {
    let before = paragraph(r#"<w:sz w:val="22"/>"#, "clause");
    let after = paragraph(r#"<w:sz w:val="28"/>"#, "clause");

    assert_eq!(rich(&before, &after), rich(&before, &after));
}

#[test]
fn different_property_changes_never_print_identical_deltas() {
    let before = paragraph(r#"<w:sz w:val="22"/>"#, "clause");
    let to_fourteen = rich(&before, &paragraph(r#"<w:sz w:val="28"/>"#, "clause"));
    let to_fifteen = rich(&before, &paragraph(r#"<w:sz w:val="30"/>"#, "clause"));

    assert_eq!(
        to_fourteen[0].summary.as_deref(),
        Some("\"clause\" font size 11 → 14")
    );
    assert_eq!(
        to_fifteen[0].summary.as_deref(),
        Some("\"clause\" font size 11 → 15")
    );
    assert_ne!(to_fourteen, to_fifteen);
}

#[test]
fn an_odd_half_point_size_reports_a_half() {
    let deltas = rich(
        &paragraph(r#"<w:sz w:val="22"/>"#, "clause"),
        &paragraph(r#"<w:sz w:val="23"/>"#, "clause"),
    );

    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"clause\" font size 11 → 11.5")
    );
}

#[test]
fn an_emphasis_only_change_is_the_text_rungs_story() {
    let deltas = rich(
        &paragraph("", "a critical clause"),
        &paragraph("<w:b/>", "a critical clause"),
    );

    assert_eq!(deltas, Vec::new());
}

#[test]
fn emphasis_reports_when_it_co_occurs_with_other_changes() {
    let deltas = rich(
        &paragraph(r#"<w:sz w:val="22"/>"#, "a critical clause"),
        &paragraph(r#"<w:b/><w:sz w:val="28"/>"#, "a critical clause"),
    );

    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"a critical clause\" gained bold, font size 11 → 14")
    );
}

#[test]
fn underline_family_and_color_changes_report_in_format_terms() {
    let deltas = rich(
        &paragraph(r#"<w:rFonts w:ascii="Calibri"/>"#, "clause"),
        &paragraph(
            r#"<w:rFonts w:ascii="Arial"/><w:u w:val="single"/><w:color w:val="FF0000"/>"#,
            "clause",
        ),
    );

    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"clause\" underline none → single, font Calibri → Arial, color default → FF0000")
    );
}

#[test]
fn only_the_changed_range_is_named() {
    let before = paragraph("", "keep this part loud");
    let after = "<w:p><w:r><w:t xml:space=\"preserve\">keep this </w:t></w:r>\
                 <w:r><w:rPr><w:u w:val=\"single\"/></w:rPr><w:t>part</w:t></w:r>\
                 <w:r><w:t xml:space=\"preserve\"> loud</w:t></w:r></w:p>"
        .to_owned();

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"part\" underline none → single")
    );
}

#[test]
fn a_text_changed_paragraph_yields_no_property_delta() {
    // Text changes are the text rung's story; the second paragraph's
    // formatting change still reports — combined edits report both.
    let before = format!(
        "{}{}",
        paragraph("", "the wording changes here"),
        paragraph(r#"<w:sz w:val="22"/>"#, "the size changes here"),
    );
    let after = format!(
        "{}{}",
        paragraph("", "the wording changed here"),
        paragraph(r#"<w:sz w:val="28"/>"#, "the size changes here"),
    );

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].address.as_str(), "paragraph 2");
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"the size changes here\" font size 11 → 14")
    );
}

#[test]
fn equal_documents_yield_no_deltas() {
    let body = paragraph(r#"<w:sz w:val="22"/>"#, "clause");

    assert_eq!(rich(&body, &body), Vec::new());
}

#[test]
fn a_size_outside_half_points_is_an_error() {
    let error = DocxPackage
        .diff(
            &docx(&paragraph(r#"<w:sz w:val="22"/>"#, "clause")),
            &docx(&paragraph(r#"<w:sz w:val="banana"/>"#, "clause")),
        )
        .expect("the docx package ships a differ")
        .expect_err("a non-numeric size is malformed");

    assert!(
        error.to_string().contains("not half-points"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_underline_without_a_style_is_an_error() {
    let error = DocxPackage
        .diff(
            &docx(&paragraph("", "clause")),
            &docx(&paragraph("<w:u/>", "clause")),
        )
        .expect("the docx package ships a differ")
        .expect_err("an underline naming no style is ambiguous");

    assert!(
        error.to_string().contains("no underline style"),
        "unexpected error: {error}"
    );
}

#[test]
fn two_ranges_changing_differently_yield_two_ordered_deltas() {
    let before = paragraph("", "keep this part loud");
    let after = "<w:p><w:r><w:t xml:space=\"preserve\">keep </w:t></w:r>\
                 <w:r><w:rPr><w:sz w:val=\"28\"/></w:rPr><w:t>this</w:t></w:r>\
                 <w:r><w:t xml:space=\"preserve\"> part </w:t></w:r>\
                 <w:r><w:rPr><w:u w:val=\"single\"/></w:rPr><w:t>loud</w:t></w:r></w:p>"
        .to_owned();

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 2);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"this\" font size default → 14")
    );
    assert_eq!(
        deltas[1].summary.as_deref(),
        Some("\"loud\" underline none → single")
    );
    assert_eq!(deltas[0].address, deltas[1].address);
}

#[test]
fn addresses_follow_the_after_side_across_an_insertion() {
    // A paragraph inserted above must not shift the blame: the address
    // names the paragraph where the reader will find it now.
    let before = format!(
        "{}{}",
        paragraph("", "alpha"),
        paragraph(r#"<w:sz w:val="22"/>"#, "resize this clause"),
    );
    let after = format!(
        "{}{}{}",
        paragraph("", "inserted between"),
        paragraph("", "alpha"),
        paragraph(r#"<w:sz w:val="28"/>"#, "resize this clause"),
    );

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].address.as_str(), "paragraph 3");
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"resize this clause\" font size 11 → 14")
    );
}

#[test]
fn historical_properties_in_rpr_change_never_reach_a_delta() {
    // The revision record carries the rejected size (5) beside the
    // accepted one (14); only the accepted properties compare.
    let before = paragraph(r#"<w:sz w:val="22"/>"#, "clause");
    let after = paragraph(
        r#"<w:rPrChange w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z"><w:rPr><w:sz w:val="10"/></w:rPr></w:rPrChange><w:sz w:val="28"/>"#,
        "clause",
    );

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"clause\" font size 11 → 14")
    );
}

#[test]
fn table_cell_paragraphs_count_in_document_order() {
    let table = |rpr: &str| {
        format!(
            "<w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl>",
            paragraph(rpr, "cell clause")
        )
    };
    let before = format!(
        "{}{}",
        paragraph("", "intro"),
        table(r#"<w:sz w:val="22"/>"#)
    );
    let after = format!(
        "{}{}",
        paragraph("", "intro"),
        table(r#"<w:sz w:val="28"/>"#)
    );

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].address.as_str(), "paragraph 2");
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"cell clause\" font size 11 → 14")
    );
}

#[test]
fn paragraph_mark_properties_never_yield_a_delta() {
    // w:pPr/w:rPr formats the paragraph mark, not any text; a size change
    // there is invisible to the run-property model.
    let mark = |size: &str| {
        format!(
            "<w:p><w:pPr><w:rPr><w:sz w:val=\"{size}\"/></w:rPr></w:pPr><w:r><w:t>steady text</w:t></w:r></w:p>"
        )
    };

    let deltas = rich(&mark("22"), &mark("28"));

    assert_eq!(deltas, Vec::new());
}

#[test]
fn formatting_an_empty_paragraph_yields_no_delta() {
    let empty = |rpr: &str| format!("<w:p><w:r><w:rPr>{rpr}</w:rPr></w:r></w:p>");

    let deltas = rich(&empty(""), &empty("<w:b/><w:u w:val=\"single\"/>"));

    assert_eq!(deltas, Vec::new());
}

#[test]
fn a_range_spanning_a_hard_break_reports_with_the_break_escaped() {
    let with_break = |rpr: &str| {
        format!(
            "<w:p><w:r><w:rPr>{rpr}</w:rPr><w:t>first</w:t><w:br/><w:t>second</w:t></w:r></w:p>"
        )
    };

    let deltas = rich(&with_break(""), &with_break(r#"<w:u w:val="single"/>"#));

    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"first\\nsecond\" underline none → single")
    );
}

#[test]
fn multibyte_ranges_name_exactly_the_changed_text() {
    let before = paragraph("", "um café bem quente");
    let after = "<w:p><w:r><w:t xml:space=\"preserve\">um </w:t></w:r>\
                 <w:r><w:rPr><w:sz w:val=\"28\"/></w:rPr><w:t>café</w:t></w:r>\
                 <w:r><w:t xml:space=\"preserve\"> bem quente</w:t></w:r></w:p>"
        .to_owned();

    let deltas = rich(&before, &after);

    assert_eq!(deltas.len(), 1);
    assert_eq!(
        deltas[0].summary.as_deref(),
        Some("\"café\" font size default → 14")
    );
}
