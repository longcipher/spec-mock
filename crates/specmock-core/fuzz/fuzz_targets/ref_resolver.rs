//! Fuzzes `$ref` resolution over hostile spec documents.
//!
//! `RefResolver` walks an entire OpenAPI/AsyncAPI document tree and inlines
//! every `$ref` it finds, including file-relative refs that touch the
//! filesystem. This target feeds it attacker-controlled documents whose `$ref`
//! strings are also attacker-controlled, because that string decides the local
//! pointer lookup, the file path, or the URL rejection branch.
//!
//! Invariant: resolution may legitimately fail (missing refs, cycles, depth
//! limits, unreadable files) but must never panic.

#![no_main]

use std::path::PathBuf;

use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value, json};
use specmock_core::ref_resolver::RefResolver;

/// Maximum tree depth the injector will descend to, so a deeply nested input
/// cannot turn into an exponential walk.
const MAX_INJECT_DEPTH: usize = 8;

fuzz_target!(|data: &[u8]| {
    // Split the input: the first half is the document, the second half becomes
    // the injected `$ref` string. Both halves are attacker-controlled.
    let split = data.len() / 2;
    let (document_bytes, ref_bytes) = data.split_at(split);

    let mut document = match serde_json::from_slice::<Value>(document_bytes) {
        Ok(value) => value,
        // Unparsable input still reaches the resolver as a string leaf, which
        // exercises the scalar branch of the walker.
        Err(_) => json!({ "seed": String::from_utf8_lossy(document_bytes) }),
    };

    let ref_string = String::from_utf8_lossy(ref_bytes).into_owned();
    inject_refs(&mut document, &ref_string, 0);

    let mut resolver = RefResolver::new(PathBuf::from("/specmock-fuzz-nonexistent-root"));
    // `resolve_value_tree` resolves against `base_dir`, so no file outside that
    // (non-existent) root can ever be reached.
    let _ = resolver.resolve_value_tree(&mut document);
});

/// Recursively attach a `$ref` node built from `ref_string` to every object and
/// to every array element, so one input exercises many resolution sites.
fn inject_refs(node: &mut Value, ref_string: &str, depth: usize) {
    if depth > MAX_INJECT_DEPTH {
        return;
    }

    match node {
        Value::Object(map) => {
            map.insert(
                "fuzzRef".to_owned(),
                json!({
                    "$ref": ref_string,
                    // Sibling keys exercise the sibling-merge path, including
                    // the case where the resolved target is not an object.
                    "description": "fuzz sibling"
                }),
            );

            for value in map.values_mut() {
                inject_refs(value, ref_string, depth + 1);
            }
        }
        Value::Array(items) => {
            items.push(json!({ "$ref": ref_string }));
            for item in items.iter_mut() {
                inject_refs(item, ref_string, depth + 1);
            }
        }
        // Scalars get wrapped so the walker sees an object it can attach to.
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            let replacement = Map::from_iter([
                ("wrapped".to_owned(), Value::String(ref_string.to_owned())),
                ("nested".to_owned(), json!({ "$ref": ref_string })),
            ]);
            *node = Value::Object(replacement);
        }
    }
}