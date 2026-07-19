use crate::function_tool::FunctionCallError;

const UTF16_LE_BOM: &[u8] = &[0xff, 0xfe];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileEncoding {
    Utf8,
    Utf16Le,
}

pub(super) struct DecodedFile {
    pub content: String,
    pub encoding: FileEncoding,
}

pub(super) fn decode_text_file(
    bytes: Vec<u8>,
    path_display: &str,
    operation: &str,
) -> Result<DecodedFile, FunctionCallError> {
    if is_utf16_le(&bytes) {
        if !bytes.len().is_multiple_of(2) {
            return Err(FunctionCallError::RespondToModel(format!(
                "{operation} could not decode UTF-16LE file `{path_display}` because it has an incomplete code unit"
            )));
        }
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let content = String::from_utf16(&units).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{operation} could not decode UTF-16LE file `{path_display}`: {error}"
            ))
        })?;
        return Ok(DecodedFile {
            content,
            encoding: FileEncoding::Utf16Le,
        });
    }

    let content = String::from_utf8(bytes).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{operation} only supports UTF-8 and UTF-16LE text files; `{path_display}` could not be decoded: {error}"
        ))
    })?;
    Ok(DecodedFile {
        content,
        encoding: FileEncoding::Utf8,
    })
}

pub(super) fn is_utf16_le(bytes: &[u8]) -> bool {
    bytes.starts_with(UTF16_LE_BOM)
}

pub(super) fn encode_file(content: &str, encoding: FileEncoding) -> Vec<u8> {
    match encoding {
        FileEncoding::Utf8 => content.as_bytes().to_vec(),
        FileEncoding::Utf16Le => {
            let mut bytes = UTF16_LE_BOM.to_vec();
            bytes.extend(
                content
                    .strip_prefix('\u{feff}')
                    .unwrap_or(content)
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            );
            bytes
        }
    }
}

#[cfg(test)]
#[path = "file_encoding_tests.rs"]
mod tests;
