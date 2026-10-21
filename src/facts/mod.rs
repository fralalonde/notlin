//! Facts sourced from outside the source index (currently the JVM helper).
//!
//! Everything here is optional: without a helper every consumer keeps the
//! `Unknown` answers the syntax provider gives, which is today's behavior.

pub mod protocol;

pub mod classpath;

pub mod client;

pub mod decorator;

pub mod runtime;
