// Copyright 2021 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(missing_docs)]

use std::fs;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Poll;

use tempfile::NamedTempFile;
use tempfile::PersistError;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::io::ReadBuf;

pub use self::platform::*;

#[derive(Debug, Error)]
#[error("Cannot access {path}")]
pub struct PathError {
    pub path: PathBuf,
    #[source]
    pub error: io::Error,
}

pub trait IoResultExt<T> {
    fn context(self, path: impl AsRef<Path>) -> Result<T, PathError>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn context(self, path: impl AsRef<Path>) -> Result<T, PathError> {
        self.map_err(|error| PathError {
            path: path.as_ref().to_path_buf(),
            error,
        })
    }
}

/// Creates a directory or does nothing if the directory already exists.
///
/// Returns the underlying error if the directory can't be created.
/// The function will also fail if intermediate directories on the path do not
/// already exist.
pub fn create_or_reuse_dir(dirname: &Path) -> io::Result<()> {
    match fs::create_dir(dirname) {
        Ok(()) => Ok(()),
        Err(_) if dirname.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes all files in the directory, but not the directory itself.
///
/// The directory must exist, and there should be no sub directories.
pub fn remove_dir_contents(dirname: &Path) -> Result<(), PathError> {
    for entry in dirname.read_dir().context(dirname)? {
        let entry = entry.context(dirname)?;
        let path = entry.path();
        fs::remove_file(&path).context(&path)?;
    }
    Ok(())
}

/// Expands "~/" to "$HOME/".
pub fn expand_home_path(path_str: &str) -> PathBuf {
    if let Some(remainder) = path_str.strip_prefix("~/") {
        if let Ok(home_dir_str) = std::env::var("HOME") {
            return PathBuf::from(home_dir_str).join(remainder);
        }
    }
    PathBuf::from(path_str)
}

/// Turns the given `to` path into relative path starting from the `from` path.
///
/// Both `from` and `to` paths are supposed to be absolute and normalized in the
/// same manner.
pub fn relative_path(from: &Path, to: &Path) -> PathBuf {
    // Find common prefix.
    for (i, base) in from.ancestors().enumerate() {
        if let Ok(suffix) = to.strip_prefix(base) {
            if i == 0 && suffix.as_os_str().is_empty() {
                return ".".into();
            } else {
                let mut result = PathBuf::from_iter(std::iter::repeat_n("..", i));
                result.push(suffix);
                return result;
            }
        }
    }

    // No common prefix found. Return the original (absolute) path.
    to.to_owned()
}

/// Consumes as much `..` and `.` as possible without considering symlinks.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(result.components().next_back(), Some(Component::Normal(_))) =>
            {
                // Do not pop ".."
                let popped = result.pop();
                assert!(popped);
            }
            _ => {
                result.push(c);
            }
        }
    }

    if result.as_os_str().is_empty() {
        ".".into()
    } else {
        result
    }
}

/// Like `NamedTempFile::persist()`, but doesn't try to overwrite the existing
/// target on Windows.
pub fn persist_content_addressed_temp_file<P: AsRef<Path>>(
    temp_file: NamedTempFile,
    new_path: P,
) -> io::Result<File> {
    if cfg!(windows) {
        // On Windows, overwriting file can fail if the file is opened without
        // FILE_SHARE_DELETE for example. We don't need to take a risk if the
        // file already exists.
        match temp_file.persist_noclobber(&new_path) {
            Ok(file) => Ok(file),
            Err(PersistError { error, file: _ }) => {
                if let Ok(existing_file) = File::open(new_path) {
                    // TODO: Update mtime to help GC keep this file
                    Ok(existing_file)
                } else {
                    Err(error)
                }
            }
        }
    } else {
        // On Unix, rename() is atomic and should succeed even if the
        // destination file exists. Checking if the target exists might involve
        // non-atomic operation, so don't use persist_noclobber().
        temp_file
            .persist(new_path)
            .map_err(|PersistError { error, file: _ }| error)
    }
}

/// Reads from an async source and writes to a sync destination. Does not spawn
/// a task, so writes will block.
pub async fn copy_async_to_sync<R: AsyncRead, W: Write + ?Sized>(
    reader: R,
    writer: &mut W,
) -> io::Result<usize> {
    let mut buf = vec![0; 16 << 10];
    let mut total_written_bytes = 0;

    let mut reader = std::pin::pin!(reader);
    loop {
        let written_bytes = reader.read(&mut buf).await?;
        if written_bytes == 0 {
            return Ok(total_written_bytes);
        }
        writer.write_all(&buf[0..written_bytes])?;
        total_written_bytes += written_bytes;
    }
}

/// `AsyncRead`` implementation backed by a `Read`. It is not actually async;
/// the goal is simply to avoid reading the full contents from the `Read` into
/// memory.
pub struct BlockingAsyncReader<R> {
    reader: R,
}

