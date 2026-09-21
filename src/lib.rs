//! notlin library: Kotlin-to-Java transpiler.
//!
//! Exposes the transpiler as a library so integration tests and other tools
//! can use it in-process; the `notlin` binary is a thin CLI wrapper.

pub mod cli;
pub mod diagnostics;
pub mod migrate;
pub mod transpiler;
