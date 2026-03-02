// Tasks 2.1 – 2.4: OpenAPI spec parser and request generators.
use rand::{RngExt as _, SeedableRng as _};
use rand_chacha::ChaCha8Rng;

// Task 2.1: OpenAPI spec parser for fuzz test generation.

pub(crate) struct OperationInfo {
    pub(crate) method: http::Method,
    pub(crate) path_template: String,
    pub(crate) path_params: Vec<ParamInfo>,
    pub(crate) query_params: Vec<ParamInfo>,
    #[expect(dead_code, reason = "available for future header-matching generators")]
    pub(crate) header_params: Vec<ParamInfo>,
    pub(crate) request_body_schema: Option<serde_json::Value>,
    pub(crate) response_codes: Vec<u16>,
    pub(crate) named_examples: Vec<String>,
    pub(crate) content_types: Vec<String>,
}

pub(crate) struct ParamInfo {
    pub(crate) name: String,
    pub(crate) location: String, // "path", "query", "header"
    pub(crate) required: bool,
    pub(crate) schema: serde_json::Value,
}

pub(crate) struct OpenApiFuzzer {
    pub(crate) operations: Vec<OperationInfo>,
    pub(crate) seed: u64,
    pub(crate) iterations: usize,
}

impl OpenApiFuzzer {
    pub(crate) fn new(
        spec_path: &std::path::Path,
        seed: u64,
        iterations: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let spec = load_spec(spec_path)?;
        let operations = parse_spec(&spec);
        Ok(Self { operations, seed, iterations })
    }
}

/// Load an OpenAPI spec file (YAML or JSON) from disk and return it as a
/// `serde_json::Value`.
pub(crate) fn load_spec(
    spec_path: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(spec_path)?;
    let value: serde_json::Value = serde_yml::from_str(&content)?;
    Ok(value)
}

