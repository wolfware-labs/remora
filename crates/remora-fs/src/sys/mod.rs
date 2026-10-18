//! System calls behind safe functions, and the only place in the workspace where
//! `unsafe` is allowed (spec 3.5, D-06).
//!
//! Rules for every submodule:
//!
//! 1. Built on `rustix` (Unix) and `windows-sys` (Windows). `libc` only for the macOS
//!    calls spec 3.5 lists.
//! 2. Every `unsafe` block carries a `// SAFETY:` comment stating the invariant it
//!    relies on; clippy denies one without it.
//! 3. Every function has its own unit tests on each OS where it compiles.
//! 4. No raw pointer, raw descriptor or `HANDLE` crosses the module boundary: the API
//!    takes and returns plain Rust types.

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("remora-fs supports Linux, macOS and Windows only");

pub(crate) mod at;
mod create;
mod dir;
mod final_path;
mod name;
#[cfg(windows)]
mod nt;
pub mod random;
#[cfg(windows)]
pub(crate) mod security;
#[cfg(unix)]
mod umask;

pub(crate) use create::{create_dir, create_file};
pub use dir::Dir;
pub(crate) use final_path::final_path;
pub(crate) use name::{check_name, name_from_bytes};
#[cfg(unix)]
pub(crate) use umask::ScopedUmask;

#[cfg(test)]
pub(crate) mod test_dir {
    use std::path::{Path, PathBuf};

    /// A fresh directory under `REMORA_TEST_FS`, or the system temporary directory
    /// when it is unset, removed on drop.
    pub(crate) struct TestDir(PathBuf);

    impl TestDir {
        pub(crate) fn new() -> Self {
            let root =
                std::env::var_os("REMORA_TEST_FS").map_or_else(std::env::temp_dir, PathBuf::from);
            let path = root.join(format!("remora-fs-{}", super::random::hex(8).unwrap()));
            let _lock = crate::private::creation_lock();
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }

        /// Creates `rel` and its missing parents. Holds the creation lock, so a test
        /// that narrows the umask cannot leave the directories untraversable.
        pub(crate) fn mkdir_all(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            let _lock = crate::private::creation_lock();
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    /// Creates a symlink to the directory `target`. Windows needs the symlink
    /// privilege or developer mode.
    pub(crate) fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        return std::os::windows::fs::symlink_dir(target, link);
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
