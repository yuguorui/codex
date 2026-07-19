use super::*;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LOCAL_FS;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::RemoveOptions;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use std::io;
use std::sync::Arc;
use tempfile::tempdir;

#[derive(Clone, Copy)]
enum StreamBehavior {
    Unsupported,
    ErrorAfterSelectedRange,
}

struct TestStreamFileSystem {
    inner: Arc<dyn ExecutorFileSystem>,
    behavior: StreamBehavior,
}

impl ExecutorFileSystem for TestStreamFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        self.inner.canonicalize(path, sandbox)
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        self.inner.read_file(path, sandbox)
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            match self.behavior {
                StreamBehavior::Unsupported => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "streaming disabled for test",
                )),
                StreamBehavior::ErrorAfterSelectedRange => {
                    let chunks = vec![
                        Ok(b"alpha\nbeta\n".to_vec().into()),
                        Err(io::Error::other("range reader consumed one chunk too many")),
                    ];
                    Ok(FileSystemReadStream::new(futures::stream::iter(chunks)))
                }
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.write_file(path, contents, sandbox)
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        self.inner.get_metadata(path, sandbox)
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        self.inner.read_directory(path, sandbox)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner.remove(path, options, sandbox)
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        self.inner
            .copy(source_path, destination_path, options, sandbox)
    }
}

#[test]
fn text_range_uses_compact_claude_code_line_numbers() {
    assert_eq!(
        read_text_range_with_state("alpha\r\nbeta\ngamma\n", 2, Some(2))
            .expect("read range")
            .0,
        "2\tbeta\n3\tgamma"
    );
}

#[test]
fn buffered_text_range_caches_only_selected_content() {
    assert_eq!(
        read_text_range_with_state("alpha\nbeta\ngamma\n", 2, Some(1))
            .expect("read buffered range"),
        ("2\tbeta".to_string(), "beta".to_string())
    );
}

#[test]
fn zero_offset_starts_at_the_first_line_and_labels_it_zero() {
    assert_eq!(
        read_text_range_with_state("alpha\nbeta", 0, Some(1))
            .expect("read range")
            .0,
        "0\talpha"
    );
}

#[test]
fn empty_and_out_of_range_reads_return_reminders() {
    assert_eq!(
        read_text_range_with_state("", 1, None)
            .expect("read empty file")
            .0,
        EMPTY_FILE_REMINDER
    );
    assert_eq!(
        read_text_range_with_state("alpha\nbeta", 4, None)
            .expect("read past end")
            .0,
        "<system-reminder>Warning: the file exists but is shorter than the provided offset (4). The file has 2 lines.</system-reminder>"
    );
}

#[test]
fn text_range_rejects_output_above_the_model_context_budget() {
    let error = read_text_range_with_state(&"x".repeat(33_000), 1, None)
        .expect_err("oversized model output should fail");

    assert_eq!(error, read_output_too_large_error());
}

#[tokio::test]
async fn streamed_range_reads_only_selected_lines() {
    let temp = tempdir().expect("tempdir");
    let native_path = temp.path().join("example.txt");
    std::fs::write(&native_path, "alpha\r\nbeta\r\ngamma\r\n").expect("write fixture");
    let path = PathUri::from_host_native_path(&native_path).expect("path URI");

    let output = read_text_range_stream(
        LOCAL_FS.as_ref(),
        &path,
        None,
        &native_path.to_string_lossy(),
        2,
        Some(1),
    )
    .await
    .expect("read streamed range");

    assert_eq!(output, ("2\tbeta".to_string(), "beta".to_string()));
}

#[tokio::test]
async fn streamed_range_covering_eof_returns_all_selected_content() {
    let temp = tempdir().expect("tempdir");
    let native_path = temp.path().join("example.txt");
    std::fs::write(&native_path, "alpha\nbeta\n").expect("write fixture");
    let path = PathUri::from_host_native_path(&native_path).expect("path URI");

    let output = read_text_range_stream(
        LOCAL_FS.as_ref(),
        &path,
        None,
        &native_path.to_string_lossy(),
        1,
        Some(100),
    )
    .await
    .expect("read streamed range");

    assert_eq!(
        output,
        (
            "1\talpha\n2\tbeta\n3\t".to_string(),
            "alpha\nbeta\n".to_string(),
        )
    );
}

#[tokio::test]
async fn streamed_range_falls_back_for_utf16le() {
    let temp = tempdir().expect("tempdir");
    let native_path = temp.path().join("utf16.txt");
    let bytes = "\u{feff}alpha\nbeta\ngamma\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(&native_path, bytes).expect("write UTF-16 fixture");
    let path = PathUri::from_host_native_path(&native_path).expect("path URI");

    let output = read_text_range_stream(
        LOCAL_FS.as_ref(),
        &path,
        None,
        &native_path.to_string_lossy(),
        2,
        Some(1),
    )
    .await
    .expect("read UTF-16 streamed range");

    assert_eq!(output, ("2\tbeta".to_string(), "beta".to_string()));
}

#[tokio::test]
async fn unsupported_streaming_falls_back_to_buffered_read() {
    let temp = tempdir().expect("tempdir");
    let native_path = temp.path().join("fallback.txt");
    std::fs::write(&native_path, "alpha\nbeta\ngamma\n").expect("write fixture");
    let path = PathUri::from_host_native_path(&native_path).expect("path URI");
    let fs = TestStreamFileSystem {
        inner: Arc::clone(&LOCAL_FS),
        behavior: StreamBehavior::Unsupported,
    };

    let output =
        read_text_range_stream(&fs, &path, None, &native_path.to_string_lossy(), 2, Some(1))
            .await
            .expect("buffered fallback");

    assert_eq!(output, ("2\tbeta".to_string(), "beta".to_string()));
}

#[tokio::test]
async fn limited_streaming_stops_after_the_selected_range() {
    let path = PathUri::from_host_native_path(std::env::temp_dir().join("stream-range.txt"))
        .expect("path URI");
    let fs = TestStreamFileSystem {
        inner: Arc::clone(&LOCAL_FS),
        behavior: StreamBehavior::ErrorAfterSelectedRange,
    };

    let output = read_text_range_stream(&fs, &path, None, "stream-range.txt", 2, Some(1))
        .await
        .expect("selected range should finish before the injected error");

    assert_eq!(output, ("2\tbeta".to_string(), "beta".to_string()));
}
