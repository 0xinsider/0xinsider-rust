//! Every example the OpenAPI document publishes must decode into the type this
//! crate generates for it: 2xx response bodies, request bodies, and error
//! envelopes. A contract change that breaks a type fails here, not in a caller.

mod support;

use serde_json::Value;

fn document() -> Value {
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json")).expect("openapi.json is committed");
    serde_json::from_slice(&raw).expect("openapi.json is JSON")
}

/// `(label, value)` for every example under a media-type object.
fn media_examples(label: &str, media: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    if let Some(example) = media.get("example") {
        out.push((format!("{label} example"), example.clone()));
    }
    if let Some(examples) = media.get("examples").and_then(Value::as_object) {
        for (name, example) in examples {
            if let Some(value) = example.get("value") {
                out.push((format!("{label} examples.{name}"), value.clone()));
            }
        }
    }
    out
}

fn operations(doc: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for item in doc["paths"].as_object().expect("paths").values() {
        for method in ["get", "post", "put", "patch", "delete"] {
            if let Some(operation) = item.get(method) {
                out.push((
                    operation["operationId"].as_str().expect("operationId").to_owned(),
                    operation.clone(),
                ));
            }
        }
    }
    out
}

#[test]
fn every_documented_success_example_decodes() {
    let doc = document();
    let mut checked = 0;
    let mut failures = Vec::new();
    for (id, operation) in operations(&doc) {
        let mut examples = Vec::new();
        for (code, response) in operation["responses"].as_object().expect("responses") {
            if code.starts_with('2') {
                if let Some(media) = response.pointer("/content/application~1json") {
                    examples.extend(media_examples(&format!("{id} {code}"), media));
                }
            }
        }
        for (label, value) in examples {
            match support::generated::decode_response(&id, value) {
                Some(Ok(())) => checked += 1,
                Some(Err(error)) => failures.push(format!("{label}: {error}")),
                None => failures.push(format!("{label}: no generated response type for {id}")),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} examples failed:\n{}",
        failures.len(),
        checked + failures.len(),
        failures.join("\n")
    );
    assert!(
        checked >= 50,
        "only {checked} success examples were checked; the document publishes more"
    );
}

/// `x-examples` are documentation stubs, and some predate the schema they sit
/// beside; the app repository tracks their correction (0xinsider#16139) and its
/// own sandbox check reports them without failing. This does the same: it
/// prints each one that does not decode, so a regenerate PR shows them.
#[test]
fn x_examples_are_reported_not_enforced() {
    let doc = document();
    let mut stale = Vec::new();
    for (id, operation) in operations(&doc) {
        if let Some(extra) = operation.get("x-examples").and_then(Value::as_object) {
            for (name, value) in extra {
                if let Some(Err(error)) = support::generated::decode_response(&id, value.clone()) {
                    stale.push(format!("{id} x-examples.{name}: {error}"));
                }
            }
        }
    }
    if !stale.is_empty() {
        eprintln!(
            "{} x-examples do not match their schema (reported, not enforced):\n{}",
            stale.len(),
            stale.join("\n")
        );
    }
}

#[test]
fn every_documented_request_body_example_decodes() {
    let doc = document();
    let mut failures = Vec::new();
    let mut checked = 0;
    for (id, operation) in operations(&doc) {
        let Some(media) = operation.pointer("/requestBody/content/application~1json") else {
            continue;
        };
        for (label, value) in media_examples(&format!("{id} request"), media) {
            // An empty object is a placeholder, not an example of the body.
            if value.as_object().is_some_and(serde_json::Map::is_empty) {
                continue;
            }
            match support::generated::decode_body(&id, value) {
                Some(Ok(())) => checked += 1,
                Some(Err(error)) => failures.push(format!("{label}: {error}")),
                None => failures.push(format!("{label}: no generated body type for {id}")),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "request body examples failed:\n{}",
        failures.join("\n")
    );
    assert!(checked > 0, "no request body example was checked");
}

#[test]
fn every_documented_error_example_is_an_api_error_envelope() {
    let doc = document();
    let mut failures = Vec::new();
    let mut checked = 0;
    for (id, operation) in operations(&doc) {
        for (code, response) in operation["responses"].as_object().expect("responses") {
            if !(code.starts_with('4') || code.starts_with('5')) {
                continue;
            }
            let Some(media) = response.pointer("/content/application~1json") else {
                continue;
            };
            let schema = media
                .pointer("/schema/$ref")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if schema != "#/components/schemas/ApiError" {
                continue;
            }
            for (label, value) in media_examples(&format!("{id} {code}"), media) {
                match support::generated::decode_component("ApiError", value) {
                    Some(Ok(())) => checked += 1,
                    Some(Err(error)) => failures.push(format!("{label}: {error}")),
                    None => failures.push("no generated type for ApiError".to_owned()),
                }
            }
        }
    }
    assert!(failures.is_empty(), "error examples failed:\n{}", failures.join("\n"));
    assert!(checked > 0, "no error example was checked");
}

#[test]
fn every_operation_is_in_the_table_once() {
    let doc = document();
    let ids: Vec<String> = operations(&doc).into_iter().map(|(id, _)| id).collect();
    let table: Vec<&str> = oxinsider::OPERATIONS.iter().map(|op| op.id).collect();
    assert_eq!(table.len(), ids.len());
    for id in &ids {
        assert_eq!(table.iter().filter(|t| **t == id.as_str()).count(), 1, "{id}");
    }
    assert_eq!(oxinsider::provenance::OPERATION_COUNT, ids.len());
}

#[test]
fn provenance_matches_the_committed_document() {
    use sha2::{Digest, Sha256};
    let raw = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json")).expect("openapi.json");
    let digest: String = Sha256::digest(&raw).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(digest, oxinsider::provenance::OPENAPI_SHA256);
}
