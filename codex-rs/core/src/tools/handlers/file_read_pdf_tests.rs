use super::*;
use pretty_assertions::assert_eq;

#[test]
fn pdf_page_ranges_match_claude_code_rules() {
    assert_eq!(
        parse_pdf_page_range("3").expect("single page"),
        PdfPageRange { first: 3, last: 3 }
    );
    assert_eq!(
        parse_pdf_page_range(" 2-5 ").expect("page range"),
        PdfPageRange { first: 2, last: 5 }
    );
    for invalid in ["", "0", "4-2", "one", "1-5", "3-"] {
        assert!(parse_pdf_page_range(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn pdf_page_file_names_are_sorted_numerically() {
    assert_eq!(pdf_page_number(Path::new("page-12.jpg")), Some(12));
    assert_eq!(pdf_page_number(Path::new("page.jpg")), None);
}

#[test]
fn pdfinfo_page_count_is_parsed_without_matching_metadata_text() {
    assert_eq!(
        parse_pdf_page_count("Title: Pages: 99\nPages:          12\nEncrypted: no\n"),
        Some(12)
    );
    assert_eq!(parse_pdf_page_count("Pages: unknown\n"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_is_rendered_as_multimodal_page_output_when_poppler_is_available() {
    if std::process::Command::new("pdftoppm")
        .arg("-v")
        .output()
        .is_err()
    {
        return;
    }

    let output = render_pdf_pages(minimal_pdf(), "fixture.pdf", None)
        .await
        .expect("render PDF");

    assert_eq!(output.content.len(), 2);
    assert!(matches!(
        output.content[1],
        FunctionCallOutputContentItem::InputImage { .. }
    ));
}

fn minimal_pdf() -> Vec<u8> {
    let stream = "BT /F1 24 Tf 72 720 Td (Hello) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}
