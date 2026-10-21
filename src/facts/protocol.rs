//! Versioned newline-delimited JSON protocol between notlin and the JVM
//! facts helper. One request per line, one response per line. Requests are
//! answered independently; an unanswered/invalid line fails closed (Unknown).

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub enum Query {
    /// All public/protected members of a classpath type, plus its hierarchy
    /// chain availability. `owner` is a binary name (`java.math.BigDecimal`).
    ClassMembers { owner: String },
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
            Ping(()),
        }
        match self {
            Query::ClassMembers { owner } => Wire::ClassMembers { owner },
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
    Pong {
        version: u32,
    },
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
