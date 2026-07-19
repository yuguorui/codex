use crate::function_tool::FunctionCallError;
use crate::tools::handlers::file_read::FILE_READ_IMAGE_RESIZE_LIMITS;
use crate::tools::handlers::file_read::MAX_READ_IMAGES_PER_RESULT;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_utils_image::PromptImageMode;
use codex_utils_image::load_for_prompt_bytes;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

pub(super) const PDF_MAX_PAGES_PER_READ: usize = MAX_READ_IMAGES_PER_RESULT;
pub(super) const PDF_MAX_EXTRACT_SIZE: u64 = 100 * 1024 * 1024;
const PDF_INLINE_PAGE_LIMIT: usize = MAX_READ_IMAGES_PER_RESULT;
const PDF_RENDER_TIMEOUT: Duration = Duration::from_secs(120);
const PDF_INFO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PDF_PAGE_IMAGE_BYTES: usize = 3_750_000;
const MAX_PDF_IMAGES_TOTAL_BYTES: usize = 20_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PdfPageRange {
    pub first: usize,
    pub last: usize,
}

pub(super) struct PdfReadOutput {
    pub content: Vec<FunctionCallOutputContentItem>,
    pub event_summary: String,
}

pub(super) fn parse_pdf_page_range(pages: &str) -> Result<PdfPageRange, FunctionCallError> {
    let pages = pages.trim();
    let invalid = || {
        FunctionCallError::RespondToModel(format!(
            "Invalid pages parameter: `{pages}`. Use formats like `1-4` or `3`. Pages are 1-indexed."
        ))
    };
    let (first, last) = match pages.split_once('-') {
        Some((_, "")) => return Err(page_limit_error(pages)),
        Some((first, last)) if !last.contains('-') => (
            first.parse::<usize>().map_err(|_| invalid())?,
            last.parse::<usize>().map_err(|_| invalid())?,
        ),
        Some(_) => return Err(invalid()),
        None => {
            let page = pages.parse::<usize>().map_err(|_| invalid())?;
            (page, page)
        }
    };
    if first == 0 || last < first {
        return Err(invalid());
    }
    if last.saturating_sub(first).saturating_add(1) > PDF_MAX_PAGES_PER_READ {
        return Err(page_limit_error(pages));
    }
    Ok(PdfPageRange { first, last })
}

fn page_limit_error(pages: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "Page range `{pages}` exceeds maximum of {PDF_MAX_PAGES_PER_READ} pages per request. Please use a smaller range."
    ))
}

