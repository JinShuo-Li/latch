//! CLI-side concerns: the command surface, the shared session builder, the
//! machine-oriented run/resume implementation, structured output, and
//! read-only session inspection.
//!
//! Everything here stays in `latch-cli`; the kernel is untouched.

pub mod command;
pub mod discovery;
pub mod doctor;
pub mod machine;
pub mod output;
pub mod session;
pub mod sessions;
