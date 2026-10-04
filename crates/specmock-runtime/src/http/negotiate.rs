//! Content negotiation: `Prefer` header parsing and `Accept` media-type matching.

use http::HeaderMap;

/// Directives extracted from the `Prefer` and `Accept` request headers.
#[derive(Debug, Clone, Default)]
pub struct PreferDirectives {
    /// Desired HTTP status code (`Prefer: code=404`).
    pub code: Option<u16>,
    /// Desired named example (`Prefer: example=notFound`).
    pub example: Option<String>,
    /// Desired media type from `Accept` header.
    pub media_type: Option<String>,
    /// Whether dynamic faker mode was requested (`Prefer: dynamic=true`).
    pub dynamic: bool,
}

impl PreferDirectives {
    /// Parse directives from HTTP request headers.
    ///
    /// Recognizes:
    /// - `Prefer: code=<status>` — select a response by status code
    /// - `Prefer: example=<name>` — select a named example
    /// - `Prefer: dynamic=true`  — force faker-generated dynamic data
    /// - `Accept` header         — preferred response media type
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let mut directives = Self::default();

        // Parse all `Prefer` header values (may appear multiple times).
        for value in headers.get_all("prefer") {
            let Ok(text) = value.to_str() else {
                continue;
            };
            parse_prefer_value(text, &mut directives);
        }

        // Extract media type from `Accept` header.
        if let Some(accept) = headers.get("accept").and_then(|v| v.to_str().ok()) {
            directives.media_type = best_media_type(accept);
        }

        directives
    }
}

/// Parse a single `Prefer` header value which may contain comma-separated directives.
fn parse_prefer_value(text: &str, directives: &mut PreferDirectives) {
    for segment in text.split(',') {
        let trimmed = segment.trim();
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "code" => {
                    directives.code = value.parse::<u16>().ok();
                }
                "example" if !value.is_empty() => {
                    directives.example = Some(value.to_owned());
                }
                "dynamic" => {
                    directives.dynamic = value.eq_ignore_ascii_case("true");
                }
                _ => {}
            }
        }
    }
}

/// Parsed entry from an `Accept` header (e.g. `application/json;q=0.9`).
#[derive(Debug)]
struct MediaEntry {
    media_type: String,
    quality: f32,
}

/// Select the best media type from an `Accept` header value.
///
/// Returns the highest-quality non-wildcard type, or `None` when the header is
/// empty/unparseable.
fn best_media_type(accept: &str) -> Option<String> {
    let mut entries: Vec<MediaEntry> = accept
        .split(',')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }

            let (media_type, params) = segment.split_once(';').unwrap_or((segment, ""));

            let quality = params
                .split(';')
                .chain(std::iter::once(params))
                .find_map(|part| {
                    let part = part.trim();
                    part.strip_prefix("q=").and_then(|q| q.trim().parse::<f32>().ok())
                })
                .unwrap_or(1.0);

            Some(MediaEntry { media_type: media_type.trim().to_owned(), quality })
        })
        .collect();

    entries.sort_by(|a, b| b.quality.partial_cmp(&a.quality).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().map(|e| e.media_type).find(|mt| mt != "*/*")
}

use super::openapi::ResponseSpec;

/// Select the best `ResponseSpec` given the caller's preferences.
///
/// Priority order:
/// 1. Exact status-code match when `prefer.code` is set.
/// 2. First 2xx response.
/// 3. `default` response.
/// 4. First declared response.
pub fn select_response<'a>(
    responses: &'a [ResponseSpec],
    prefer: &PreferDirectives,
) -> Option<&'a ResponseSpec> {
    if let Some(code) = prefer.code {
        let code_str = code.to_string();
        return responses.iter().find(|r| r.status == code_str);
    }

    responses
        .iter()
        .find(|r| r.status == "200")
        .or_else(|| responses.iter().find(|r| r.status.starts_with('2')))
        .or_else(|| responses.iter().find(|r| r.status == "default"))
        .or_else(|| responses.first())
}

