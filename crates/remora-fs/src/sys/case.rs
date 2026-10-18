//! Whether a directory tells names apart by case (spec 5.6 rule 6), and the
//! Windows upcase table.

use super::Dir;
use std::io;

/// Whether `dir` treats `a` and `A` as two names.
///
/// - macOS: `fpathconf(_PC_CASE_SENSITIVE)`.
/// - Windows: the directory's `FILE_CS_FLAG_CASE_SENSITIVE_DIR`. A volume
///   that has no per-directory flag answers with `FILE_CASE_SENSITIVE_SEARCH`.
/// - Linux: case-sensitive unless the directory has `FS_CASEFOLD_FL`. A
///   filesystem without inode flags (`ENOTTY`, `EOPNOTSUPP`) has no casefold
///   either.
pub(crate) fn case_sensitive(dir: &Dir) -> io::Result<bool> {
    imp::case_sensitive(dir)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Dir;
    use rustix::fs::ioctl_getflags;
    use rustix::io::Errno;
    use std::io;

    /// `FS_CASEFOLD_FL`, which `rustix::fs::IFlags` does not name.
    pub(super) const CASEFOLD: u32 = 0x4000_0000;

    pub(super) fn case_sensitive(dir: &Dir) -> io::Result<bool> {
        match ioctl_getflags(dir) {
            Ok(flags) => Ok(flags.bits() & CASEFOLD == 0),
            Err(Errno::NOTTY | Errno::OPNOTSUPP) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Dir;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd};

    pub(super) fn case_sensitive(dir: &Dir) -> io::Result<bool> {
        // SAFETY: the descriptor stays open for the call, borrowed from `dir`.
        let rc = unsafe { libc::fpathconf(dir.as_fd().as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
        match rc {
            -1 => Err(io::Error::last_os_error()),
            0 => Ok(false),
            _ => Ok(true),
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::Dir;
    use crate::sys::at::query;
    use std::io;
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_CASE_SENSITIVE_INFO, FileCaseSensitiveInfo, GetVolumeInformationByHandleW,
    };
    use windows_sys::Win32::System::SystemServices::{
        FILE_CASE_SENSITIVE_SEARCH, FILE_CS_FLAG_CASE_SENSITIVE_DIR,
    };

    pub(super) fn case_sensitive(dir: &Dir) -> io::Result<bool> {
        match query::<FILE_CASE_SENSITIVE_INFO>(dir.as_handle(), FileCaseSensitiveInfo) {
            Ok(info) => Ok(info.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0),
            Err(e) if unsupported(&e) => Ok(volume_flags(dir)? & FILE_CASE_SENSITIVE_SEARCH != 0),
            Err(e) => Err(e),
        }
    }

    /// The errors of a volume that has no per-directory case flag (FAT, exFAT,
    /// some network redirectors).
    fn unsupported(e: &io::Error) -> bool {
        [
            ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED,
            ERROR_INVALID_FUNCTION,
        ]
        .iter()
        .any(|&code| e.raw_os_error() == i32::try_from(code).ok())
    }

    fn volume_flags(dir: &Dir) -> io::Result<u32> {
        let mut flags = 0;
        // SAFETY: the handle is open for the call, borrowed from `dir`; `flags` is
        // writable; the name buffers are null with a size of 0, which the call
        // documents as "not requested", and so are the other out-pointers.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                dir.as_handle().as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags)
    }
}

/// One UTF-16 unit upper-cased with the OS table, the one
/// `CompareStringOrdinal(a, b, TRUE)` compares with.
#[cfg(windows)]
pub(crate) fn upcase(unit: u16) -> u16 {
    // SAFETY: takes and returns a plain value; no memory is involved.
    unsafe { windows_sys::Wdk::System::SystemServices::RtlUpcaseUnicodeChar(unit) }
}

/// Gives the empty directory `dir` the casefold attribute. Fails with
/// `EOPNOTSUPP` on a filesystem created without the casefold feature.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn set_casefold(dir: &Dir) -> io::Result<()> {
    use rustix::fs::{IFlags, ioctl_getflags, ioctl_setflags};
    let flags = ioctl_getflags(dir)?;
    ioctl_setflags(dir, flags | IFlags::from_bits_retain(imp::CASEFOLD))?;
    Ok(())
}

/// Sets or clears the case-sensitive flag of the empty directory at `path`.
/// Windows may refuse it without developer mode or the WSL feature.
#[cfg(all(test, windows))]
pub(crate) fn set_case_sensitive(path: &std::path::Path, on: bool) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FILE_WRITE_ATTRIBUTES, FileCaseSensitiveInfo,
    };
    use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
    let dir = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let info = FILE_CASE_SENSITIVE_INFO {
        Flags: if on {
            FILE_CS_FLAG_CASE_SENSITIVE_DIR
        } else {
            0
        },
    };
    super::at::set_info(dir.as_handle(), FileCaseSensitiveInfo, &info)
}

/// `CompareStringOrdinal(a, b, TRUE) == CSTR_EQUAL`, the reference the upcase
/// comparison is tested against.
#[cfg(all(test, windows))]
pub(crate) fn compare_ordinal(a: &str, b: &str) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    let (a, b): (Vec<u16>, Vec<u16>) = (a.encode_utf16().collect(), b.encode_utf16().collect());
    let (Ok(a_len), Ok(b_len)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return false;
    };
    // SAFETY: each pointer is valid for reads of the length passed with it.
    let rc = unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) };
    rc == CSTR_EQUAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::test_dir::{TestDir, case_insensitive};

    #[test]
    fn temp_dir_answers() {
        let tmp = TestDir::new();
        let sensitive = case_sensitive(&Dir::open(tmp.path()).unwrap()).unwrap();
        assert_eq!(sensitive, !case_insensitive(tmp.path()));
    }

    #[cfg(windows)]
    #[test]
    fn upcase_uses_the_os_table() {
        assert_eq!(upcase(u16::from(b'a')), u16::from(b'A'));
        assert_eq!(upcase(0x03C3), 0x03A3); // σ to Σ
        assert_eq!(upcase(u16::from(b'/')), u16::from(b'/'));
        assert_eq!(upcase(0xD83D), 0xD83D); // a surrogate is left alone
    }
}