/// Parse an OpenAPI spec (as `serde_json::Value`) and return one
/// [`OperationInfo`] for every defined operation.
pub(crate) fn parse_spec(spec: &serde_json::Value) -> Vec<OperationInfo> {
    const HTTP_METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

    let Some(paths) = spec.get("paths").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    let mut operations = Vec::new();

    for (path_str, path_item) in paths {
        let Some(path_obj) = path_item.as_object() else {
            continue;
        };

        // Parameters defined at the path level (shared by all operations).
        let path_level_params: Vec<&serde_json::Value> = path_obj
            .get("parameters")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();

        for &method_str in HTTP_METHODS {
            let Some(operation) = path_obj.get(method_str).and_then(|v| v.as_object()) else {
                continue;
            };

            // Merge path-level and operation-level parameters; operation-level
            // overrides by (name, in) pair.
            let op_params: Vec<&serde_json::Value> = operation
                .get("parameters")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().collect())
                .unwrap_or_default();

            // Build merged list: start with path-level, then override with op-level.
            // Use a Vec of (key, value) to maintain insertion order while deduplicating.
            let mut param_keys: Vec<(String, String)> = Vec::new();
            let mut param_vals: Vec<&serde_json::Value> = Vec::new();
            for p in path_level_params.iter().chain(op_params.iter()) {
                let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let loc = p.get("in").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let key = (name, loc);
                if let Some(pos) = param_keys.iter().position(|k| k == &key) {
                    // op-level overrides path-level at same position
                    param_vals[pos] = p;
                } else {
                    param_keys.push(key);
                    param_vals.push(p);
                }
            }

            let mut path_params = Vec::new();
            let mut query_params = Vec::new();
            let mut header_params = Vec::new();

            for ((name, loc), param_val) in param_keys.iter().zip(param_vals.iter()) {
                let required = param_val
                    .get("required")
                    .and_then(|v| v.as_bool())
                    // OpenAPI spec: path params MUST be required.
                    .unwrap_or_else(|| loc == "path");
                let schema = param_val
                    .get("schema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                let info =
                    ParamInfo { name: name.clone(), location: loc.clone(), required, schema };
                match loc.as_str() {
                    "path" => path_params.push(info),
                    "query" => query_params.push(info),
                    "header" => header_params.push(info),
                    _ => {}
                }
            }

            // requestBody.content.application/json.schema
            let request_body_schema = operation
                .get("requestBody")
                .and_then(|rb| rb.get("content"))
                .and_then(|ct| ct.get("application/json"))
                .and_then(|aj| aj.get("schema"))
                .cloned();

            // Responses: codes, named examples, content types.
            let mut response_codes: Vec<u16> = Vec::new();
            let mut named_examples: Vec<String> = Vec::new();
            let mut content_types: Vec<String> = Vec::new();

            if let Some(responses) = operation.get("responses").and_then(|v| v.as_object()) {
                for (code_str, response_val) in responses {
                    // Skip "default" and anything that isn't a numeric status code.
                    if let Ok(code) = code_str.parse::<u16>() &&
                        !response_codes.contains(&code)
                    {
                        response_codes.push(code);
                    }

                    // Collect content types and named examples.
                    if let Some(content) = response_val.get("content").and_then(|v| v.as_object()) {
                        for (ct, ct_val) in content {
                            if !content_types.contains(ct) {
                                content_types.push(ct.clone());
                            }
                            // Named examples under `examples` key (not `example`).
                            if let Some(examples) =
                                ct_val.get("examples").and_then(|v| v.as_object())
                            {
                                for example_name in examples.keys() {
                                    if !named_examples.contains(example_name) {
                                        named_examples.push(example_name.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let method = match method_str {
                "get" => http::Method::GET,
                "post" => http::Method::POST,
                "put" => http::Method::PUT,
                "delete" => http::Method::DELETE,
                "patch" => http::Method::PATCH,
                "head" => http::Method::HEAD,
                "options" => http::Method::OPTIONS,
                // SAFETY: the slice only contains the seven values above.
                _ => unreachable!("unexpected HTTP method literal"),
            };

            operations.push(OperationInfo {
                method,
                path_template: path_str.clone(),
                path_params,
                query_params,
                header_params,
                request_body_schema,
                response_codes,
                named_examples,
                content_types,
            });
        }
    }

    operations
}

// ─── Task 2.2 helpers ────────────────────────────────────────────────────────

fn random_alphanumeric(rng: &mut ChaCha8Rng, len: usize) -> String {
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0usize..36);
            let byte = if idx < 10 { b'0' + idx as u8 } else { b'a' + idx as u8 - 10 };
            char::from(byte)
        })
        .collect()
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Generate a JSON value that is valid for the given JSON Schema fragment.
pub(crate) fn generate_value(
    schema: &serde_json::Value,
    rng: &mut ChaCha8Rng,
) -> serde_json::Value {
    // enum shortcut — applies regardless of declared type
    if let Some(enum_vals) = schema.get("enum").and_then(|v| v.as_array()) &&
        !enum_vals.is_empty()
    {
        let idx = rng.random_range(0..enum_vals.len());
        return enum_vals.get(idx).cloned().unwrap_or(serde_json::Value::Null);
    }

    match schema.get("type").and_then(|v| v.as_str()) {
        Some("integer") => {
            let min = schema.get("minimum").and_then(|v| v.as_i64()).unwrap_or(1);
            let max = schema.get("maximum").and_then(|v| v.as_i64()).unwrap_or(100).max(min);
            serde_json::Value::Number(rng.random_range(min..=max).into())
        }
        Some("number") => {
            let min = schema.get("minimum").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let raw_max = schema.get("maximum").and_then(|v| v.as_f64()).unwrap_or(100.0);
            let max = if raw_max < min { min } else { raw_max };
            let v: f64 =
                if (max - min).abs() < f64::EPSILON { min } else { rng.random_range(min..=max) };
            serde_json::Number::from_f64(v)
                .map_or(serde_json::Value::Null, serde_json::Value::Number)
        }
        Some("string") => {
            let min_len = schema.get("minLength").and_then(|v| v.as_u64()).unwrap_or(5).min(64);
            let raw_max = schema.get("maxLength").and_then(|v| v.as_u64()).unwrap_or(15);
            let max_len = raw_max.max(min_len).min(64);
            let len = rng.random_range(min_len..=max_len) as usize;
            serde_json::Value::String(random_alphanumeric(rng, len))
        }
        Some("boolean") => serde_json::Value::Bool(rng.random_bool(0.5)),
        Some("array") => {
            let item_schema = schema.get("items").cloned().unwrap_or(serde_json::Value::Null);
            let count = rng.random_range(0usize..=3);
            let items = (0..count).map(|_| generate_value(&item_schema, rng)).collect();
            serde_json::Value::Array(items)
        }
        Some("object") => {
            let mut obj = serde_json::Map::new();
            let required: Vec<String> = schema
                .get("required")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
                for name in &required {
                    let val = props
                        .get(name)
                        .map_or(serde_json::Value::Null, |prop| generate_value(prop, rng));
                    obj.insert(name.clone(), val);
                }
            }
            serde_json::Value::Object(obj)
        }
        _ => serde_json::Value::Null,
    }
}

/// Fill a path template like `/pets/{id}` with generated path-param values.
pub(crate) fn generate_path(template: &str, params: &[ParamInfo], rng: &mut ChaCha8Rng) -> String {
    let mut path = template.to_owned();
    for param in params {
        if param.location == "path" {
            let val = generate_value(&param.schema, rng);
            path = path.replace(&format!("{{{}}}", param.name), &value_to_string(&val));
        }
    }
    path
}

/// Generate query-string params.  Required params are always included;
/// optional params are included with 50 % probability.
pub(crate) fn generate_query_params(
    params: &[ParamInfo],
    rng: &mut ChaCha8Rng,
) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for param in params {
        if param.location != "query" {
            continue;
        }
        if param.required || rng.random_bool(0.5) {
            let val = generate_value(&param.schema, rng);
            result.push((param.name.clone(), value_to_string(&val)));
        }
    }
    result
}

/// Generate a JSON request body from schema.  Returns `None` if schema is `None`.
pub(crate) fn generate_body(
    schema: Option<&serde_json::Value>,
    rng: &mut ChaCha8Rng,
) -> Option<Vec<u8>> {
    let schema = schema?;
    let val = generate_value(schema, rng);
    serde_json::to_vec(&val).ok()
}

// ─── Task 2.2 – 2.4: OpenApiFuzzer request generators ────────────────────────

impl OpenApiFuzzer {
    /// Generate `self.iterations` valid [`FuzzRequest`]s per operation.
    pub(crate) fn generate_valid_requests(&self) -> Vec<super::request::FuzzRequest> {
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed);
        let mut requests = Vec::new();
        for i in 0..self.iterations {
            for op in &self.operations {
                let path = generate_path(&op.path_template, &op.path_params, &mut rng);
                let query = generate_query_params(&op.query_params, &mut rng);
                let body = generate_body(op.request_body_schema.as_ref(), &mut rng);
                let content_type = body.as_ref().map(|_| "application/json".to_owned());
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path,
                    query,
                    headers: Vec::new(),
                    body,
                    content_type,
                    description: format!("{} {} (valid #{})", op.method, op.path_template, i),
                });
            }
        }
        requests
    }

    /// Generate requests designed to trigger 4xx errors.
    pub(crate) fn generate_invalid_requests(
        &self,
        rng: &mut ChaCha8Rng,
    ) -> Vec<super::request::FuzzRequest> {
        let mut requests = Vec::new();

        for op in &self.operations {
            // 1. Non-integer value for integer path params.
            for param in &op.path_params {
                if param.schema.get("type").and_then(|v| v.as_str()) == Some("integer") {
                    let path = op.path_template.replace(&format!("{{{}}}", param.name), "abc");
                    requests.push(super::request::FuzzRequest {
                        method: op.method.clone(),
                        path,
                        query: Vec::new(),
                        headers: Vec::new(),
                        body: None,
                        content_type: None,
                        description: format!(
                            "{} {} (invalid path param: non-integer for {})",
                            op.method, op.path_template, param.name
                        ),
                    });
                }
            }

            // 2. Missing required query params (one request per required param).
            let required_query: Vec<&ParamInfo> =
                op.query_params.iter().filter(|p| p.required).collect();
            if !required_query.is_empty() {
                let path = generate_path(&op.path_template, &op.path_params, rng);
                for param in &required_query {
                    requests.push(super::request::FuzzRequest {
                        method: op.method.clone(),
                        path: path.clone(),
                        query: Vec::new(),
                        headers: Vec::new(),
                        body: None,
                        content_type: None,
                        description: format!(
                            "{} {} (missing required query param: {})",
                            op.method, op.path_template, param.name
                        ),
                    });
                }
            }

            // 3. Invalid body (missing required fields).
            if let Some(schema) = &op.request_body_schema {
                let invalid_body: &[u8] =
                    if schema.get("type").and_then(|v| v.as_str()) == Some("object") {
                        b"{}"
                    } else {
                        b"42"
                    };
                let path = generate_path(&op.path_template, &op.path_params, rng);
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path,
                    query: Vec::new(),
                    headers: Vec::new(),
                    body: Some(invalid_body.to_vec()),
                    content_type: Some("application/json".to_owned()),
                    description: format!(
                        "{} {} (invalid body: missing required fields)",
                        op.method, op.path_template
                    ),
                });
            }

            // 4. Wrong Content-Type for operations with a JSON body.
            if op.request_body_schema.is_some() {
                let path = generate_path(&op.path_template, &op.path_params, rng);
                let valid_body =
                    generate_body(op.request_body_schema.as_ref(), rng).unwrap_or_default();
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path,
                    query: Vec::new(),
                    headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
                    body: Some(valid_body),
                    content_type: Some("text/plain".to_owned()),
                    description: format!(
                        "{} {} (wrong content-type: text/plain)",
                        op.method, op.path_template
                    ),
                });
            }
        }

        // 5. Unknown paths.
        for _ in 0u8..3 {
            let rand_str = random_alphanumeric(rng, 8);
            let path = format!("/unknown-{rand_str}");
            requests.push(super::request::FuzzRequest {
                method: http::Method::GET,
                path: path.clone(),
                query: Vec::new(),
                headers: Vec::new(),
                body: None,
                content_type: None,
                description: format!("GET {path} (unknown path)"),
            });
        }

        requests
    }

    /// Generate requests with `Prefer: code=NNN` / `example=NAME` / `dynamic=true` headers.
    pub(crate) fn generate_prefer_requests(&self) -> Vec<super::request::FuzzRequest> {
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed);
        let mut requests = Vec::new();

        for op in &self.operations {
            let path = generate_path(&op.path_template, &op.path_params, &mut rng);
            let needs_body = op.method == http::Method::POST ||
                op.method == http::Method::PUT ||
                op.method == http::Method::PATCH;

            // One request per response code.
            for &code in &op.response_codes {
                let body = if needs_body {
                    generate_body(op.request_body_schema.as_ref(), &mut rng)
                } else {
                    None
                };
                let content_type = body.as_ref().map(|_| "application/json".to_owned());
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path: path.clone(),
                    query: Vec::new(),
                    headers: vec![("Prefer".to_owned(), format!("code={code}"))],
                    body,
                    content_type,
                    description: format!("{} {} (prefer code={code})", op.method, op.path_template),
                });
            }

            // One request per named example.
            for name in &op.named_examples {
                let body = if needs_body {
                    generate_body(op.request_body_schema.as_ref(), &mut rng)
                } else {
                    None
                };
                let content_type = body.as_ref().map(|_| "application/json".to_owned());
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path: path.clone(),
                    query: Vec::new(),
                    headers: vec![("Prefer".to_owned(), format!("example={name}"))],
                    body,
                    content_type,
                    description: format!(
                        "{} {} (prefer example={name})",
                        op.method, op.path_template
                    ),
                });
            }

            // One `Prefer: dynamic=true` request.
            {
                let body = if needs_body {
                    generate_body(op.request_body_schema.as_ref(), &mut rng)
                } else {
                    None
                };
                let content_type = body.as_ref().map(|_| "application/json".to_owned());
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path: path.clone(),
                    query: Vec::new(),
                    headers: vec![("Prefer".to_owned(), "dynamic=true".to_owned())],
                    body,
                    content_type,
                    description: format!(
                        "{} {} (prefer dynamic=true)",
                        op.method, op.path_template
                    ),
                });
            }
        }

        requests
    }

    /// Generate requests with `Accept` header variants, one per content type per operation.
    pub(crate) fn generate_accept_requests(&self) -> Vec<super::request::FuzzRequest> {
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed);
        let mut requests = Vec::new();

        for op in &self.operations {
            let path = generate_path(&op.path_template, &op.path_params, &mut rng);
            for ct in &op.content_types {
                requests.push(super::request::FuzzRequest {
                    method: op.method.clone(),
                    path: path.clone(),
                    query: Vec::new(),
                    headers: vec![("Accept".to_owned(), ct.clone())],
                    body: None,
                    content_type: None,
                    description: format!("{} {} (accept {ct})", op.method, op.path_template),
                });
            }
        }

        requests
    }
}

