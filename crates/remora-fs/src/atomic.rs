//! Temporary files and atomic writes (spec 5.3, 5.4), the single owner of the
//! atomic write (spec 4). A reader of the target sees the old file or the new one,
//! never a partial one, and the new one survives a power loss once the call returns.

use crate::Error;
use crate::durable::{self, Point};
use crate::private::create_private_file;
use crate::sys::{Dir, at, check_name, random};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};

/// Random names drawn for one temporary file before giving up (spec 5.3 rule 1).
const NAME_ATTEMPTS: usize = 8;

/// A temporary file `.<purpose>.<16 lowercase hex>.tmp`, created exclusively and
/// owner-only, and removed on drop unless it was renamed (spec 5.3 rule 2).
#[derive(Debug)]
pub struct Temp<'d> {
    dir: &'d Dir,
    name: OsString,
    file: File,
    armed: bool,
}

impl<'d> Temp<'d> {
    /// Creates a temporary file in `dir`. A `purpose` outside `[a-z0-9-]{1,32}` is
    /// `InvalidInput`. An existing name draws a new one; after 8 names the call
    /// fails with `AlreadyExists` and creates nothing.
    pub fn create(dir: &'d Dir, purpose: &str) -> io::Result<Self> {
        Self::create_with(dir, purpose, || random::hex(8))
    }

    /// [`Temp::create`] with the random part of each name from `random`.
    fn create_with(
        dir: &'d Dir,
        purpose: &str,
        mut random: impl FnMut() -> io::Result<String>,
    ) -> io::Result<Self> {
        if !valid_purpose(purpose) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("temporary file purpose {purpose:?} is not [a-z0-9-]{{1,32}}"),
            ));
        }
        for _ in 0..NAME_ATTEMPTS {
            let name = OsString::from(format!(".{purpose}.{}.tmp", random()?));
            match create_private_file(dir, &name) {
                Ok(file) => {
                    return Ok(Self {
                        dir,
                        name,
                        file,
                        armed: true,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{NAME_ATTEMPTS} temporary names in a row already exist"),
        ))
    }

    /// The open file, for writing and flushing.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// The temporary file's name in its directory.
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Renames the temporary file to `to` in `to_dir`, replacing a file there. On
    /// failure the temporary file is removed.
    pub fn rename(mut self, to_dir: &Dir, to: &OsStr) -> Result<(), Error> {
        at::rename_at(self.dir, &self.name, to_dir, to)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Temp<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = at::unlink_at(self.dir, &self.name, false);
        }
    }
}

fn valid_purpose(purpose: &str) -> bool {
    (1..=32).contains(&purpose.len())
        && purpose
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Replaces `name` in `dir` with a file holding `bytes` (spec 5.4): a temporary
/// file is written, given `mode` (Unix; ignored on Windows), flushed, renamed over
/// `name`, and `dir` is flushed. `point` is the durability point of the caller,
/// F3 for a manifest and F4 for a state file. On failure no temporary file
/// remains; `name` is untouched unless the failure is the final directory flush.
pub fn write_atomic(
    dir: &Dir,
    name: &OsStr,
    bytes: &[u8],
    mode: u32,
    purpose: &str,
    point: Point,
) -> Result<(), Error> {
    check_name(name)?;
    let temp = Temp::create(dir, purpose)?;
    (&temp.file).write_all(bytes)?;
    at::set_mode(&temp.file, mode)?;
    durable::flush_file(&temp.file, point)?;
    temp.rename(dir, name)?;
    durable::flush_dir(dir, point)?;
    Ok(())
}

/// Creates `name` in `dir` holding `bytes`, owner-only, unless `name` exists
/// (spec 5.4): the temporary file is renamed without replacing. An existing `name`
/// fails with `AlreadyExists` and stays as it was; the caller reads it. A
/// filesystem with neither a no-replace rename nor hard links fails with
/// `Unsupported`. `point` is F5 for the machine key.
pub fn create_exclusive_atomic(
    dir: &Dir,
    name: &OsStr,
    bytes: &[u8],
    purpose: &str,
    point: Point,
) -> Result<(), Error> {
    create_exclusive_with(dir, name, bytes, purpose, point, Strategy::Rename)
}

/// How [`create_exclusive_with`] puts the temporary file in place. `Link` skips
/// the rename as if the filesystem did not support it, for tests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strategy {
    Rename,
    #[cfg_attr(not(test), expect(dead_code, reason = "selected by tests"))]
    Link,
}

