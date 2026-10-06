//! OS boundaries shared by the supervisor and its CLI.
pub(crate) mod files;
#[cfg(windows)]
pub(crate) mod job;
#[cfg(windows)]
pub(crate) mod security;
pub(crate) mod shutdown;
