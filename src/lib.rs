//! computer-block: keeps one Windows account looking the way it was set up.
//!
//! * [`core`]: the platform-independent state machine (no `unsafe`, no I/O),
//!   verified against the TLA+ model in `spec/`.
//! * `win`: the Windows runtime that feeds it events and executes its
//!   commands. All `unsafe` code in the project lives there.

pub mod core;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win;
