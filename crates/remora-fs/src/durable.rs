//! The flushes of spec 5.8, the single owner of the fsync policy (spec 4). Remora
//! flushes at exactly the points of [`Point`] (D-22); every flush names its point.
//! Directory flushes make no call on Windows, where NTFS and ReFS journal the
//! entries (spec 5.4).

use crate::sys::{Dir, at};
use std::fs::File;
use std::io;

/// A durability point of spec 5.8. F10 (SQLite commits) and F11 (log rotation)
/// never flush through this module, so they have no variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// A blob temporary file was written: the file.
    F1,
    /// A blob was renamed into `blobs/<aa>/`: that directory, and `blobs/` if
    /// `<aa>` was created.
    F2,
    /// A manifest temporary file was written, then renamed: the file, then
    /// `manifests/<aa>/`, and `manifests/` if created.
    F3,
    /// A state file was written with `write_atomic`: the file, then its directory.
    F4,
    /// `machine.key` was created: the file, then `<data>`.
    F5,
    /// A directory was created: its parent.
    F6,
    /// A restored file was staged: the staged file.
    F7,
    /// A restore changed entries: every directory whose entries changed, deepest
    /// first, then the root's parent if the root was recreated.
    F8,
    /// The restore staging directory was removed: the root directory.
    F9,
    /// `machine.keys.log` was appended to: the file, and `<data>` when the log was
    /// created.
    F12,
}

impl Point {
    /// Whether the point flushes a file.
    pub const fn flushes_file(self) -> bool {
        matches!(
            self,
            Self::F1 | Self::F3 | Self::F4 | Self::F5 | Self::F7 | Self::F12
        )
    }

    /// Whether the point flushes a directory.
    pub const fn flushes_dir(self) -> bool {
        !matches!(self, Self::F1 | Self::F7)
    }
}

/// Flushes `file` to storage: `fsync` on Linux, `F_FULLFSYNC` on macOS,
/// `FlushFileBuffers` on Windows, where `file` must be open for writing.
pub fn flush_file(file: &File, point: Point) -> io::Result<()> {
    debug_assert!(point.flushes_file(), "{point:?} flushes no file");
    file.sync_all()
}

/// Flushes the entries of `dir` to storage: `fsync` on Linux, `F_FULLFSYNC` on
/// macOS, and no call on Windows.
pub fn flush_dir(dir: &Dir, point: Point) -> io::Result<()> {
    debug_assert!(point.flushes_dir(), "{point:?} flushes no directory");
    at::sync_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::test_dir::TestDir;

    #[test]
    fn every_point_flushes_something() {
        use Point::*;
        for point in [F1, F2, F3, F4, F5, F6, F7, F8, F9, F12] {
            assert!(point.flushes_file() || point.flushes_dir(), "{point:?}");
        }
        assert!(!F2.flushes_file() && !F6.flushes_file() && !F8.flushes_file());
    }

    #[test]
    fn flushes_succeed() {
        let tmp = TestDir::new();
        let path = tmp.path().join("file");
        let file = std::fs::File::create(&path).unwrap();
        flush_file(&file, Point::F4).unwrap();
        // The handle is open for reading only, so FlushFileBuffers on it would fail
        // with access denied on Windows: success shows that no call is made there.
        flush_dir(&Dir::open(tmp.path()).unwrap(), Point::F6).unwrap();
    }
}