/// Pick the best media type from a set of available types given an `Accept`
/// header.
///
/// Returns `None` when no acceptable match is found.
pub fn negotiate_media_type(available: &[String], accept_header: Option<&str>) -> Option<String> {
    let Some(accept) = accept_header else {
        return available.first().cloned();
    };

    // Build priority-sorted list of requested types.
    let mut requested: Vec<MediaEntry> = accept
        .split(',')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }
            let (media_type, params) = segment.split_once(';').unwrap_or((segment, ""));
            let quality = params
                .split(';')
                .chain(std::iter::once(params))
                .find_map(|part| {
                    let part = part.trim();
                    part.strip_prefix("q=").and_then(|q| q.trim().parse::<f32>().ok())
                })
                .unwrap_or(1.0);
            Some(MediaEntry { media_type: media_type.trim().to_owned(), quality })
        })
        .collect();

    requested
        .sort_by(|a, b| b.quality.partial_cmp(&a.quality).unwrap_or(std::cmp::Ordering::Equal));

    for entry in &requested {
        if entry.media_type == "*/*" {
            return available.first().cloned();
        }
        if available.iter().any(|a| a == &entry.media_type) {
            return Some(entry.media_type.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use http::HeaderMap;

    use super::*;

    // ── PreferDirectives::from_headers ─────────────────────────────────

    #[test]
    fn parses_empty_headers() {
        let headers = HeaderMap::new();
        let prefer = PreferDirectives::from_headers(&headers);
        assert!(prefer.code.is_none());
        assert!(prefer.example.is_none());
        assert!(prefer.media_type.is_none());
        assert!(!prefer.dynamic);
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "test code: header value is a valid str literal")]
    fn parses_single_prefer_code() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "code=404".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, Some(404));
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "test code: header value is a valid str literal")]
    fn parses_prefer_example() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "example=notFound".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.example.as_deref(), Some("notFound"));
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "test code: header value is a valid str literal")]
    fn parses_prefer_dynamic() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "dynamic=true".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert!(prefer.dynamic);
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "test code: header value is a valid str literal")]
    fn parses_combined_prefer_directives() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "code=500, example=serverError, dynamic=true".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, Some(500));
        assert_eq!(prefer.example.as_deref(), Some("serverError"));
        assert!(prefer.dynamic);
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "test code: header value is a valid str literal")]
    fn parses_accept_header() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "application/xml;q=0.5, application/json".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.media_type.as_deref(), Some("application/json"));
    }

    // ── select_response ────────────────────────────────────────────────

    /// Build a body-less response spec for the given status.
    fn bodyless_response(status: &str) -> ResponseSpec {
        ResponseSpec { status: status.into(), content: Vec::new(), headers: BTreeMap::new() }
    }

    fn make_responses() -> Vec<ResponseSpec> {
        vec![bodyless_response("200"), bodyless_response("404"), bodyless_response("500")]
    }

    #[test]
    fn select_response_prefers_requested_code() {
        let responses = make_responses();
        let prefer = PreferDirectives { code: Some(404), ..Default::default() };
        let selected = select_response(&responses, &prefer);
        assert_eq!(selected.map(|r| r.status.as_str()), Some("404"));
    }

    #[test]
    fn select_response_falls_back_to_200() {
        let responses = make_responses();
        let prefer = PreferDirectives::default();
        let selected = select_response(&responses, &prefer);
        assert_eq!(selected.map(|r| r.status.as_str()), Some("200"));
    }

    #[test]
    fn select_response_falls_back_when_code_missing() {
        let responses = make_responses();
        let prefer = PreferDirectives { code: Some(418), ..Default::default() };
        let selected = select_response(&responses, &prefer);
        assert!(selected.is_none(), "requested code 418 not found should return None");
    }

    #[test]
    fn select_response_default_fallback() {
        let responses = vec![bodyless_response("default")];
        let prefer = PreferDirectives::default();
        let selected = select_response(&responses, &prefer);
        assert_eq!(selected.map(|r| r.status.as_str()), Some("default"));
    }

    // ── negotiate_media_type ───────────────────────────────────────────

    #[test]
    fn negotiate_returns_first_when_no_accept() {
        let available = vec!["application/json".into(), "text/plain".into()];
        let result = negotiate_media_type(&available, None);
        assert_eq!(result.as_deref(), Some("application/json"));
    }

    #[test]
    fn negotiate_matches_exact_type() {
        let available = vec!["application/json".into(), "application/xml".into()];
        let result = negotiate_media_type(&available, Some("application/xml"));
        assert_eq!(result.as_deref(), Some("application/xml"));
    }

    #[test]
    fn negotiate_respects_quality_values() {
        let available = vec!["application/json".into(), "application/xml".into()];
        let result =
            negotiate_media_type(&available, Some("application/xml;q=0.5, application/json;q=0.9"));
        assert_eq!(result.as_deref(), Some("application/json"));
    }

    #[test]
    fn negotiate_wildcard_returns_first_available() {
        let available = vec!["application/json".into()];
        let result = negotiate_media_type(&available, Some("*/*"));
        assert_eq!(result.as_deref(), Some("application/json"));
    }

    #[test]
    fn negotiate_returns_none_for_unsupported() {
        let available = vec!["application/json".into()];
        let result = negotiate_media_type(&available, Some("text/html"));
        assert!(result.is_none());
    }

    // ── Prefer parsing edge cases ───────────────────────────────────────

    #[test]
    fn multiple_prefer_headers_are_merged() {
        let mut headers = HeaderMap::new();
        headers.append("prefer", "code=404".parse().expect("value"));
        headers.append("prefer", "example=missing".parse().expect("value"));

        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, Some(404));
        assert_eq!(prefer.example.as_deref(), Some("missing"));
    }

    #[test]
    fn later_prefer_headers_override_earlier_ones() {
        let mut headers = HeaderMap::new();
        headers.append("prefer", "code=404".parse().expect("value"));
        headers.append("prefer", "code=500".parse().expect("value"));

        assert_eq!(PreferDirectives::from_headers(&headers).code, Some(500));
    }

    #[test]
    fn invalid_code_directive_leaves_the_code_unset() {
        let mut headers = HeaderMap::new();
        for value in ["code=abc", "code=", "code=-1", "code=99999"] {
            headers.insert("prefer", value.parse().expect("value"));
            assert_eq!(
                PreferDirectives::from_headers(&headers).code,
                None,
                "{value} must not produce a status code"
            );
        }
    }

    #[test]
    fn empty_example_directive_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "example=".parse().expect("value"));
        assert_eq!(PreferDirectives::from_headers(&headers).example, None);
    }

    #[test]
    fn dynamic_directive_requires_the_literal_true() {
        for (value, expected) in [
            ("dynamic=true", true),
            ("dynamic=TRUE", true),
            ("dynamic=True", true),
            ("dynamic=false", false),
            ("dynamic=1", false),
            ("dynamic=", false),
            ("dynamic=yes", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("prefer", value.parse().expect("value"));
            assert_eq!(
                PreferDirectives::from_headers(&headers).dynamic,
                expected,
                "{value} should map to dynamic={expected}"
            );
        }
    }

    #[test]
    fn unknown_prefer_directives_are_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "wait=10, handling=strict, respond-async".parse().expect("v"));
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, None);
        assert_eq!(prefer.example, None);
        assert!(!prefer.dynamic);
    }

    #[test]
    fn malformed_segments_and_whitespace_are_tolerated() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "  code = 201 , , dynamic = true ,".parse().expect("v"));
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, Some(201));
        assert!(prefer.dynamic);
    }

    #[test]
    fn invalid_header_bytes_in_prefer_are_skipped() {
        let mut headers = HeaderMap::new();
        headers.append("prefer", http::HeaderValue::from_bytes(b"code=4\x80").expect("raw bytes"));
        headers.append("prefer", "example=kept".parse().expect("value"));

        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, None, "an undecodable header value is skipped");
        assert_eq!(prefer.example.as_deref(), Some("kept"));
    }

    // ── Accept / best_media_type ───────────────────────────────────────

    #[test]
    fn best_media_type_orders_by_quality() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            "text/html;q=0.2, application/xml;q=0.9, application/json;q=0.5".parse().expect("v"),
        );
        assert_eq!(
            PreferDirectives::from_headers(&headers).media_type.as_deref(),
            Some("application/xml")
        );
    }

    #[test]
    fn best_media_type_falls_back_to_the_first_wildcard_free_entry() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "*/*, text/plain".parse().expect("v"));
        assert_eq!(
            PreferDirectives::from_headers(&headers).media_type.as_deref(),
            Some("text/plain"),
            "a wildcard is skipped in favour of a concrete type"
        );
    }

    #[test]
    fn best_media_type_is_none_for_a_wildcard_only_accept() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "*/*".parse().expect("v"));
        assert_eq!(PreferDirectives::from_headers(&headers).media_type, None);
    }

    #[test]
    fn best_media_type_is_none_for_an_empty_accept() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "".parse().expect("v"));
        assert_eq!(PreferDirectives::from_headers(&headers).media_type, None);

        headers.insert("accept", ", ,".parse().expect("v"));
        assert_eq!(PreferDirectives::from_headers(&headers).media_type, None);
    }

    #[test]
    fn best_media_type_ignores_an_unparsable_quality() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "application/json;q=high".parse().expect("v"));
        assert_eq!(
            PreferDirectives::from_headers(&headers).media_type.as_deref(),
            Some("application/json"),
            "an unparsable q defaults to 1.0 rather than discarding the type"
        );
    }

    #[test]
    fn invalid_accept_header_bytes_yield_no_media_type() {
        let mut headers = HeaderMap::new();
        headers
            .insert("accept", http::HeaderValue::from_bytes(b"application/json\x80").expect("raw"));
        assert_eq!(PreferDirectives::from_headers(&headers).media_type, None);
    }

    #[test]
    fn best_media_type_handles_multiple_parameters() {
        let mut headers = HeaderMap::new();
        headers
            .insert("accept", "application/json;charset=utf-8;q=0.4;level=1".parse().expect("v"));
        assert_eq!(
            PreferDirectives::from_headers(&headers).media_type.as_deref(),
            Some("application/json")
        );
    }

    // ── select_response ────────────────────────────────────────────────

    #[test]
    fn select_response_prefers_any_2xx_when_200_is_absent() {
        let responses = vec![bodyless_response("404"), bodyless_response("204")];
        let selected = select_response(&responses, &PreferDirectives::default());
        assert_eq!(selected.map(|r| r.status.as_str()), Some("204"));
    }

    #[test]
    fn select_response_prefers_default_over_the_first_declared() {
        let responses = vec![bodyless_response("404"), bodyless_response("default")];
        let selected = select_response(&responses, &PreferDirectives::default());
        assert_eq!(selected.map(|r| r.status.as_str()), Some("default"));
    }

    #[test]
    fn select_response_on_an_empty_list_is_none() {
        assert!(select_response(&[], &PreferDirectives::default()).is_none());
    }

    #[test]
    fn select_response_can_match_the_default_selector_by_code_string() {
        // `Prefer: code=default` is not a number so it never reaches here, but
        // `select_response` compares stringified codes; confirm an explicit
        // numeric code never aliases `default`.
        let responses = vec![bodyless_response("default")];
        let prefer = PreferDirectives { code: Some(200), ..Default::default() };
        assert!(select_response(&responses, &prefer).is_none());
    }

    #[test]
    fn select_response_with_no_content_prefers_200_by_declaration() {
        let responses = vec![bodyless_response("201"), bodyless_response("200")];
        assert_eq!(
            select_response(&responses, &PreferDirectives::default()).map(|r| r.status.as_str()),
            Some("200"),
            "an exact 200 wins even when another 2xx is declared first"
        );
    }

    // ── negotiate_media_type ───────────────────────────────────────────

    #[test]
    fn negotiate_prefers_a_high_quality_unsupported_type_over_a_low_quality_match() {
        let available = vec!["application/json".to_owned(), "application/xml".to_owned()];
        // `application/xml` sorts first by q but is available, so it wins.
        let result =
            negotiate_media_type(&available, Some("application/xml;q=0.9, application/json;q=0.1"));
        assert_eq!(result.as_deref(), Some("application/xml"));
    }

    #[test]
    fn negotiate_skips_an_unsupported_high_quality_type() {
        let available = vec!["application/json".to_owned()];
        let result =
            negotiate_media_type(&available, Some("text/html;q=1.0, application/json;q=0.1"));
        assert_eq!(
            result.as_deref(),
            Some("application/json"),
            "an unavailable top preference must not abort negotiation"
        );
    }

    #[test]
    fn negotiate_on_an_empty_available_list_is_none() {
        assert!(negotiate_media_type(&[], None).is_none());
        assert!(negotiate_media_type(&[], Some("application/json")).is_none());
        assert!(negotiate_media_type(&[], Some("*/*")).is_none());
    }

    #[test]
    fn negotiate_wildcard_at_any_position_yields_the_first_available() {
        let available = vec!["application/json".to_owned(), "text/plain".to_owned()];
        for accept in ["text/html, */*", "*/*, text/html", "text/html;q=0.1, */*;q=0.2"] {
            assert_eq!(
                negotiate_media_type(&available, Some(accept)).as_deref(),
                Some("application/json"),
                "accept {accept} should fall back to the first available type"
            );
        }
    }

    #[test]
    fn negotiate_matches_media_types_case_sensitively() {
        let available = vec!["application/json".to_owned()];
        assert!(negotiate_media_type(&available, Some("APPLICATION/JSON")).is_none());
    }

    #[test]
    fn negotiate_handles_parameters_and_empty_segments() {
        let available = vec!["application/json".to_owned()];
        assert_eq!(
            negotiate_media_type(&available, Some(", application/json;charset=utf-8 ;")).as_deref(),
            Some("application/json")
        );
    }

    #[test]
    fn negotiate_ignores_unparsable_quality_values() {
        let available = vec!["application/json".to_owned(), "text/plain".to_owned()];
        assert_eq!(
            negotiate_media_type(&available, Some("application/json;q=nonsense")).as_deref(),
            Some("application/json")
        );
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// Whatever the `Prefer` header text, parsing must never panic and the
        /// resulting code must always be a real status when present.
        #[test]
        fn prefer_parsing_never_panics(value in ".{0,60}")
        {
            let Ok(header) = http::HeaderValue::from_str(&value) else {
                return Ok(());
            };
            let mut headers = HeaderMap::new();
            headers.insert("prefer", header);
            let prefer = PreferDirectives::from_headers(&headers);
            if let Some(code) = prefer.code {
                prop_assert!(code > 0);
            }
        }

        /// Negotiation is total and only ever returns a declared media type.
        #[test]
        fn negotiation_only_returns_declared_types(
            available in prop::collection::vec(
                prop::sample::select(vec![
                    "application/json".to_owned(),
                    "text/plain".to_owned(),
                    "application/xml".to_owned(),
                ]),
                1..4,
            ),
            accept in prop::sample::select(vec![
                "application/json".to_owned(),
                "text/plain".to_owned(),
                "application/xml".to_owned(),
                "text/html".to_owned(),
                "*/*".to_owned(),
            ]),
        ) {
            if let Some(chosen) = negotiate_media_type(&available, Some(&accept)) {
                prop_assert!(
                    available.contains(&chosen),
                    "negotiation returned {chosen} which is not in {available:?}"
                );
            }
        }

        /// With no `Accept` header the first declared type must always win.
        #[test]
        fn absent_accept_returns_the_first_declared_type(
            mut available in prop::collection::vec("[a-z]+/[a-z+.-]{1,8}", 1..4)
        ) {
            available.dedup();
            let result = negotiate_media_type(&available, None);
            prop_assert_eq!(result, available.first().cloned());
        }

        /// A selected response is always one of the declared responses.
        #[test]
        fn selection_always_returns_a_declared_response(
            statuses in prop::collection::vec(
                prop::sample::select(vec!["200", "201", "404", "500", "default", "2XX"]),
                1..5,
            ),
            code in prop::option::of(any::<u16>()),
        ) {
            let responses: Vec<ResponseSpec> =
                statuses.iter().map(|status| bodyless_response(status)).collect();
            let prefer = PreferDirectives { code, ..Default::default() };
            if let Some(selected) = select_response(&responses, &prefer) {
                prop_assert!(
                    statuses.iter().any(|status| status == &selected.status),
                    "selected {} which is not declared in {statuses:?}",
                    selected.status
                );
            }
        }

        /// Prefer parsing over arbitrary repeated headers is total.
        #[test]
        fn repeated_prefer_headers_never_panic(
            values in prop::collection::vec("[a-z]{1,10}(=[^,]{0,10})?", 0..4)
        ) {
            let mut headers = HeaderMap::new();
            for value in &values {
                if let Ok(header) = http::HeaderValue::from_str(value) {
                    headers.append("prefer", header);
                }
            }
            let _ = PreferDirectives::from_headers(&headers);
        }
    }
}
