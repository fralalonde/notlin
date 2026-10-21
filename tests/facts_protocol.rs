//! Wire-format contract for the JVM facts helper protocol. These literal
//! strings are the contract: the helper's hand-written JSON parser and the
//! Rust serde types must agree byte for byte.

use notlin::facts::protocol::{
    Answer, MemberFacts, Query, Response, decode_response, encode_request,
};

#[test]
fn ping_request_line_is_byte_exact() {
    assert_eq!(
        encode_request(1, Query::Ping),
        "{\"version\":1,\"id\":1,\"query\":{\"Ping\":null}}\n"
    );
}

#[test]
fn class_members_request_line_is_byte_exact() {
    assert_eq!(
        encode_request(
            7,
            Query::ClassMembers {
                owner: "java.math.BigDecimal".to_string()
            }
        ),
        "{\"version\":1,\"id\":7,\"query\":{\"ClassMembers\":{\"owner\":\"java.math.BigDecimal\"}}}\n"
    );
}

#[test]
fn query_deserializes_from_the_same_lines_it_encodes() {
    let line = encode_request(1, Query::Ping);
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let query: Query = serde_json::from_value(parsed["query"].clone()).unwrap();
    assert_eq!(query, Query::Ping);

    let line = encode_request(
        2,
        Query::ClassMembers {
            owner: "java.util.UUID".to_string(),
        },
    );
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let query: Query = serde_json::from_value(parsed["query"].clone()).unwrap();
    assert_eq!(
        query,
        Query::ClassMembers {
            owner: "java.util.UUID".to_string()
        }
    );
}

#[test]
fn decode_accepts_a_pong_line() {
    let response =
        decode_response("{\"version\":1,\"id\":4,\"answer\":{\"Pong\":{\"version\":1}}}")
            .expect("valid pong line");
    assert_eq!(response.version, 1);
    assert_eq!(response.id, 4);
    assert_eq!(response.answer, Answer::Pong { version: 1 });
}

#[test]
fn decode_accepts_a_members_line_and_carries_unknown_as_none() {
    let response = decode_response(
        "{\"version\":1,\"id\":5,\"answer\":{\"Members\":{\"owner\":\"com.example.Missing\",\"members\":null}}}",
    )
    .expect("valid members line");
    assert_eq!(
        response.answer,
        Answer::Members {
            owner: "com.example.Missing".to_string(),
            members: None
        }
    );

    let response = decode_response(
        "{\"version\":1,\"id\":6,\"answer\":{\"Members\":{\"owner\":\"java.math.BigDecimal\",\"members\":[{\
         \"name\":\"add\",\"static_member\":false,\"parameter_types\":[\"java.math.BigDecimal\"],\
         \"result_type\":\"java.math.BigDecimal\",\"parameter_nullability\":[\"unknown\"],\
         \"result_nullability\":\"unknown\"}]}}}",
    )
    .expect("valid members line");
    assert_eq!(
        response.answer,
        Answer::Members {
            owner: "java.math.BigDecimal".to_string(),
            members: Some(vec![MemberFacts {
                name: "add".to_string(),
                static_member: false,
                parameter_types: vec!["java.math.BigDecimal".to_string()],
                result_type: "java.math.BigDecimal".to_string(),
                parameter_nullability: vec!["unknown".to_string()],
                result_nullability: "unknown".to_string(),
            }]),
        }
    );
}

#[test]
fn decode_rejects_a_version_mismatch() {
    assert!(
        decode_response("{\"version\":999,\"id\":1,\"answer\":{\"Pong\":{\"version\":1}}}")
            .is_none()
    );
}

#[test]
fn decode_rejects_garbage() {
    for line in ["", "not json", "{\"version\":1}", "[]"] {
        assert!(decode_response(line).is_none(), "accepted {line:?}");
    }
}

#[test]
fn response_round_trips_through_the_envelope() {
    let response = Response {
        version: notlin::facts::protocol::PROTOCOL_VERSION,
        id: 9,
        answer: Answer::Pong { version: 1 },
    };
    let line = serde_json::to_string(&response).unwrap();
    assert_eq!(decode_response(&line), Some(response));
}
