//! Fuzzes HTTP content negotiation against hostile `Accept` and `Prefer` headers.
//!
//! An `Accept` header is fully attacker-controlled text that the runtime splits
//! on commas and semicolons, parses quality values out of, sorts, and then
//! matches against the media types a spec declares. `Prefer` is parsed the same
//! way and additionally feeds `code=` / `example=` selection.
//!
//! Invariants:
//! - parsing never panics;
//! - a negotiated media type is always one the response actually declared;
//! - an unsatisfiable `Accept` yields `None` rather than a fallback;
//! - `Prefer: code=N` selects a response only when `N` is declared.

#![no_main]

use std::collections::BTreeMap;

use http::HeaderMap;
use libfuzzer_sys::fuzz_target;
use specmock_runtime::http::negotiate::{
    PreferDirectives, negotiate_media_type, select_response,
};
use specmock_runtime::http::openapi::{MediaTypeSpec, ResponseSpec};

/// A single declared media type with no schema and no example.
fn media(media_type: &str) -> MediaTypeSpec {
    MediaTypeSpec {
        media_type: media_type.to_owned(),
        schema: None,
        example: None,
        named_examples: BTreeMap::new(),
    }
}

/// The response catalogue every input is negotiated against.
fn catalogue() -> Vec<ResponseSpec> {
    vec![
        ResponseSpec {
            status: "200".to_owned(),
            content: vec![media("application/json")],
            headers: BTreeMap::new(),
        },
        ResponseSpec {
            status: "404".to_owned(),
            content: vec![media("application/json")],
            headers: BTreeMap::new(),
        },
        ResponseSpec {
            status: "default".to_owned(),
            content: vec![media("application/json"), media("text/plain")],
            headers: BTreeMap::new(),
        },
    ]
}

fuzz_target!(|data: &[u8]| {
    let responses = catalogue();
    let declared: Vec<String> =
        responses.iter().flat_map(|response| response.media_types()).collect();

    let text = String::from_utf8_lossy(data);
    let mut headers = HeaderMap::new();
    if let Ok(value) = http::HeaderValue::from_str(&text) {
        headers.insert("accept", value);
    }

    // Prefer parsing over the same raw bytes.
    let mut prefer_headers = HeaderMap::new();
    if let Ok(value) = http::HeaderValue::from_str(&text) {
        prefer_headers.append("prefer", value);
    }
    let prefer = PreferDirectives::from_headers(&prefer_headers);

    // Negotiation must never produce an undeclared media type.
    if let Some(chosen) = negotiate_media_type(&declared, Some(&text)) {
        assert!(
            declared.contains(&chosen),
            "negotiation returned undeclared media type {chosen:?}; declared {declared:?}"
        );
    }

    // With no Accept header the first declared type is always chosen.
    assert_eq!(negotiate_media_type(&declared, None), declared.first().cloned());

    // A `code=` directive that names a declared status must select it.
    if let Some(code) = prefer.code {
        let status = code.to_string();
        let selected = select_response(&responses, &prefer);
        if responses.iter().any(|response| response.status == status) {
            assert_eq!(
                selected.map(|response| response.status.clone()),
                Some(status),
                "a declared code must be selectable"
            );
        } else {
            assert!(
                selected.is_none(),
                "an undeclared code must select nothing, got {selected:?}"
            );
        }
    }

    // Negotiation without preferences always yields something.
    assert!(select_response(&responses, &PreferDirectives::default()).is_some());
});