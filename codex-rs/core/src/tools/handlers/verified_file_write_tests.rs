use super::*;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::LOCAL_FS;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::RemoveOptions;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy)]
enum WriteFailureMode {
    Write,
    ReadAfterWrite,
    Corrupt,
}

struct ScriptedFileSystem {
    contents: Mutex<Option<Vec<u8>>>,
    reads: AtomicUsize,
    mode: WriteFailureMode,
}

impl ScriptedFileSystem {
    fn new(contents: &[u8], mode: WriteFailureMode) -> Self {
        Self {
            contents: Mutex::new(Some(contents.to_vec())),
            reads: AtomicUsize::new(0),
            mode,
        }
    }

    fn contents(&self) -> Option<Vec<u8>> {
        self.contents.lock().expect("contents lock").clone()
    }

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation is unused by verified write tests",
        ))
    }
}

impl ExecutorFileSystem for ScriptedFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async { Self::unsupported() })
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let read_index = self.reads.fetch_add(1, Ordering::SeqCst);
            if read_index > 0 && matches!(self.mode, WriteFailureMode::ReadAfterWrite) {
                return Err(io::Error::other("injected post-write read failure"));
            }
            self.contents()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing test file"))
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async { Self::unsupported() })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        contents: Vec<u8>,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async move {
            match self.mode {
                WriteFailureMode::Write => Err(io::Error::other("injected write failure")),
                WriteFailureMode::ReadAfterWrite => {
                    *self.contents.lock().expect("contents lock") = Some(contents);
                    Ok(())
                }
                WriteFailureMode::Corrupt => {
                    *self.contents.lock().expect("contents lock") =
                        Some(b"silently corrupted\n".to_vec());
                    Ok(())
                }
            }
        })
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: CreateDirectoryOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async { Self::unsupported() })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async { Self::unsupported() })
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: RemoveOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: CopyOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }
}

fn path_uri(path: std::path::PathBuf) -> PathUri {
    PathUri::from_abs_path(&AbsolutePathBuf::try_from(path).expect("absolute temp path"))
}

#[tokio::test]
async fn creates_parent_directories_for_a_verified_new_file() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let path = path_uri(temp.path().join("nested").join("file.txt"));

    write_file_verified(VerifiedFileWrite {
        fs: LOCAL_FS.as_ref(),
        path: &path,
        sandbox: None,
        expected: ExpectedFileContents::Missing,
        updated: b"new contents\n",
    })
    .await
    .expect("write new file");

    assert_eq!(
        std::fs::read(temp.path().join("nested").join("file.txt")).expect("read written file"),
        b"new contents\n"
    );
}

#[tokio::test]
async fn rejects_a_stale_expected_file_without_overwriting_it() {
    let temp = tempfile::tempdir().expect("create temp dir");
    let native_path = temp.path().join("file.txt");
    std::fs::write(&native_path, b"changed externally\n").expect("write fixture");
    let path = path_uri(native_path.clone());

    let error = write_file_verified(VerifiedFileWrite {
        fs: LOCAL_FS.as_ref(),
        path: &path,
        sandbox: None,
        expected: ExpectedFileContents::Present(b"previous contents\n"),
        updated: b"model update\n",
    })
    .await
    .expect_err("stale write should fail");

    assert_eq!(error.commit_state(), FileWriteCommitState::NotCommitted);
    assert!(matches!(error, VerifiedFileWriteError::UnexpectedContents));
    assert_eq!(
        std::fs::read(native_path).expect("read unchanged file"),
        b"changed externally\n"
    );
}

#[tokio::test]
async fn reports_a_write_failure_without_changing_the_expected_contents() {
    let path = path_uri(std::env::temp_dir().join("verified-write-failure.txt"));
    let fs = ScriptedFileSystem::new(b"before\n", WriteFailureMode::Write);

    let error = write_file_verified(VerifiedFileWrite {
        fs: &fs,
        path: &path,
        sandbox: None,
        expected: ExpectedFileContents::Present(b"before\n"),
        updated: b"after\n",
    })
    .await
    .expect_err("write should fail");

    assert_eq!(error.commit_state(), FileWriteCommitState::Unknown);
    assert!(matches!(error, VerifiedFileWriteError::Write(_)));
    assert_eq!(fs.contents(), Some(b"before\n".to_vec()));
}

#[tokio::test]
async fn reports_a_post_write_read_failure_after_the_update_is_committed() {
    let path = path_uri(std::env::temp_dir().join("verified-read-after-write-failure.txt"));
    let fs = ScriptedFileSystem::new(b"before\n", WriteFailureMode::ReadAfterWrite);

    let error = write_file_verified(VerifiedFileWrite {
        fs: &fs,
        path: &path,
        sandbox: None,
        expected: ExpectedFileContents::Present(b"before\n"),
        updated: b"after\n",
    })
    .await
    .expect_err("post-write read should fail");

    assert_eq!(error.commit_state(), FileWriteCommitState::Committed);
    assert!(matches!(error, VerifiedFileWriteError::ReadAfterWrite(_)));
    assert_eq!(fs.contents(), Some(b"after\n".to_vec()));
}

#[tokio::test]
async fn rejects_a_silent_write_corruption() {
    let path = path_uri(std::env::temp_dir().join("verified-corrupt-write.txt"));
    let fs = ScriptedFileSystem::new(b"before\n", WriteFailureMode::Corrupt);

    let error = write_file_verified(VerifiedFileWrite {
        fs: &fs,
        path: &path,
        sandbox: None,
        expected: ExpectedFileContents::Present(b"before\n"),
        updated: b"after\n",
    })
    .await
    .expect_err("corrupt write should fail verification");

    assert_eq!(error.commit_state(), FileWriteCommitState::Unknown);
    assert!(matches!(
        error,
        VerifiedFileWriteError::UnexpectedWrittenContents
    ));
    assert_eq!(fs.contents(), Some(b"silently corrupted\n".to_vec()));
}
