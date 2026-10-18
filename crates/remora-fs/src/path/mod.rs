//! Path canonicalization (spec 5.6): one spelling for every path that is compared,
//! hashed or recorded (D-10), and the case-folding comparison of names on the
//! volume that holds them.

mod case;
mod fold_table;

pub use case::{CaseRule, case_rule, eq_on, fold_key};

use crate::sys;
use camino::Utf8PathBuf;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

/// The canonical spelling of `path`: symlinks resolved, the on-disk case of every
/// existing component, and on Windows a drive path without `\\?\`, an upper-case
/// drive letter, `\\server\share` for UNC paths and `subst` drives replaced by
/// their target.
///
/// `path` must be absolute (`InvalidInput` otherwise). When it does not exist, its
/// deepest existing ancestor is canonicalized and the missing components are
/// appended unchanged, except that `.` is dropped and `..` is refused with
/// `InvalidInput`. On Windows, Win32 resolves `..` by name before the open, so a
/// missing component followed by `..` is never reached; in a verbatim path
/// (`\\?\`), which Win32 passes on as is, `.` is dropped and `..` refused
/// anywhere. An existing ancestor that is not a directory fails with
/// `NotADirectory`.
///
/// On macOS, a regular file that cannot be opened read-only is spelled as its
/// canonical parent plus the file name as given.
pub fn canonical(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a canonical path needs an absolute input",
        ));
    }
    let mut existing = without_verbatim_dots(path)?;
    // The missing components, last first.
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        let not_found = match sys::final_path(&existing) {
            Ok(found) => {
                if !missing.is_empty() && !found.is_dir {
                    return Err(io::ErrorKind::NotADirectory.into());
                }
                let mut out = found.path;
                out.extend(missing.iter().rev());
                return Ok(out);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => e,
            #[cfg(target_os = "macos")]
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && missing.is_empty() => {
                return unreadable(&existing, e);
            }
            Err(e) => return Err(e),
        };
        match existing.components().next_back() {
            Some(Component::Normal(name)) => missing.push(name.to_os_string()),
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => return Err(dotdot_refused()),
            // The root or the prefix itself does not exist.
            Some(Component::RootDir | Component::Prefix(_)) | None => return Err(not_found),
        }
        existing.pop();
    }
}

/// A verbatim Windows path (`\\?\`) is passed to the filesystem as is, so `.` and
/// `..` would reach it as names: `.` is dropped and `..` refused, as in a missing
/// tail. Other paths are returned unchanged.
fn without_verbatim_dots(path: &Path) -> io::Result<PathBuf> {
    let verbatim = matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if p.kind().is_verbatim()
    );
    if !verbatim {
        return Ok(path.to_path_buf());
    }
    let mut out = PathBuf::new();
    for c in path.components().filter(|c| !is_dot(*c)) {
        if is_dotdot(c) {
            return Err(dotdot_refused());
        }
        out.push(c);
    }
    Ok(out)
}

// `Path::components` can report `.` and `..` in a verbatim path as normal names,
// so both forms count.
fn is_dot(c: Component<'_>) -> bool {
    matches!(c, Component::CurDir) || matches!(c, Component::Normal(n) if n == ".")
}

fn is_dotdot(c: Component<'_>) -> bool {
    matches!(c, Component::ParentDir) || matches!(c, Component::Normal(n) if n == "..")
}

fn dotdot_refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "`..` after a component that does not exist, or in a verbatim path",
    )
}

/// A regular file that exists but cannot be opened read-only: its canonical
/// parent plus its name as given. A directory or a symlink keeps the error, since
/// its name alone is not its canonical spelling.
#[cfg(target_os = "macos")]
fn unreadable(path: &Path, err: io::Error) -> io::Result<PathBuf> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(err);
    };
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file()) {
        return Err(err);
    }
    let found = sys::final_path(parent)?;
    if !found.is_dir {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    Ok(found.path.join(name))
}

/// `canonical`, for a path that must be valid UTF-8: a root, a data directory or a
/// blob home (spec 5.6 rule 5).
pub fn canonical_utf8(path: &Path) -> Result<Utf8PathBuf, CanonicalError> {
    Utf8PathBuf::from_path_buf(canonical(path)?).map_err(CanonicalError::NotUtf8)
}

/// Why `canonical_utf8` failed.
#[derive(Debug)]
pub enum CanonicalError {
    /// The canonical path is not valid UTF-8. Holds that path.
    NotUtf8(PathBuf),
    /// Canonicalization failed.
    Io(io::Error),
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8(path) => write!(f, "not valid UTF-8: {}", path.display()),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CanonicalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::NotUtf8(_) => None,
        }
    }
}

