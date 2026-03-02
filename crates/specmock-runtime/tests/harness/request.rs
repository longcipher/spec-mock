/// A single fuzz request to replay against both Prism and spec-mock.
pub(crate) struct FuzzRequest {
    pub(crate) method: http::Method,
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Option<Vec<u8>>,
    pub(crate) content_type: Option<String>,
    /// Human-readable label used in comparator output.
    pub(crate) description: String,
}

/// The captured HTTP response from one server.
pub(crate) struct CapturedResponse {
    pub(crate) status: u16,
    /// Lowercased MIME type without charset (e.g. `application/json`).
    pub(crate) content_type: Option<String>,
    /// Raw response body bytes.
    pub(crate) body: Vec<u8>,
    /// All response headers as `(name, value)` pairs.
    #[expect(dead_code, reason = "available for future comparator extensions")]
    pub(crate) headers: Vec<(String, String)>,
}

/// Returns a human-readable multi-line string describing the request.
pub(crate) fn format_request(req: &FuzzRequest) -> String {
    let mut s = format!("{} {}", req.method, req.path);
    if !req.query.is_empty() {
        let qs: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
        s.push_str(&format!("?{}", qs.join("&")));
    }
    if let Some(ct) = &req.content_type {
        s.push_str(&format!("\n  Content-Type: {ct}"));
    }
    for (k, v) in &req.headers {
        s.push_str(&format!("\n  {k}: {v}"));
    }
    if let Some(body) = &req.body {
        if let Ok(text) = std::str::from_utf8(body) {
            s.push_str(&format!("\n  Body: {text}"));
        } else {
            s.push_str(&format!("\n  Body: <{} bytes>", body.len()));
        }
    }
    s
}

/// Returns a human-readable multi-line string describing the response.
#[expect(dead_code, reason = "available for future test output improvements")]
pub(crate) fn format_response(resp: &CapturedResponse) -> String {
    let mut s = format!("HTTP {}", resp.status);
    if let Some(ct) = &resp.content_type {
        s.push_str(&format!("\n  Content-Type: {ct}"));
    }
    if let Ok(text) = std::str::from_utf8(&resp.body) {
        if !text.is_empty() {
            s.push_str(&format!("\n  Body: {text}"));
        }
    } else {
        s.push_str(&format!("\n  Body: <{} bytes>", resp.body.len()));
    }
    s
}

/// Send `req` to `base_url` using `client` and return a [`CapturedResponse`].
///
/// The full request URL is built as `{base_url}{req.path}[?query_string]`.
/// All headers in `req.headers` and `req.content_type` are forwarded.
/// The body in `req.body` is attached when present.
pub(crate) async fn send_request(
    client: &hpx::Client,
    base_url: &str,
    req: &FuzzRequest,
) -> Result<CapturedResponse, Box<dyn std::error::Error>> {
    // Build URL with optional query string.
    let mut url = format!("{base_url}{}", req.path);
    if !req.query.is_empty() {
        let qs = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        url.push('?');
        url.push_str(&qs);
    }

    // Dispatch to the correct HTTP method.
    let builder = match req.method {
        http::Method::GET => client.get(&url),
        http::Method::POST => client.post(&url),
        http::Method::PUT => client.put(&url),
        http::Method::DELETE => client.delete(&url),
        http::Method::PATCH => client.patch(&url),
        _ => client.get(&url),
    };

    // Inject request headers.
    let mut builder = builder;
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    if let Some(ct) = &req.content_type {
        builder = builder.header("content-type", ct);
    }

    // Attach body when present.
    let builder = if let Some(body) = &req.body { builder.body(body.clone()) } else { builder };

    let response = builder.send().await?;

    let status = response.status().as_u16();

    // Collect headers and content-type before consuming the response for body.
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase());

    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let body = response.bytes().await?.to_vec();

    Ok(CapturedResponse { status, content_type, body, headers })
}
