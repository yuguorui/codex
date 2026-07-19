use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_utils_path_uri::PathUri;
use std::io;

pub(super) enum ExpectedFileContents<'a> {
    Missing,
    Present(&'a [u8]),
}

impl ExpectedFileContents<'_> {
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Missing => None,
            Self::Present(contents) => Some(contents),
        }
    }
}

pub(super) struct VerifiedFileWrite<'a> {
    pub fs: &'a dyn ExecutorFileSystem,
    pub path: &'a PathUri,
    pub sandbox: Option<&'a FileSystemSandboxContext>,
    pub expected: ExpectedFileContents<'a>,
    pub updated: &'a [u8],
}

#[derive(Debug)]
pub(super) enum VerifiedFileWriteError {
    ReadBeforeWrite(io::Error),
    UnexpectedContents,
    CreateParent(io::Error),
    Write(io::Error),
    ReadAfterWrite(io::Error),
    UnexpectedWrittenContents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileWriteCommitState {
    NotCommitted,
    Committed,
    Unknown,
}

impl VerifiedFileWriteError {
    pub(super) fn commit_state(&self) -> FileWriteCommitState {
        match self {
            Self::ReadBeforeWrite(_) | Self::UnexpectedContents | Self::CreateParent(_) => {
                FileWriteCommitState::NotCommitted
            }
            Self::ReadAfterWrite(_) => FileWriteCommitState::Committed,
            Self::Write(_) | Self::UnexpectedWrittenContents => FileWriteCommitState::Unknown,
        }
    }
}

pub(super) async fn write_file_verified(
    request: VerifiedFileWrite<'_>,
) -> Result<(), VerifiedFileWriteError> {
    let current = match request.fs.read_file(request.path, request.sandbox).await {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(VerifiedFileWriteError::ReadBeforeWrite(error)),
    };
    if current.as_deref() != request.expected.as_bytes() {
        return Err(VerifiedFileWriteError::UnexpectedContents);
    }

    if matches!(&request.expected, ExpectedFileContents::Missing)
        && let Some(parent) = request.path.parent()
    {
        request
            .fs
            .create_directory(
                &parent,
                CreateDirectoryOptions { recursive: true },
                request.sandbox,
            )
            .await
            .map_err(VerifiedFileWriteError::CreateParent)?;
    }

    request
        .fs
        .write_file(request.path, request.updated.to_vec(), request.sandbox)
        .await
        .map_err(VerifiedFileWriteError::Write)?;
    let written = request
        .fs
        .read_file(request.path, request.sandbox)
        .await
        .map_err(VerifiedFileWriteError::ReadAfterWrite)?;
    if written != request.updated {
        return Err(VerifiedFileWriteError::UnexpectedWrittenContents);
    }
    Ok(())
}

#[cfg(test)]
#[path = "verified_file_write_tests.rs"]
mod tests;
