//! Fuzzes JSON-pointer navigation against hostile documents and pointers.
//!
//! `resolve_pointer` is on the hot path of every `$ref` in a spec, so it
//! consumes input that is fully attacker-controlled on both sides: the
//! document being walked and the pointer being followed. It also unescapes
//! `~0`/`~1` and parses array indices, all of which index into the tree.
//!
//! Invariant: resolution either returns a node from the document or `None`; it
//! never panics and never returns a value the document does not contain.

#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use specmock_core::ref_resolver::resolve_pointer;

fuzz_target!(|data: &[u8]| {
    let split = data.len() / 2;
    let (document_bytes, pointer_bytes) = data.split_at(split);

    // Build the document from the fuzz input, and also try a fixed structural
    // document so the pointer always has real nesting to traverse.
    let parsed: Option<Value> = serde_json::from_slice(document_bytes).ok();
    let synthetic = synthetic_document(pointer_bytes);

    for document in [parsed.unwrap_or_else(|| json!({})), synthetic] {
        for pointer in candidate_pointers(pointer_bytes) {
            let resolved = resolve_pointer(&document, &pointer);

            if let Some(value) = resolved {
                // A returned value must belong to the document it came from.
                assert!(
                    contains(&document, &value),
                    "resolve_pointer returned a value absent from the source document for pointer {pointer:?}"
                );
            }
        }
    }
});

/// Pointer candidates derived from the fuzz input.
///
/// Covers the interesting shapes: empty, `/`, no leading slash, unterminated
/// escapes, escaped separators, numeric array indices, and deep nesting.
fn candidate_pointers(bytes: &[u8]) -> Vec<String> {
    let raw = String::from_utf8_lossy(bytes);
    vec![
        String::new(),
        "/".to_owned(),
        raw.to_string(),
        format!("/{raw}"),
        format!("/a~1b"),
        format!("/a~0b"),
        format!("/a/b/c/d/e"),
        "/0".to_owned(),
        "/items/0".to_owned(),
        "/items/9999".to_owned(),
        "/items/-1".to_owned(),
        "/items/not-a-number".to_owned(),
        format!("/{raw}/.."),
        format!("/{}", raw.replace('~', "~0").replace('/', "~1")),
    ]
}

/// A document whose shape is driven by the input, so pointer-shaped bytes can
/// produce documents that match them.
fn synthetic_document(bytes: &[u8]) -> Value {
    let segment = String::from_utf8_lossy(bytes).replace(['/', '~'], "_");
    json!({
        "a": {"b": {"c": segment.clone()}},
        "m~n": segment,
        "items": [1, {"a": 2}],
        "0": "numeric key",
        "": "empty key",
    })
}

/// Shallow structural containment check: the resolved value must be reachable
/// somewhere in the document tree.
fn contains(document: &Value, needle: &Value) -> bool {
    if document == needle {
        return true;
    }
    match document {
        Value::Array(items) => items.iter().any(|item| contains(item, needle)),
        Value::Object(map) => map.values().any(|value| contains(value, needle)),
        _ => false,
    }
}