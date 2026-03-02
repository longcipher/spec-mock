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
        if let Some(found) = responses.iter().find(|r| r.status == code_str) {
            return Some(found);
        }
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
    use std::collections::HashMap;

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
    fn parses_single_prefer_code() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "code=404".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, Some(404));
    }

    #[test]
    fn parses_prefer_example() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "example=notFound".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.example.as_deref(), Some("notFound"));
    }

    #[test]
    fn parses_prefer_dynamic() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "dynamic=true".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert!(prefer.dynamic);
    }

    #[test]
    fn parses_combined_prefer_directives() {
        let mut headers = HeaderMap::new();
        headers.insert("prefer", "code=500, example=serverError, dynamic=true".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.code, Some(500));
        assert_eq!(prefer.example.as_deref(), Some("serverError"));
        assert!(prefer.dynamic);
    }

    #[test]
    fn parses_accept_header() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "application/xml;q=0.5, application/json".parse().unwrap());
        let prefer = PreferDirectives::from_headers(&headers);
        assert_eq!(prefer.media_type.as_deref(), Some("application/json"));
    }

    // ── select_response ────────────────────────────────────────────────

    fn make_responses() -> Vec<ResponseSpec> {
        vec![
            ResponseSpec {
                status: "200".into(),
                schema: None,
                example: None,
                named_examples: HashMap::new(),
            },
            ResponseSpec {
                status: "404".into(),
                schema: None,
                example: None,
                named_examples: HashMap::new(),
            },
            ResponseSpec {
                status: "500".into(),
                schema: None,
                example: None,
                named_examples: HashMap::new(),
            },
        ]
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
        // No 418, falls back to 200
        assert_eq!(selected.map(|r| r.status.as_str()), Some("200"));
    }

    #[test]
    fn select_response_default_fallback() {
        let responses = vec![ResponseSpec {
            status: "default".into(),
            schema: None,
            example: None,
            named_examples: HashMap::new(),
        }];
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
}
