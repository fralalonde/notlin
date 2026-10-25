//! Wire-format contract for the pinned `ResolveCalls` / `CallSites` addition.
//!
//! The helper's hand-written JSON parser and these serde types must agree byte
//! for byte, so the golden line in `fixtures/facts_callsites/` is the contract
//! and not an illustration: it decodes to the pinned field names and
//! re-serializes to itself. Version stays 1 — the addition is additive, and an
//! old helper simply never emits a `CallSites` answer (callers see `None`,
//! which reads as `Unknown`).

use notlin::facts::protocol::{
    Answer, CallSite, PROTOCOL_VERSION, Query, Response, SiteStatus, decode_response,
    encode_request,
};
use std::path::PathBuf;

const GOLDEN: &str = include_str!("fixtures/facts_callsites/callsites_response.json");

const ESTABLISHED: CallSite = CallSite {
    byte_offset: 42,
    text: String::new(),
    status: SiteStatus::Established,
    callee_owner: None,
    callee_name: None,
    parameter_types: Vec::new(),
    result_type: None,
    result_nullable: false,
    jvm_descriptor: None,
};

fn established_site() -> CallSite {
    CallSite {
        text: "list.add(1)".to_string(),
        callee_owner: Some("java.util.ArrayList".to_string()),
        callee_name: Some("add".to_string()),
        parameter_types: vec!["Int".to_string()],
        result_type: Some("Boolean".to_string()),
        jvm_descriptor: Some("(I)Z".to_string()),
        ..ESTABLISHED
    }
}

fn ambiguous_site() -> CallSite {
    CallSite {
        byte_offset: 99,
        text: "list.map { it.toString() }".to_string(),
        status: SiteStatus::Ambiguous,
        callee_owner: None,
        callee_name: Some("map".to_string()),
        parameter_types: Vec::new(),
        result_type: None,
        result_nullable: true,
        jvm_descriptor: None,
    }
}

fn unknown_site() -> CallSite {
    CallSite {
        byte_offset: 140,
        text: "unknownThing()".to_string(),
        status: SiteStatus::Unknown,
        callee_owner: None,
        callee_name: None,
        parameter_types: Vec::new(),
        result_type: None,
        result_nullable: false,
        jvm_descriptor: None,
    }
}

#[test]
fn resolve_calls_request_line_is_byte_exact() {
    assert_eq!(
        encode_request(
            3,
            Query::ResolveCalls {
                file: PathBuf::from("/srv/mod/Sample.kt")
            }
        ),
        "{\"version\":1,\"id\":3,\"query\":{\"ResolveCalls\":{\"file\":\"/srv/mod/Sample.kt\"}}}\n"
    );
}

#[test]
fn resolve_calls_deserializes_from_the_same_line_it_encodes() {
    let line = encode_request(
        4,
        Query::ResolveCalls {
            file: PathBuf::from("Sample.kt"),
        },
    );
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let query: Query = serde_json::from_value(parsed["query"].clone()).unwrap();
    assert_eq!(
        query,
        Query::ResolveCalls {
            file: PathBuf::from("Sample.kt")
        }
    );
}

#[test]
fn the_golden_callsites_line_decodes_to_the_pinned_shape() {
    let response = decode_response(GOLDEN).expect("golden call sites line");
    assert_eq!(response.version, PROTOCOL_VERSION);
    assert_eq!(response.id, 0);
    let Answer::CallSites { file, sites, .. } = response.answer else {
        panic!("golden line must carry CallSites");
    };
    assert_eq!(file, PathBuf::from("/srv/mod/Sample.kt"));
    assert_eq!(
        sites,
        vec![established_site(), ambiguous_site(), unknown_site()]
    );
}

#[test]
fn call_sites_reserialize_to_the_same_golden_line() {
    let response = Response {
        version: PROTOCOL_VERSION,
        id: 0,
        answer: Answer::CallSites {
            file: PathBuf::from("/srv/mod/Sample.kt"),
            sites: vec![established_site(), ambiguous_site(), unknown_site()],
            references: None,
        },
    };
    assert_eq!(serde_json::to_string(&response).unwrap(), GOLDEN.trim());
}

#[test]
fn statuses_serialize_as_the_pinned_spellings() {
    for (status, spelling) in [
        (SiteStatus::Established, "\"Established\""),
        (SiteStatus::Ambiguous, "\"Ambiguous\""),
        (SiteStatus::Unknown, "\"Unknown\""),
    ] {
        assert_eq!(serde_json::to_string(&status).unwrap(), spelling);
        assert_eq!(
            serde_json::from_str::<SiteStatus>(spelling).unwrap(),
            status
        );
    }
}

#[test]
fn an_unpinned_status_rejects_the_whole_line() {
    let line = GOLDEN.replace("\"status\":\"Ambiguous\"", "\"status\":\"Probable\"");
    assert_ne!(line, GOLDEN);
    assert!(
        decode_response(&line).is_none(),
        "an unknown status must not decode to a made-up certainty"
    );
}

#[test]
fn a_missing_status_field_rejects_the_whole_line() {
    let line = GOLDEN.replace("\"status\":\"Established\",", "");
    assert!(decode_response(&line).is_none());
}

#[test]
fn members_answers_are_untouched_by_the_addition() {
    let line = "{\"version\":1,\"id\":5,\"answer\":{\"Members\":{\"owner\":\"com.example.Missing\",\"members\":null}}}";
    assert_eq!(
        decode_response(line).map(|response| response.answer),
        Some(Answer::Members {
            owner: "com.example.Missing".to_string(),
            members: None,
        })
    );
}

#[test]
fn an_empty_sites_batch_is_a_real_answer_not_an_unknown() {
    let line =
        "{\"version\":1,\"id\":1,\"answer\":{\"CallSites\":{\"file\":\"A.kt\",\"sites\":[]}}}";
    assert_eq!(
        decode_response(line).map(|response| response.answer),
        Some(Answer::CallSites {
            file: PathBuf::from("A.kt"),
            sites: Vec::new(),
            references: None,
        })
    );
}

#[test]
fn a_per_request_absence_is_a_typed_answer_not_an_empty_batch() {
    // Byte-exact line `Facts.noEvidenceAnswer` writes for a ResolveCalls the
    // helper cannot satisfy: an answer, so the waiting client is released, and
    // not a `CallSites` batch, which would claim "looked, saw no calls".
    const LINE: &str = "{\"version\":1,\"id\":7,\"answer\":{\"NoEvidence\":{\"query\":\"ResolveCalls\",\"reason\":\"file is not indexed by this session\"}}}";
    let answer = Answer::NoEvidence {
        query: "ResolveCalls".to_string(),
        reason: "file is not indexed by this session".to_string(),
    };
    let response = decode_response(LINE).expect("absence line");
    assert_eq!(response.version, PROTOCOL_VERSION);
    assert_eq!(response.id, 7);
    assert_eq!(response.answer, answer);
    assert_eq!(
        serde_json::to_string(&Response {
            version: PROTOCOL_VERSION,
            id: 7,
            answer,
        })
        .unwrap(),
        LINE,
        "both ends must share one shape for the absence answer"
    );
}
