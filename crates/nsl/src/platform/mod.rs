//! Cross-platform process and filesystem primitives.
//!
//! On Unix, daemonization uses `fork + setsid` via the `daemonize` crate, and
//! process liveness/termination uses POSIX signals via `nix`.
//!
//! On Windows, there is no fork: the parent spawns a self-copy with
//! `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` creation
//! flags, and the detached child writes its own PID file and serves the
//! proxy. Liveness checks go through `OpenProcess + GetExitCodeProcess`, and
//! termination through `TerminateProcess`.

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;