fn create_exclusive_with(
    dir: &Dir,
    name: &OsStr,
    bytes: &[u8],
    purpose: &str,
    point: Point,
    strategy: Strategy,
) -> Result<(), Error> {
    check_name(name)?;
    let mut temp = Temp::create(dir, purpose)?;
    (&temp.file).write_all(bytes)?;
    durable::flush_file(&temp.file, point)?;
    let renamed = match strategy {
        Strategy::Rename => at::rename_no_replace(dir, &temp.name, dir, name),
        Strategy::Link => Err(io::Error::from(io::ErrorKind::Unsupported).into()),
    };
    match renamed {
        Ok(()) => temp.armed = false,
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::Unsupported => {
            at::link_at(dir, &temp.name, dir, name)?;
            at::unlink_at(dir, &temp.name, false)?;
            temp.armed = false;
        }
        Err(e) => return Err(e),
    }
    durable::flush_dir(dir, point)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::test_dir::TestDir;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    /// The names in `dir`, sorted.
    fn listing(tmp: &TestDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn is_temporary_name(s: &str) -> bool {
        let Some(rest) = s.strip_prefix('.').and_then(|r| r.strip_suffix(".tmp")) else {
            return false;
        };
        let Some((purpose, random)) = rest.rsplit_once('.') else {
            return false;
        };
        valid_purpose(purpose)
            && random.len() == 16
            && random
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn temporary_name_format() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        let temp = Temp::create(&dir, "project").unwrap();
        let temp_name = temp.name().to_str().unwrap().to_owned();
        assert!(is_temporary_name(&temp_name), "{temp_name}");
        assert!(temp_name.starts_with(".project."));
        assert_eq!(listing(&tmp), [temp_name]);
        drop(temp);
        assert!(listing(&tmp).is_empty());
    }

    #[test]
    fn invalid_purpose_is_refused() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        let long = "a".repeat(33);
        for purpose in ["", "Project", "a.b", "a/b", long.as_str()] {
            let err = Temp::create(&dir, purpose).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{purpose:?}");
        }
        assert!(listing(&tmp).is_empty());
    }

    #[test]
    fn eight_collisions_fail() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        let taken = ".project.0000000000000000.tmp";
        std::fs::write(tmp.path().join(taken), b"").unwrap();
        let mut draws = 0;
        let err = Temp::create_with(&dir, "project", || {
            draws += 1;
            Ok("0000000000000000".to_owned())
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(draws, NAME_ATTEMPTS);
        assert_eq!(listing(&tmp), [taken]);
    }

    #[test]
    fn write_atomic_replaces_target() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("project.json"), b"old").unwrap();
        write_atomic(
            &dir,
            name("project.json"),
            b"new",
            0o600,
            "project",
            Point::F4,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("project.json")).unwrap(),
            b"new"
        );
        assert_eq!(listing(&tmp), ["project.json"]);
    }

    #[test]
    fn write_atomic_failure_leaves_no_temporary() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        tmp.mkdir_all("project.json");
        std::fs::write(tmp.path().join("project.json/inside"), b"kept").unwrap();
        assert!(
            write_atomic(
                &dir,
                name("project.json"),
                b"new",
                0o600,
                "project",
                Point::F4
            )
            .is_err()
        );
        assert_eq!(listing(&tmp), ["project.json"]);
        assert_eq!(
            std::fs::read(tmp.path().join("project.json/inside")).unwrap(),
            b"kept"
        );
    }

    #[test]
    fn reader_never_sees_partial_file() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        let short = vec![b'a'; 100];
        let long = vec![b'b'; 64 * 1024];
        write_atomic(&dir, name("state"), &short, 0o600, "state", Point::F4).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        // The reader is running before the first rewrite and reads once more after
        // the last one, so every rewrite happens while it reads.
        let started = Arc::new(Barrier::new(2));
        let reader = {
            let (path, done) = (tmp.path().join("state"), Arc::clone(&done));
            let (short, long) = (short.clone(), long.clone());
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                started.wait();
                loop {
                    let finished = done.load(Ordering::Acquire);
                    let bytes = std::fs::read(&path).unwrap();
                    assert!(
                        bytes == short || bytes == long,
                        "read {} bytes",
                        bytes.len()
                    );
                    if finished {
                        break;
                    }
                }
            })
        };
        started.wait();
        for i in 0..1000 {
            let bytes = if i % 2 == 0 { &long } else { &short };
            write_atomic(&dir, name("state"), bytes, 0o600, "state", Point::F4).unwrap();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
        assert_eq!(listing(&tmp), ["state"]);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_sets_mode_before_rename() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        // The temporary file is created 0o600, so 0o644 can only come from the
        // descriptor, before the rename made the name visible.
        write_atomic(&dir, name("unit"), b"[Unit]", 0o644, "unit", Point::F4).unwrap();
        let mode = std::fs::metadata(tmp.path().join("unit"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn create_exclusive_atomic_existing_target_untouched() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("machine.key"), b"first").unwrap();
        let err = create_exclusive_atomic(
            &dir,
            name("machine.key"),
            b"second",
            "machine-key",
            Point::F5,
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == io::ErrorKind::AlreadyExists),
            "{err}"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("machine.key")).unwrap(),
            b"first"
        );
        assert_eq!(listing(&tmp), ["machine.key"]);
    }

    /// 8 threads create the same name: one wins, seven see `AlreadyExists`, and the
    /// file holds the winner's bytes.
    fn race(strategy: Strategy) {
        let tmp = TestDir::new();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dir = Dir::open(tmp.path()).unwrap();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let bytes = format!("thread {i}");
                    barrier.wait();
                    let result = create_exclusive_with(
                        &dir,
                        name("machine.key"),
                        bytes.as_bytes(),
                        "machine-key",
                        Point::F5,
                        strategy,
                    );
                    (bytes, result)
                })
            })
            .collect();
        let mut winners = Vec::new();
        for thread in threads {
            match thread.join().unwrap() {
                (bytes, Ok(())) => winners.push(bytes),
                (_, Err(Error::Io(e))) if e.kind() == io::ErrorKind::AlreadyExists => {}
                (_, Err(e)) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(winners.len(), 1);
        let stored = std::fs::read(tmp.path().join("machine.key")).unwrap();
        assert_eq!(stored, winners[0].as_bytes());
        assert_eq!(listing(&tmp), ["machine.key"]);
    }

    #[test]
    fn create_exclusive_atomic_race_8_threads() {
        race(Strategy::Rename);
    }

    #[cfg(unix)]
    #[test]
    fn link_fallback_matches_rename() {
        race(Strategy::Link);
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("machine.key"), b"first").unwrap();
        let err = create_exclusive_with(
            &dir,
            name("machine.key"),
            b"second",
            "machine-key",
            Point::F5,
            Strategy::Link,
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == io::ErrorKind::AlreadyExists),
            "{err}"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("machine.key")).unwrap(),
            b"first"
        );
        assert_eq!(listing(&tmp), ["machine.key"]);
    }

    #[cfg(windows)]
    #[test]
    fn link_fallback_is_unsupported_on_windows() {
        let tmp = TestDir::new();
        let dir = Dir::open(tmp.path()).unwrap();
        let err = create_exclusive_with(
            &dir,
            name("machine.key"),
            b"key",
            "machine-key",
            Point::F5,
            Strategy::Link,
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == io::ErrorKind::Unsupported),
            "{err}"
        );
        assert!(listing(&tmp).is_empty());
    }
}
