use super::*;
use pretty_assertions::assert_eq;

#[test]
fn utf16le_round_trip_preserves_bom_and_content() {
    let original = "\u{feff}alpha\r\nbeta\r\n";
    let bytes = encode_file(original, FileEncoding::Utf16Le);

    assert_eq!(
        decode_text_file(bytes.clone(), "fixture.txt", "Edit")
            .expect("decode UTF-16LE")
            .content,
        original
    );
    assert_eq!(bytes[0..2], [0xff, 0xfe]);
}

#[test]
fn utf16le_encoding_restores_a_bom_after_replacing_an_empty_file() {
    let bytes = encode_file("content\n", FileEncoding::Utf16Le);

    assert_eq!(&bytes[..2], UTF16_LE_BOM);
    assert_eq!(
        decode_text_file(bytes, "fixture.txt", "Edit")
            .expect("decode UTF-16LE")
            .content,
        "\u{feff}content\n"
    );
}

#[test]
fn malformed_utf16le_is_rejected() {
    let error = decode_text_file(vec![0xff, 0xfe, 0x41], "fixture.txt", "Read")
        .err()
        .expect("odd UTF-16LE byte count should fail");

    assert!(error.to_string().contains("incomplete code unit"));
}
