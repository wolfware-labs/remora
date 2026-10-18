//! The OS's own spelling of an existing path: opened, symlinks followed, and read
//! back from the descriptor or handle (spec 5.6 rules 2 and 3).

use std::io;
use std::path::{Path, PathBuf};

/// What `final_path` found.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// The path read back from the open descriptor or handle.
    pub(crate) path: PathBuf,
    /// Whether the path names a directory.
    pub(crate) is_dir: bool,
}

/// Opens `path`, following symlinks, and reads its path back.
///
/// Linux opens with `O_PATH` and reads `/proc/self/fd/<n>`; without `/proc` this
/// fails. macOS opens read-only with `O_NONBLOCK` (a FIFO does not block) and calls
/// `fcntl(F_GETPATH)`; a file without read permission fails with `PermissionDenied`.
/// Windows opens with backup semantics and without `FILE_FLAG_OPEN_REPARSE_POINT`,
/// so links and junctions are followed, then calls `GetFinalPathNameByHandleW` with
/// `FILE_NAME_NORMALIZED`. A volume without a DOS name falls back to its GUID form.
/// The `\\?\` prefix of a drive path is removed and the drive letter upper-cased;
/// `\\?\UNC\server\share` becomes `\\server\share`.
pub(crate) fn final_path(path: &Path) -> io::Result<Resolved> {
    imp::final_path(path)
}

#[cfg(unix)]
mod imp {
    use super::Resolved;
    use rustix::fs::{FileType, Mode, OFlags};
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    #[cfg(target_os = "linux")]
    const FLAGS: OFlags = OFlags::PATH.union(OFlags::CLOEXEC);
    #[cfg(target_os = "macos")]
    const FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::NONBLOCK)
        .union(OFlags::CLOEXEC);

    pub(super) fn final_path(path: &Path) -> io::Result<Resolved> {
        let fd = rustix::fs::open(path, FLAGS, Mode::empty())?;
        let is_dir = FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode).is_dir();
        let bytes = read_back(&fd)?;
        Ok(Resolved {
            path: PathBuf::from(OsString::from_vec(bytes)),
            is_dir,
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn read_back(fd: &std::os::fd::OwnedFd) -> io::Result<Vec<u8>> {
        use std::os::fd::AsRawFd;
        let link = rustix::fs::readlink(format!("/proc/self/fd/{}", fd.as_raw_fd()), Vec::new())?;
        // Once the name the descriptor was opened by is removed, the kernel appends
        // " (deleted)", even while another hard link remains. The text alone cannot
        // tell that from a real name, so the name read back must still lead to the
        // same file.
        let opened = rustix::fs::fstat(fd)?;
        let named = rustix::fs::stat(link.as_c_str())?;
        if (named.st_dev, named.st_ino) != (opened.st_dev, opened.st_ino) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "removed while being resolved",
            ));
        }
        Ok(link.into_bytes())
    }

    #[cfg(target_os = "macos")]
    fn read_back(fd: &std::os::fd::OwnedFd) -> io::Result<Vec<u8>> {
        Ok(rustix::fs::getpath(fd)?.into_bytes())
    }
}

#[cfg(windows)]
mod imp {
    use super::{Resolved, normalize};
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
        VOLUME_NAME_GUID,
    };

    pub(super) fn final_path(path: &Path) -> io::Result<Resolved> {
        // Unlike every other open of spec 5.9 rule 1, no FILE_FLAG_OPEN_REPARSE_POINT:
        // following links is the point of canonicalization (spec 5.6).
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let is_dir = file.metadata()?.is_dir();
        let wide = match read_back(&file, VOLUME_NAME_DOS) {
            Ok(wide) => wide,
            Err(dos) => read_back(&file, VOLUME_NAME_GUID).map_err(|_| dos)?,
        };
        Ok(Resolved {
            path: PathBuf::from(OsString::from_wide(&normalize(wide))),
            is_dir,
        })
    }

    fn read_back(file: &File, volume: u32) -> io::Result<Vec<u16>> {
        let mut buf = vec![0u16; 512];
        loop {
            let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            // SAFETY: the handle is valid while `file` is borrowed, and `buf` has room
            // for `len` UTF-16 units.
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buf.as_mut_ptr(),
                    len,
                    FILE_NAME_NORMALIZED | volume,
                )
            };
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            let n = usize::try_from(n).map_err(io::Error::other)?;
            // On success `n` excludes the terminating NUL; when the buffer is too
            // small it is the size needed, NUL included.
            if n < buf.len() {
                buf.truncate(n);
                return Ok(buf);
            }
            buf.resize(n, 0);
        }
    }
}

/// Removes the `\\?\` prefix of a drive path and upper-cases its letter, and turns
/// `\\?\UNC\server\share` into `\\server\share`. Other forms, such as
/// `\\?\Volume{...}\`, are kept.
#[cfg(any(windows, test))]
fn normalize(mut wide: Vec<u16>) -> Vec<u16> {
    const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];
    let Some(rest) = wide.strip_prefix(&VERBATIM[..]) else {
        return wide;
    };
    if rest.starts_with(&UNC) {
        // `\\?\UNC\server` keeps its leading `\\` and drops `?\UNC\`.
        wide.drain(2..8);
        return wide;
    }
    match rest {
        [letter, colon, ..] if *colon == u16::from(b':') && is_ascii_letter(*letter) => {
            wide.drain(..4);
            wide[0] = ascii_upper(wide[0]);
            wide
        }
        _ => wide,
    }
}