pub(super) async fn render_pdf_pages(
    pdf_bytes: Vec<u8>,
    source_path: &str,
    pages: Option<PdfPageRange>,
) -> Result<PdfReadOutput, FunctionCallError> {
    if pdf_bytes.is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "PDF file is empty: {source_path}"
        )));
    }
    if pdf_bytes.len() > PDF_MAX_EXTRACT_SIZE as usize {
        return Err(FunctionCallError::RespondToModel(format!(
            "PDF file exceeds maximum allowed size for page extraction ({PDF_MAX_EXTRACT_SIZE} bytes)."
        )));
    }
    if !pdf_bytes.starts_with(b"%PDF-") {
        return Err(FunctionCallError::RespondToModel(format!(
            "File is not a valid PDF (missing %PDF- header): {source_path}"
        )));
    }

    let temp = tempfile::tempdir().map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "Failed to prepare temporary PDF workspace: {error}"
        ))
    })?;
    let pdf_path = temp.path().join("input.pdf");
    let output_prefix = temp.path().join("page");
    let write_path = pdf_path.clone();
    tokio::task::spawn_blocking(move || std::fs::write(write_path, pdf_bytes))
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "Failed to prepare PDF for rendering: {error}"
            ))
        })?
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "Failed to prepare PDF for rendering: {error}"
            ))
        })?;

    if pages.is_none()
        && pdf_page_count(&pdf_path)
            .await
            .is_some_and(|page_count| page_count > PDF_INLINE_PAGE_LIMIT)
    {
        return Err(pdf_inline_page_limit_error());
    }

    let (first, last) = pages.map_or((1, PDF_INLINE_PAGE_LIMIT + 1), |range| {
        (range.first, range.last)
    });
    let mut command = Command::new("pdftoppm");
    command
        .args(["-jpeg", "-r", "100", "-f"])
        .arg(first.to_string())
        .arg("-l")
        .arg(last.to_string())
        .arg(&pdf_path)
        .arg(&output_prefix)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(PDF_RENDER_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            FunctionCallError::RespondToModel(
                "PDF page rendering timed out after 120 seconds".to_string(),
            )
        })?
        .map_err(|error| {
            let message = if error.kind() == std::io::ErrorKind::NotFound {
                "pdftoppm is not installed. Install poppler-utils (`brew install poppler` on macOS or `apt-get install poppler-utils` on Debian/Ubuntu) to read PDF files."
                    .to_string()
            } else {
                format!("Failed to start PDF page renderer: {error}")
            };
            FunctionCallError::RespondToModel(message)
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = crate::tools::handlers::file_path::summarize_tool_argument(&stderr);
        return Err(FunctionCallError::RespondToModel(format!(
            "pdftoppm failed to render the PDF: {stderr}"
        )));
    }

    let mut page_paths = std::fs::read_dir(temp.path())
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "Failed to inspect rendered PDF pages: {error}"
            ))
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jpg"))
        .collect::<Vec<_>>();
    page_paths.sort_by_key(|path| pdf_page_number(path).unwrap_or(usize::MAX));
    if page_paths.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "pdftoppm produced no output pages. The PDF may be invalid or the requested range may be outside the document."
                .to_string(),
        ));
    }
    if pages.is_none() && page_paths.len() > PDF_INLINE_PAGE_LIMIT {
        return Err(pdf_inline_page_limit_error());
    }

    let mut total_image_bytes = 0usize;
    let mut content = vec![FunctionCallOutputContentItem::InputText {
        text: format!(
            "PDF pages extracted: {} page(s) from {source_path}",
            page_paths.len()
        ),
    }];
    for page_path in page_paths {
        let bytes = std::fs::read(&page_path).map_err(|error| {
            FunctionCallError::RespondToModel(format!("Failed to read rendered PDF page: {error}"))
        })?;
        let image = load_for_prompt_bytes(
            &page_path,
            bytes,
            PromptImageMode::ResizeWithLimits(FILE_READ_IMAGE_RESIZE_LIMITS),
        )
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "Failed to process rendered PDF page: {error}"
            ))
        })?;
        if image.bytes.len() > MAX_PDF_PAGE_IMAGE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "Rendered PDF page is too large for the model ({} bytes).",
                image.bytes.len()
            )));
        }
        total_image_bytes = total_image_bytes.saturating_add(image.bytes.len());
        if total_image_bytes > MAX_PDF_IMAGES_TOTAL_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "Rendered PDF pages exceed the combined image limit of {MAX_PDF_IMAGES_TOTAL_BYTES} bytes. Read a smaller page range."
            )));
        }
        content.push(FunctionCallOutputContentItem::InputImage {
            image_url: image.into_data_url(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        });
    }
    let event_summary = format!(
        "Read {} PDF page(s) from `{source_path}`",
        content.len().saturating_sub(1)
    );
    Ok(PdfReadOutput {
        content,
        event_summary,
    })
}

async fn pdf_page_count(path: &Path) -> Option<usize> {
    let mut command = Command::new("pdfinfo");
    command
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(PDF_INFO_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_pdf_page_count(&String::from_utf8_lossy(&output.stdout))
}

fn parse_pdf_page_count(output: &str) -> Option<usize> {
    output.lines().find_map(|line| {
        let value = line.strip_prefix("Pages:")?.trim();
        value.parse().ok()
    })
}

fn pdf_inline_page_limit_error() -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "This PDF has more than {PDF_INLINE_PAGE_LIMIT} pages, which is too many to read at once. Use the pages parameter to read a specific range such as `1-4`. Maximum {PDF_MAX_PAGES_PER_READ} pages per request."
    ))
}

fn pdf_page_number(path: &Path) -> Option<usize> {
    let stem = path.file_stem()?.to_str()?;
    stem.rsplit_once('-')?.1.parse().ok()
}

#[cfg(test)]
#[path = "file_read_pdf_tests.rs"]
mod tests;
