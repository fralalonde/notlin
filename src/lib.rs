//! notlin library: Kotlin-to-Java transpiler.
//!
//! Exposes the transpiler as a library so integration tests and other tools
//! can use it in-process; the `notlin` binary is a thin CLI wrapper.

pub mod cli;
pub mod ctor_defaults;
pub mod diagnostics;
pub mod function_callsite;
pub mod java_ir;
pub mod jvm_validation;
pub mod manual_marks;
pub mod migrate;
pub mod migration_pipeline;
pub mod paths;
pub mod planning;
pub mod property_abi;
pub mod property_callsite;
pub mod retention_docs;
pub mod semantics;
pub mod smart_cast;
pub mod translation_plan;
pub mod transpiler;
pub mod workspace;
