//! The open-directory handle shared by every `remora-fs` module that works relative
//! to a directory: owner-only creation, temporary files, the no-follow API.

use std::io;
use std::path::Path;

/// An open directory: an `OwnedFd` on Unix, an `OwnedHandle` on Windows.
///
/// The handle stays valid for the directory itself when the directory is renamed or
/// its path is replaced, which is what makes work relative to it safe from races.
#[derive(Debug)]
pub struct Dir {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    /// A Windows listing keeps its position in the file object, which a duplicated
    /// handle shares; one lock per file object keeps two listings apart.
    #[cfg(windows)]
    listing: std::sync::Arc<std::sync::Mutex<()>>,
}

impl Dir {
    /// Opens the directory at `path`.
    ///
    /// Symlinks in the leading components are followed, so callers pass a canonical
    /// path. A symlink, junction or other reparse point as the last component is
    /// refused: Linux and Windows report `NotADirectory`, macOS reports `ELOOP`.
    /// Anything else that is not a directory fails with `NotADirectory`.
    pub fn open(path: &Path) -> io::Result<Self> {
        imp::open(path)
    }

    /// A second handle to the same open directory.
    pub fn try_clone(&self) -> io::Result<Self> {
        #[cfg(unix)]
        return Ok(Self {
            fd: self.fd.try_clone()?,
        });
        #[cfg(windows)]
        return Ok(Self {
            handle: self.handle.try_clone()?,
            listing: std::sync::Arc::clone(&self.listing),
        });
    }

    #[cfg(unix)]
    pub(super) fn from_fd(fd: std::os::fd::OwnedFd) -> Self {
        Self { fd }
    }

    #[cfg(windows)]
    pub(super) fn from_handle(handle: std::os::windows::io::OwnedHandle) -> Self {
        Self {
            handle,
            listing: std::sync::Arc::default(),
        }
    }

    /// Held while a listing reads through this handle.
    #[cfg(windows)]
    pub(super) fn lock_listing(&self) -> std::sync::MutexGuard<'_, ()> {
        self.listing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for Dir {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsHandle for Dir {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

#[cfg(unix)]
mod imp {
    use super::Dir;
    use rustix::fs::{Mode, OFlags};
    use std::io;
    use std::path::Path;

    pub(super) fn open(path: &Path) -> io::Result<Dir> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::open(path, flags, Mode::empty())?;
        Ok(Dir { fd })
    }
}

#[cfg(windows)]
mod imp {
    use super::Dir;
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TRAVERSE,
    };

    pub(super) fn open(path: &Path) -> io::Result<Dir> {
        // Backup semantics opens a directory at all; open-reparse-point opens a link
        // itself instead of its target, so the check below can refuse it (spec 5.9).
        let file = OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | FILE_TRAVERSE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let attributes = file.metadata()?.file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "the last path component is a reparse point",
            ));
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(Dir::from_handle(file.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::test_dir::{TestDir, symlink_dir};
    use std::fs::File;

    /// Reopens the handle as a `File` to inspect what it refers to.
    fn reopen(dir: &Dir) -> File {
        #[cfg(unix)]
        let owned = std::os::fd::AsFd::as_fd(dir).try_clone_to_owned().unwrap();
        #[cfg(windows)]
        let owned = std::os::windows::io::AsHandle::as_handle(dir)
            .try_clone_to_owned()
            .unwrap();
        File::from(owned)
    }

    #[test]
    fn opens_a_directory() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        assert!(reopen(&dir).metadata().unwrap().is_dir());
    }

    #[test]
    fn refuses_a_file() {
        let tmp = TestDir::new();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let err = Dir::open(&file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{err}");
    }

    #[test]
    fn refuses_a_missing_path() {
        let tmp = TestDir::new();
        let err = Dir::open(&tmp.path().join("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    #[test]
    fn refuses_a_symlink_as_last_component() {
        let tmp = TestDir::new();
        tmp.mkdir_all("real");
        symlink_dir(&tmp.path().join("real"), &tmp.path().join("link")).unwrap();
        let err = Dir::open(&tmp.path().join("link")).unwrap_err();
        #[cfg(unix)]
        let eloop = err.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error());
        #[cfg(windows)]
        let eloop = false;
        assert!(err.kind() == io::ErrorKind::NotADirectory || eloop, "{err}");
    }

    #[test]
    fn follows_a_symlink_in_a_leading_component() {
        let tmp = TestDir::new();
        tmp.mkdir_all("real/sub");
        symlink_dir(&tmp.path().join("real"), &tmp.path().join("link")).unwrap();
        let dir = Dir::open(&tmp.path().join("link").join("sub")).unwrap();
        assert!(reopen(&dir).metadata().unwrap().is_dir());
    }
}
