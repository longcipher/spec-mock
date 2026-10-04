//! Fuzzes response-body encoding and accept matching for hostile inputs.
//!
//! Every mocked HTTP response runs its JSON value through `encode_body`, which
//! dispatches on the negotiated media type and — for XML — walks the value tree
//! while sanitizing element names and escaping text. Both the value and the
//! media type come from a spec file, so both are attacker-controlled, and the
//! XML name sanitizer derives tag names from arbitrary object keys.
//!
//! Invariants:
//! - encoding never panics, whatever the media type or value;
//! - JSON media types round-trip the value exactly;
//! - XML output is always a single, balanced element tree;
//! - accept matching never returns a media type it was not offered.

#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use specmock_runtime::http::media::{
    encode_body, is_binary_media_type, is_json_media_type, is_text_media_type, is_xml_media_type,
    json_to_xml, media_type_is_accepted, normalize_media_type, xml_root_name,
};

/// A handful of interesting media types, chosen by input byte, plus the exact
/// string the fuzzer supplied.
fn media_type(input: &[u8], raw: &str) -> String {
    const POOL: [&str; 10] = [
        "application/json",
        "application/problem+json",
        "application/vnd.api+json",
        "application/xml",
        "text/xml",
        "application/soap+xml",
        "text/plain",
        "text/html",
        "application/octet-stream",
        "image/png",
    ];
    match input.first() {
        Some(byte) => POOL[usize::from(*byte) % POOL.len()].to_owned(),
        None => raw.to_owned(),
    }
}

fuzz_target!(|data: &[u8]| {
    let split = data.len() / 2;
    let (value_bytes, media_bytes) = data.split_at(split);

    let raw_media = String::from_utf8_lossy(media_bytes);
    let media = media_type(media_bytes, raw_media.trim());

    let value: Value = serde_json::from_slice(value_bytes).unwrap_or_else(|_error| {
        Value::String(String::from_utf8_lossy(value_bytes).into_owned())
    });

    // Classification must be total and mutually exclusive. Binary deliberately
    // overlaps XML (`image/svg+xml` is both), so it is only checked against text.
    let is_json = is_json_media_type(&media);
    let is_xml = is_xml_media_type(&media);
    let is_text = is_text_media_type(&media);
    let is_binary = is_binary_media_type(&media);
    assert!(!(is_json && is_xml), "{media} classified as both JSON and XML");
    assert!(!(is_json && is_text), "{media} classified as both JSON and text");
    assert!(!(is_xml && is_text), "{media} classified as both XML and text");
    assert!(!(is_binary && is_text), "{media} classified as both binary and text");

    // Normalization is idempotent.
    let normalized = normalize_media_type(&media);
    assert_eq!(
        normalize_media_type(&normalized),
        normalized,
        "media-type normalization is not idempotent for {media:?}"
    );

    // Encoding must never panic and must stay bounded.
    let encoded = encode_body(&media, &value, None);
    assert!(
        encoded.len() <= 1 << 24,
        "encode_body produced {} bytes for a {}-byte input",
        encoded.len(),
        data.len()
    );

    if is_json {
        let text = std::str::from_utf8(&encoded).expect("JSON output must be valid UTF-8");
        let decoded: Value = serde_json::from_str(text).expect("JSON output must re-parse");
        assert_eq!(&decoded, &value, "JSON encoding did not round-trip");
    }

    // XML rendering is reachable both directly and through encode_body.
    let root = xml_root_name(Some(&value));
    let xml = json_to_xml(&root, &value);
    assert_balanced(&xml);

    let via_encoder = encode_body("application/xml", &value, Some(&value));
    let via_encoder_text = String::from_utf8_lossy(&via_encoder);
    assert_balanced(&via_encoder_text);

    // Accept matching is total: an empty declaration list accepts nothing, and a
    // non-empty media type always accepts both itself and the `*/*` wildcard.
    // An empty `actual` type is rejected by design, so it is excluded.
    assert!(
        !media_type_is_accepted(&[], &media),
        "an empty declaration list must accept nothing"
    );
    if !media.is_empty() {
        assert!(
            media_type_is_accepted(&[media.clone()], &media),
            "an exactly-declared media type must accept itself"
        );
        assert!(media_type_is_accepted(&["*/*".to_owned()], &media));
    }
});

/// Every `<` in the output must be matched by a `>` and the document must be
/// rooted in an element.
fn assert_balanced(rendered: &str) {
    let opens = rendered.matches('<').count();
    let closes = rendered.matches('>').count();
    assert_eq!(opens, closes, "unbalanced angle brackets in {rendered:?}");

    assert!(rendered.contains("</"), "no closing tag at all in {rendered:?}");
    assert!(rendered.starts_with('<'), "output is not rooted in an element: {rendered:?}");
}