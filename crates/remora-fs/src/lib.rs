//! The filesystem and OS safety module: atomic writes, owner-only creation, fsync
//! points, symlink-safe open and walk, locks, clones and SQLite connection setup.

pub mod atomic;
pub mod durable;
mod error;
pub mod nofollow;
pub mod private;

pub use error::Error;

// The only module allowed to contain `unsafe` (spec 3.5, D-06).
#[allow(unsafe_code)]
pub mod sys;
