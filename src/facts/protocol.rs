//! Versioned newline-delimited JSON protocol between notlin and the JVM
//! facts helper. One request per line, one response per line. Requests are
//! answered independently.
//!
//! Two kinds of "nothing" are kept apart, because the client cannot wait for a
//! line that will never come:
//!
//! * **An answer** — always means what it says. `CallSites` with an empty batch
//!   is "looked, saw no calls".
//! * **An explicit absence** — [`Answer::NoEvidence`]: the helper was asked and
//!   has nothing for this one request. The helper stays healthy.
//!
//! An *unanswered* or invalid line is a fault, not a fact: the run fails that
//! request closed and (for a fault, not an absence) stops trusting the helper.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub enum Query {
    /// All public/protected members of a classpath type, plus its hierarchy
    /// chain availability. `owner` is a binary name (`java.math.BigDecimal`).
    ClassMembers { owner: String },
    /// Every call site the Analysis API can see in one file, in one batch.
    /// `file` is a path the helper can open: the runtime canonicalizes before
    /// asking, so the bytes the helper reads are the bytes the caller hashed.
    ResolveCalls { file: PathBuf },
    /// Capability probe so the Rust side can fail fast and log once.
    Ping,
}

/// Externally tagged like its derived form, except unit variants carry an
/// explicit `null` payload so every request line has one shape
/// (`{"Ping":null}`). The helper parses this rigidly, so the encoding is
/// pinned by `tests/facts_protocol.rs` rather than left to serde's
/// unit-variant string form.
impl Serialize for Query {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        enum Wire<'a> {
            ClassMembers { owner: &'a str },
            ResolveCalls { file: &'a Path },
            Ping(()),
        }
        match self {
            Query::ClassMembers { owner } => Wire::ClassMembers { owner },
            Query::ResolveCalls { file } => Wire::ResolveCalls { file },
            Query::Ping => Wire::Ping(()),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Members {
        owner: String,
        /// Absent means the helper could not load the type: unknown, not empty.
        members: Option<Vec<MemberFacts>>,
    },
    /// One batch per file: the helper pays FIR setup once and answers every
    /// call site in it. An empty `sites` list is a real answer ("this file
    /// calls nothing we can see"), distinct from the helper being unable to
    /// answer at all, which is the absence of this variant.
    CallSites {
        file: PathBuf,
        sites: Vec<CallSite>,
    },
    Pong {
        version: u32,
    },
    /// Explicit, *per-request* absence of evidence: the helper was asked, is
    /// healthy, and has nothing to say about this one request (it did not index
    /// that file, cannot read it, or failed to analyse it).
    ///
    /// This is deliberately not silence and not an empty batch. Silence is
    /// indistinguishable from a stalled helper — the client blocks on
    /// `read_line` — and an empty [`Answer::CallSites`] claims "looked, saw no
    /// calls", which is a different fact. Callers map this answer to `Unknown`
    /// for that request only; the helper stays trusted for every other query,
    /// and the same question may be asked again later.
    NoEvidence {
        /// The query discriminator that produced no evidence. Diagnostic and
        /// stable (`ResolveCalls`); never parsed for control flow.
        query: String,
        /// Why the helper had nothing, for logs. Diagnostic only: the truth
        /// value is "no evidence", whatever the text says.
        reason: String,
    },
}

/// How far the helper got with one call site. Serialized spellings
/// (`Established`/`Ambiguous`/`Unknown`) are pinned: the Rust side treats
/// anything else as a malformed line, never as a weaker certainty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SiteStatus {
    /// The Analysis API resolved the callee: the fields below are its answer.
    Established,
    /// Overloads the helper cannot choose between for this argument shape.
    Ambiguous,
    /// Nothing established (unresolved, dynamic dispatch, or out of scope).
    Unknown,
}

/// One call expression as the JVM-side Analysis API sees it.
///
/// Field names and order are the wire contract (`tests/fixtures/facts_callsites/`);
/// `parameter_types` are Kotlin spellings (`Int`, `kotlin.String`), while
/// `jvm_descriptor` is the erased JVM shape when the helper could produce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub byte_offset: u64,
    pub text: String,
    pub status: SiteStatus,
    pub callee_owner: Option<String>,
    pub callee_name: Option<String>,
    pub parameter_types: Vec<String>,
    pub result_type: Option<String>,
    pub result_nullable: bool,
    pub jvm_descriptor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberFacts {
    /// `<init>` for constructors.
    pub name: String,
    pub static_member: bool,
    /// Fully-erased binary parameter type names, JVM order.
    pub parameter_types: Vec<String>,
    /// Binary name for references, JVM primitive name (`int`, `long`…) for
    /// primitives; `<void>` for void.
    pub result_type: String,
    /// Nullability per parameter and result as reported by annotations or
    /// `java.util.Optional`-style heuristics the helper proves; `unknown`
    /// when nothing establishes it.
    pub parameter_nullability: Vec<String>,
    pub result_nullability: String,
}

/// Envelope so one line carries id + payload for future multiplexing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub id: u64,
    pub query: Query,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub id: u64,
    pub answer: Answer,
}

pub fn encode_request(id: u64, query: Query) -> String {
    let mut line = serde_json::to_string(&Request {
        version: PROTOCOL_VERSION,
        id,
        query,
    })
    .expect("protocol request is serializable");
    line.push('\n');
    line
}

pub fn decode_response(line: &str) -> Option<Response> {
    let parsed: Response = serde_json::from_str(line.trim()).ok()?;
    (parsed.version == PROTOCOL_VERSION).then_some(parsed)
}