impl From<io::Error> for CanonicalError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::test_dir::{TestDir, symlink_dir};

    #[test]
    fn relative_path_refused() {
        let err = canonical(Path::new("relative/dir")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }

    #[test]
    fn symlinked_dir_resolves() {
        let tmp = TestDir::new();
        let real = tmp.mkdir_all("real/sub");
        match symlink_dir(&tmp.path().join("real"), &tmp.path().join("link")) {
            Ok(()) => {}
            Err(e) if cfg!(windows) => {
                eprintln!("skipped: cannot create a symlink: {e}");
                return;
            }
            Err(e) => panic!("{e}"),
        }
        let via = canonical(&tmp.path().join("link").join("sub")).unwrap();
        assert_eq!(via, canonical(&real).unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_is_private_var() {
        let out = canonical(Path::new("/var/tmp")).unwrap();
        assert_eq!(out, Path::new("/private/var/tmp"));
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn wrong_case_returns_on_disk_case() {
        let tmp = TestDir::new();
        if !crate::sys::test_dir::case_insensitive(tmp.path()) {
            eprintln!("skipped: case-sensitive volume");
            return;
        }
        let real = tmp.mkdir_all("MixedCase/Sub");
        std::fs::write(real.join("File.txt"), b"x").unwrap();
        let expected = canonical(&real.join("File.txt")).unwrap();
        assert!(
            expected.ends_with("MixedCase/Sub/File.txt"),
            "{}",
            expected.display()
        );
        let wrong = tmp.path().join("mixedcase").join("SUB").join("file.TXT");
        assert_eq!(canonical(&wrong).unwrap(), expected);
    }

    #[test]
    fn missing_tail_appended_unchanged() {
        let tmp = TestDir::new();
        let out = canonical(&tmp.path().join("Missing").join("Tail")).unwrap();
        assert_eq!(
            out,
            canonical(tmp.path()).unwrap().join("Missing").join("Tail")
        );
    }

    #[test]
    fn dot_in_missing_tail_dropped() {
        let tmp = TestDir::new();
        let out = canonical(&tmp.path().join("x").join(".").join("y")).unwrap();
        assert_eq!(out, canonical(tmp.path()).unwrap().join("x").join("y"));
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_in_missing_tail_refused() {
        let tmp = TestDir::new();
        let err = canonical(&tmp.path().join("x").join("..").join("y")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn dotdot_resolved_by_name_on_windows() {
        let tmp = TestDir::new();
        let out = canonical(&tmp.path().join("x").join("..").join("y")).unwrap();
        assert_eq!(out, canonical(tmp.path()).unwrap().join("y"));
    }

    #[test]
    fn dotdot_in_existing_part_resolved() {
        let tmp = TestDir::new();
        tmp.mkdir_all("a/b");
        let out = canonical(&tmp.path().join("a").join("b").join("..").join("new")).unwrap();
        assert_eq!(out, canonical(&tmp.path().join("a")).unwrap().join("new"));
    }

    #[test]
    fn file_in_middle_is_error() {
        let tmp = TestDir::new();
        std::fs::write(tmp.path().join("file"), b"x").unwrap();
        let err = canonical(&tmp.path().join("file").join("b")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{err}");
    }

    #[test]
    fn same_directory_two_spellings_equal() {
        let tmp = TestDir::new();
        tmp.mkdir_all("a/b");
        let plain = canonical(&tmp.path().join("a")).unwrap();
        let dotted = canonical(&tmp.path().join("a").join(".").join("b").join("..")).unwrap();
        assert_eq!(
            plain.as_os_str().as_encoded_bytes(),
            dotted.as_os_str().as_encoded_bytes()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unreadable_file_uses_parent_and_name() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TestDir::new();
        let file = tmp.path().join("locked");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let out = canonical(&file).unwrap();
        assert_eq!(out, canonical(tmp.path()).unwrap().join("locked"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unreadable_directory_and_link_are_errors() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TestDir::new();
        let dir = tmp.mkdir_all("locked");
        std::fs::write(tmp.path().join("file"), b"x").unwrap();
        std::fs::set_permissions(
            tmp.path().join("file"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("file"), tmp.path().join("link")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o311)).unwrap();
        let dir_err = canonical(&dir).map(drop);
        let link_err = canonical(&tmp.path().join("link")).map(drop);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root opens anything, so only an unprivileged run sees the errors.
        if rustix::process::geteuid().is_root() {
            return;
        }
        for err in [dir_err, link_err] {
            let err = err.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_name_detected() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = TestDir::new();
        let dir = tmp.path().join(std::ffi::OsStr::from_bytes(b"bad\xff"));
        if let Err(e) = std::fs::create_dir(&dir) {
            eprintln!("skipped: the filesystem refuses a non-UTF-8 name: {e}");
            return;
        }
        match canonical_utf8(&dir) {
            Err(CanonicalError::NotUtf8(path)) => assert_eq!(path, canonical(&dir).unwrap()),
            other => panic!("expected NotUtf8, got {other:?}"),
        }
    }

    #[test]
    fn utf8_path_accepted() {
        let tmp = TestDir::new();
        let out = canonical_utf8(tmp.path()).unwrap();
        assert_eq!(out.as_std_path(), canonical(tmp.path()).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefix_stripped_and_drive_upper() {
        let tmp = TestDir::new();
        let text = tmp.path().to_str().unwrap();
        let lower = format!("{}{}", text[..1].to_ascii_lowercase(), &text[1..]);
        let out = canonical(Path::new(&lower)).unwrap();
        let out = out.to_str().unwrap();
        assert!(!out.starts_with(r"\\?\"), "{out}");
        assert!(
            out.as_bytes()[0].is_ascii_uppercase() && &out[1..3] == r":\",
            "{out}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_dot_components_handled() {
        let tmp = TestDir::new();
        let base = canonical(tmp.path()).unwrap();
        let verbatim = format!(r"\\?\{}", base.display());
        let dot = canonical(Path::new(&format!(r"{verbatim}\x\.\y"))).unwrap();
        assert_eq!(dot, base.join("x").join("y"));
        let err = canonical(Path::new(&format!(r"{verbatim}\x\..\y"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_form() {
        let tmp = TestDir::new();
        let text = tmp.path().to_str().unwrap();
        let unc = format!(r"\\localhost\{}$\{}", &text[..1], &text[3..]);
        let out = match canonical(Path::new(&unc)) {
            Ok(out) => out,
            Err(e) => {
                eprintln!("skipped: the administrative share is not reachable: {e}");
                return;
            }
        };
        let out = out.to_str().unwrap();
        assert!(
            out.to_ascii_lowercase().starts_with(r"\\localhost\"),
            "{out}"
        );
    }
}