impl<R: Read + Unpin> BlockingAsyncReader<R> {
    /// Creates a new `BlockingAsyncReader`
    pub fn new(reader: R) -> Self {
        Self { reader }
    }
}

impl<R: Read + Unpin> AsyncRead for BlockingAsyncReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let num_bytes_read = self.reader.read(buf.initialize_unfilled())?;
        buf.advance(num_bytes_read);
        Poll::Ready(Ok(()))
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    /// Symlinks are always available on UNIX
    pub fn check_symlink_support() -> io::Result<bool> {
        Ok(true)
    }

    pub fn try_symlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
        symlink(original, link)
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::fs::symlink_file;
    use std::path::Path;

    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    /// Symlinks may or may not be enabled on Windows. They require the
    /// Developer Mode setting, which is stored in the registry key below.
    pub fn check_symlink_support() -> io::Result<bool> {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let sideloading =
            hklm.open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\AppModelUnlock")?;
        let developer_mode: u32 = sideloading.get_value("AllowDevelopmentWithoutDevLicense")?;
        Ok(developer_mode == 1)
    }

    pub fn try_symlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
        // this will create a nonfunctional link for directories, but at the moment
        // we don't have enough information in the tree to determine whether the
        // symlink target is a file or a directory
        // note: if developer mode is not enabled the error code will be 1314,
        // ERROR_PRIVILEGE_NOT_HELD

        symlink_file(original, link)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::Write as _;

    use itertools::Itertools as _;
    use pollster::FutureExt as _;
    use test_case::test_case;

    use super::*;
    use crate::tests::new_temp_dir;

    #[test]
    fn normalize_too_many_dot_dot() {
        assert_eq!(normalize_path(Path::new("foo/..")), Path::new("."));
        assert_eq!(normalize_path(Path::new("foo/../..")), Path::new(".."));
        assert_eq!(
            normalize_path(Path::new("foo/../../..")),
            Path::new("../..")
        );
        assert_eq!(
            normalize_path(Path::new("foo/../../../bar/baz/..")),
            Path::new("../../bar")
        );
    }

    #[test]
    fn test_persist_no_existing_file() {
        let temp_dir = new_temp_dir();
        let target = temp_dir.path().join("file");
        let mut temp_file = NamedTempFile::new_in(&temp_dir).unwrap();
        temp_file.write_all(b"contents").unwrap();
        assert!(persist_content_addressed_temp_file(temp_file, target).is_ok());
    }

    #[test_case(false ; "existing file open")]
    #[test_case(true ; "existing file closed")]
    fn test_persist_target_exists(existing_file_closed: bool) {
        let temp_dir = new_temp_dir();
        let target = temp_dir.path().join("file");
        let mut temp_file = NamedTempFile::new_in(&temp_dir).unwrap();
        temp_file.write_all(b"contents").unwrap();

        let mut file = File::create(&target).unwrap();
        file.write_all(b"contents").unwrap();
        if existing_file_closed {
            drop(file);
        }

        assert!(persist_content_addressed_temp_file(temp_file, &target).is_ok());
    }

    #[test]
    fn test_copy_async_to_sync_small() {
        let input = b"hello";
        let mut output = vec![];

        let result = copy_async_to_sync(Cursor::new(&input), &mut output).block_on();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 5);
        assert_eq!(output, input);
    }

    #[test]
    fn test_copy_async_to_sync_large() {
        // More than 1 buffer worth of data
        let input = (0..100u8).cycle().take(40000).collect_vec();
        let mut output = vec![];

        let result = copy_async_to_sync(Cursor::new(&input), &mut output).block_on();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 40000);
        assert_eq!(output, input);
    }

    #[test]
    fn test_blocking_async_reader() {
        let input = b"hello";
        let sync_reader = Cursor::new(&input);
        let mut async_reader = BlockingAsyncReader::new(sync_reader);

        let mut buf = [0u8; 3];
        let num_bytes_read = async_reader.read(&mut buf).block_on().unwrap();
        assert_eq!(num_bytes_read, 3);
        assert_eq!(&buf, &input[0..3]);

        let num_bytes_read = async_reader.read(&mut buf).block_on().unwrap();
        assert_eq!(num_bytes_read, 2);
        assert_eq!(&buf[0..2], &input[3..5]);
    }

    #[test]
    fn test_blocking_async_reader_read_to_end() {
        let input = b"hello";
        let sync_reader = Cursor::new(&input);
        let mut async_reader = BlockingAsyncReader::new(sync_reader);

        let mut buf = vec![];
        let num_bytes_read = async_reader.read_to_end(&mut buf).block_on().unwrap();
        assert_eq!(num_bytes_read, input.len());
        assert_eq!(&buf, &input);
    }
}