#[cfg(test)]
mod tests {
    use super::{load_spec, parse_spec};

    #[test]
    fn parse_pets_spec_extracts_one_operation() {
        let spec_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/specs/openapi-pets.yaml");
        #[expect(
            clippy::expect_used,
            reason = "test code — panic on fixture load failure is intentional"
        )]
        let spec = load_spec(&spec_path).expect("failed to load spec");
        let ops = parse_spec(&spec);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].method, http::Method::GET);
        assert_eq!(ops[0].path_template, "/pets/{id}");
        assert_eq!(ops[0].path_params.len(), 1);
        assert_eq!(ops[0].path_params[0].name, "id");
        assert!(ops[0].path_params[0].required);
        assert!(ops[0].response_codes.contains(&200u16));
    }

    #[test]
    fn generate_valid_requests_produces_requests() {
        use super::OpenApiFuzzer;
        let spec_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/specs/openapi-pets.yaml");
        #[expect(
            clippy::expect_used,
            reason = "test code — panic on fixture load failure is intentional"
        )]
        let fuzzer = OpenApiFuzzer::new(&spec_path, 42, 3).expect("failed to create fuzzer");
        let reqs = fuzzer.generate_valid_requests();
        assert_eq!(reqs.len(), 3, "expected 1 op × 3 iterations = 3 requests");
        assert_eq!(reqs[0].method, http::Method::GET);
        assert!(
            reqs[0].path.starts_with("/pets/"),
            "path should start with /pets/, got: {}",
            reqs[0].path
        );
    }
}
