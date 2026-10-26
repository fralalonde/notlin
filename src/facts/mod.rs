//! Facts sourced from outside the source index (currently the JVM helper).
//!
//! Everything here is optional: without a helper every consumer keeps the
//! `Unknown` answers the syntax provider gives, which is today's behavior.

pub mod protocol;

pub mod classpath;

pub mod client;

pub mod decorator;

pub mod runtime;

use crate::semantics::{SymbolId, TypeRef};
use protocol::CallSite;
use std::path::Path;

/// Module identity of a symbol that stands for an *external fact* rather than
/// an indexed declaration. The run's own module is the empty string today, so
/// this can never equal a declaration's identity; downstream code that needs
/// "is this a real source declaration?" asks [`is_fact_symbol`].
pub const FACT_MODULE: &str = "jvm-facts";

/// The identity notlin records for a Track B call-site fact.
///
/// A helper-established callee has no source declaration in this workspace
/// (a stdlib extension such as `let`, a library method, a constructor of an
/// external class), so the `SymbolId` is a *fact* identity, not a location:
///
/// * `module` is [`FACT_MODULE`], which no indexed declaration can carry;
/// * `file` is the file the fact was observed in (the call site), because that
///   is the only real path involved;
/// * `package` / `owner_path` split the helper's `callee_owner` at its last
///   dot. The helper reports a K2 `ClassId` (`kotlin.collections.MutableList`)
///   or, for top-level functions, the JVM facade class (`kotlin.StandardKt`);
///   either way the last segment names the owner the callee belongs to.
/// * `parameters` keeps the helper's K2 renderings (`kotlin.Int`,
///   `kotlin.String?`) verbatim: they are already Kotlin spellings, the shape
///   [`TypeRef::parse`] accepts, and re-rendering them would lose information
///   the wire carried.
///
/// Nothing answers *declaration* questions for this identity: the decorator
/// forwards every declaration-backed accessor to the syntax index, which is
/// silent for a fact id. The helper's K2 result type is Kotlin's view of a call
/// expression — not a declaration's return type, not a natively nongeneric
/// signature and not the Java type of the text this crate emits — so no
/// declaration-backed accessor may answer it, and it is not reachable through
/// the nongeneric-declaration API at all.
///
/// `None` when the helper reported no callee name — a status the decorator
/// never hands out, guarded here so a malformed line cannot mint an identity.
pub fn call_site_symbol(file: &Path, site: &CallSite) -> Option<SymbolId> {
    let name = site.callee_name.as_ref()?.trim();
    if name.is_empty() {
        return None;
    }
    let (package, owner_path) = match site.callee_owner.as_deref().map(str::trim) {
        Some(owner) if !owner.is_empty() => match owner.rsplit_once('.') {
            Some((package, last)) => (package.to_string(), vec![last.to_string()]),
            None => (String::new(), vec![owner.to_string()]),
        },
        _ => (String::new(), Vec::new()),
    };
    Some(SymbolId {
        module: FACT_MODULE.to_string(),
        package,
        file: file.to_path_buf(),
        owner_path,
        kind: if name == "<init>" {
            "constructor".to_string()
        } else {
            "function".to_string()
        },
        name: name.to_string(),
        receiver: None,
        parameters: site.parameter_types.clone(),
    })
}

/// Whether this identity names an external fact rather than a declaration the
/// source index owns. Used to keep rules phrased about *user declarations*
/// (builtin shadowing, ownership) from firing on a helper fact.
pub fn is_fact_symbol(id: &SymbolId) -> bool {
    id.module == FACT_MODULE
}

/// A helper result type in notlin's own type model, or `None` when the
/// rendering does not parse. `result_nullable` is the wire's only nullability
/// signal, so it is folded into the `TypeRef` here rather than inferred from
/// the spelling.
pub fn call_site_result_type(site: &CallSite) -> Option<TypeRef> {
    let rendered = site.result_type.as_deref()?.trim();
    if rendered.is_empty() || rendered == "<void>" {
        return None;
    }
    let mut ty = TypeRef::parse(rendered);
    ty.nullable |= site.result_nullable;
    Some(ty)
}