#[cfg(any(windows, test))]
fn is_ascii_letter(unit: u16) -> bool {
    u8::try_from(unit).is_ok_and(|b| b.is_ascii_alphabetic())
}

#[cfg(any(windows, test))]
fn ascii_upper(unit: u16) -> u16 {
    u8::try_from(unit).map_or(unit, |b| u16::from(b.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::test_dir::TestDir;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn normalize_strips_the_drive_prefix_and_upper_cases_the_letter() {
        assert_eq!(normalize(wide(r"\\?\c:\Src\x")), wide(r"C:\Src\x"));
        assert_eq!(normalize(wide(r"\\?\D:\")), wide(r"D:\"));
    }

    #[test]
    fn normalize_turns_unc_into_a_server_path() {
        assert_eq!(
            normalize(wide(r"\\?\UNC\server\share\dir")),
            wide(r"\\server\share\dir")
        );
    }

    #[test]
    fn normalize_keeps_other_forms() {
        let guid = r"\\?\Volume{0b1f6a3e-0000-0000-0000-100000000000}\dir";
        assert_eq!(normalize(wide(guid)), wide(guid));
        assert_eq!(normalize(wide(r"C:\x")), wide(r"C:\x"));
        assert_eq!(normalize(wide(r"\\?\1:\x")), wide(r"\\?\1:\x"));
    }

    #[test]
    fn resolves_a_directory_and_a_file() {
        let tmp = TestDir::new();
        let dir = final_path(tmp.path()).unwrap();
        assert!(dir.is_dir);
        assert!(dir.path.is_absolute(), "{}", dir.path.display());
        std::fs::write(tmp.path().join("file"), b"x").unwrap();
        let file = final_path(&tmp.path().join("file")).unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.path, dir.path.join("file"));
    }

    #[test]
    fn missing_path_is_not_found() {
        let tmp = TestDir::new();
        let err = final_path(&tmp.path().join("missing")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_symlink() {
        let tmp = TestDir::new();
        let real = tmp.mkdir_all("real");
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        let via = final_path(&tmp.path().join("link")).unwrap();
        assert_eq!(via.path, final_path(&real).unwrap().path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn needs_no_read_permission_on_linux() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TestDir::new();
        let file = tmp.path().join("locked");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let found = final_path(&file).unwrap();
        assert_eq!(
            found.path,
            final_path(tmp.path()).unwrap().path.join("locked")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn name_ending_in_deleted_is_kept() {
        let tmp = TestDir::new();
        let file = tmp.path().join("x (deleted)");
        std::fs::write(&file, b"x").unwrap();
        let found = final_path(&file).unwrap();
        assert_eq!(
            found.path,
            final_path(tmp.path()).unwrap().path.join("x (deleted)")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn removed_name_with_another_link_is_not_found() {
        let tmp = TestDir::new();
        let first = tmp.path().join("first");
        std::fs::write(&first, b"x").unwrap();
        std::fs::hard_link(&first, tmp.path().join("second")).unwrap();
        let fd =
            rustix::fs::open(&first, rustix::fs::OFlags::PATH, rustix::fs::Mode::empty()).unwrap();
        std::fs::remove_file(&first).unwrap();
        let err = imp::read_back(&fd).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn subst_drive_resolves_to_its_target() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            DDD_EXACT_MATCH_ON_REMOVE, DDD_REMOVE_DEFINITION, DefineDosDeviceW, GetLogicalDrives,
        };

        struct Defined {
            device: Vec<u16>,
            target: Vec<u16>,
        }
        impl Drop for Defined {
            fn drop(&mut self) {
                // SAFETY: both strings are NUL-terminated and outlive the call.
                unsafe {
                    DefineDosDeviceW(
                        DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE,
                        self.device.as_ptr(),
                        self.target.as_ptr(),
                    );
                }
            }
        }

        let tmp = TestDir::new();
        // SAFETY: takes no arguments.
        let used = unsafe { GetLogicalDrives() };
        let Some(letter) = (b'M'..=b'Z').rev().find(|l| used & (1 << (l - b'A')) == 0) else {
            eprintln!("skipped: no free drive letter");
            return;
        };
        let letter = char::from(letter);
        let defined = Defined {
            device: format!("{letter}:\0").encode_utf16().collect(),
            target: tmp.path().as_os_str().encode_wide().chain([0]).collect(),
        };
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let ok = unsafe { DefineDosDeviceW(0, defined.device.as_ptr(), defined.target.as_ptr()) };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        std::fs::write(tmp.path().join("file"), b"x").unwrap();

        let via = final_path(Path::new(&format!(r"{letter}:\file"))).unwrap();
        assert_eq!(via.path, final_path(&tmp.path().join("file")).unwrap().path);
        drop(defined);
    }
}
