//! Minimal OpenAPI 3.0/3.1 runtime parser and request/response engine.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use http::{HeaderMap, Method};
use serde_json::{Map, Value};
use specmock_core::{
    ValidationIssue,
    faker::generate_json_value,
    ref_resolver::{RefResolver, resolve_pointer},
    validate::validate_instance,
};

use super::router::{PathRouter, RouteMatch};
use crate::RuntimeError;

/// Loaded OpenAPI runtime.
#[derive(Debug, Clone)]
pub struct OpenApiRuntime {
    operations: Vec<OperationSpec>,
    router: PathRouter,
}

/// Resolved operation and path parameters.
#[derive(Debug)]
pub struct MatchedOperation<'a> {
    /// Operation definition.
    pub operation: &'a OperationSpec,
    /// Extracted path parameters.
    pub path_params: HashMap<String, String>,
}

/// Operation model.
#[derive(Debug, Clone)]
pub struct OperationSpec {
    /// HTTP method.
    pub method: Method,
    /// Path template.
    pub path_template: String,
    /// Operation id (if present).
    pub operation_id: Option<String>,
    /// Parameters.
    pub parameters: Vec<ParameterSpec>,
    /// Request body schema for the `application/json` media type.
    pub request_body_schema: Option<Value>,
    /// Whether request body is required.
    pub request_body_required: bool,
    /// Every media type declared by the request body, in declaration order.
    pub request_body_media_types: Vec<String>,
    /// Declared responses.
    pub responses: Vec<ResponseSpec>,
    /// OpenAPI callbacks (outbound requests fired after response).
    pub callbacks: Vec<CallbackSpec>,
}

/// Callback specification parsed from OpenAPI `callbacks`.
#[derive(Debug, Clone)]
pub struct CallbackSpec {
    /// Runtime expression for the callback URL, e.g. `"{$request.body#/callbackUrl}/notify"`.
    pub callback_url_expression: String,
    /// HTTP method for the outbound request.
    pub method: Method,
    /// Optional JSON schema for the callback request body.
    pub request_body_schema: Option<Value>,
}

/// Parameter location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterIn {
    /// Path parameter.
    Path,
    /// Query parameter.
    Query,
    /// Header parameter.
    Header,
}

/// Parameter spec.
#[derive(Debug, Clone)]
pub struct ParameterSpec {
    /// Parameter name.
    pub name: String,
    /// Location.
    pub location: ParameterIn,
    /// Required flag.
    pub required: bool,
    /// Schema.
    pub schema: Value,
}

/// Request body model parsed from OpenAPI `requestBody`.
#[derive(Debug, Clone)]
struct RequestBodySpec {
    /// JSON schema of the `application/json` media type, if declared.
    schema: Option<Value>,
    /// Whether the request body is required.
    required: bool,
    /// Every declared media type, in declaration order.
    media_types: Vec<String>,
}

/// Response spec.
#[derive(Debug, Clone)]
pub struct ResponseSpec {
    /// Status selector (`200`, `default`).
    pub status: String,
    /// Declared media types, JSON-family entries first, then declaration order.
    pub content: Vec<MediaTypeSpec>,
    /// Declared response headers.
    pub headers: BTreeMap<String, ResponseHeaderSpec>,
}

/// One entry of an OpenAPI response `content` map.
#[derive(Debug, Clone)]
pub struct MediaTypeSpec {
    /// Media type name, e.g. `application/json`.
    pub media_type: String,
    /// JSON schema for this media type.
    pub schema: Option<Value>,
    /// Explicit example for this media type.
    pub example: Option<Value>,
    /// Named examples keyed by example name.
    pub named_examples: BTreeMap<String, Value>,
}

/// A declared response header.
#[derive(Debug, Clone)]
pub struct ResponseHeaderSpec {
    /// JSON schema for the header value.
    pub schema: Option<Value>,
    /// Explicit example for the header value.
    pub example: Option<Value>,
}

impl MediaTypeSpec {
    /// Resolve the payload for this media type.
    ///
    /// Priority: `example` → first named example → faker over `schema`.
    fn resolve_body(&self, seed: u64) -> Result<Option<Value>, RuntimeError> {
        if let Some(example) = &self.example {
            return Ok(Some(example.clone()));
        }
        if let Some(example) = self.named_examples.values().next() {
            return Ok(Some(example.clone()));
        }
        match &self.schema {
            Some(schema) => {
                let value = generate_json_value(schema, seed)
                    .map_err(|error| RuntimeError::Parse(error.to_string()))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }
}

impl ResponseSpec {
    /// Media types this response can produce, in preference order.
    #[must_use]
    pub fn media_types(&self) -> Vec<String> {
        self.content.iter().map(|entry| entry.media_type.clone()).collect()
    }

    /// The preferred media type, preferring the JSON family.
    #[must_use]
    pub fn primary_media_type(&self) -> Option<&MediaTypeSpec> {
        self.content.first()
    }

    /// Look up a declared media type entry.
    #[must_use]
    pub fn media_type_entry(&self, media_type: &str) -> Option<&MediaTypeSpec> {
        self.content.iter().find(|entry| entry.media_type == media_type)
    }

    /// JSON schema used for payload generation and proxy validation.
    ///
    /// Prefers a JSON-family media type and falls back to the first declared
    /// entry so non-JSON responses still expose their schema.
    #[must_use]
    pub fn json_schema(&self) -> Option<&Value> {
        self.content
            .iter()
            .find(|entry| super::media::is_json_media_type(&entry.media_type))
            .or_else(|| self.content.first())
            .and_then(|entry| entry.schema.as_ref())
    }
}

/// Generated response.
#[derive(Debug, Clone)]
pub struct MockHttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Negotiated media type, absent when the response has no body.
    pub media_type: Option<String>,
    /// JSON schema the body was generated from, absent for bodyless responses.
    pub schema: Option<Value>,
    /// Mock payload, kept as JSON regardless of the wire encoding.
    pub body: Option<Value>,
    /// Declared response headers to emit.
    pub headers: Vec<(String, String)>,
}

impl OpenApiRuntime {
    /// Load OpenAPI document from path.
    ///
    /// The file is loaded, all `$ref` nodes are resolved via [`RefResolver`],
    /// and the fully-inlined document is then parsed into operation specs.
    pub fn from_path(path: &Path) -> Result<Self, RuntimeError> {
        let base_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        let mut resolver = RefResolver::new(base_dir);
        let resolved =
            resolver.resolve(path).map_err(|error| RuntimeError::Parse(error.to_string()))?;
        Self::from_resolved(resolved)
    }

    /// Build from an already-resolved OpenAPI document value.
    ///
    /// The caller must ensure that all `$ref` nodes have been inlined before
    /// invoking this constructor.
    pub fn from_resolved(root: Value) -> Result<Self, RuntimeError> {
        let version = root
            .get("openapi")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::Parse("openapi version field missing".to_owned()))?;
        if !(version.starts_with("3.0") || version.starts_with("3.1")) {
            return Err(RuntimeError::Parse(format!(
                "unsupported openapi version: {version}, expected 3.0.x or 3.1.x"
            )));
        }

        let paths = root
            .get("paths")
            .and_then(Value::as_object)
            .ok_or_else(|| RuntimeError::Parse("openapi paths object missing".to_owned()))?;

        let mut operations = Vec::new();
        for (path_template, path_item) in paths {
            let Some(path_object) = path_item.as_object() else {
                continue;
            };
            let inherited_parameters = parse_parameters(path_object.get("parameters"), version)?;

            for method_name in ["get", "post", "put", "patch", "delete", "head", "options", "trace"]
            {
                let Some(operation_value) = path_object.get(method_name) else {
                    continue;
                };
                let Some(operation_object) = operation_value.as_object() else {
                    continue;
                };

                let mut parameters = inherited_parameters.clone();
                let operation_params =
                    parse_parameters(operation_object.get("parameters"), version)?;
                for parameter in operation_params {
                    parameters.retain(|existing| {
                        existing.location != parameter.location || existing.name != parameter.name
                    });
                    parameters.push(parameter);
                }

                let request_body = parse_request_body(operation_object, version)?;
                let responses = parse_responses(operation_object, version)?;
                let callbacks = parse_callbacks(operation_object, version)?;

                let method_name_upper = method_name.to_ascii_uppercase();
                let method = Method::from_bytes(method_name_upper.as_bytes())
                    .map_err(|error| RuntimeError::Parse(error.to_string()))?;
                operations.push(OperationSpec {
                    method,
                    path_template: path_template.clone(),
                    operation_id: operation_object
                        .get("operationId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    parameters,
                    request_body_schema: request_body.schema,
                    request_body_required: request_body.required,
                    request_body_media_types: request_body.media_types,
                    responses,
                    callbacks,
                });
            }
        }

        let router = PathRouter::build(&operations);
        Ok(Self { operations, router })
    }

    /// Match operation by method and path.
    pub fn match_operation<'a>(
        &'a self,
        method: &Method,
        path: &str,
    ) -> Option<MatchedOperation<'a>> {
        let RouteMatch { operation_index, path_params } = self.router.match_route(method, path)?;
        Some(MatchedOperation { operation: &self.operations[operation_index], path_params })
    }
}

impl OperationSpec {
    /// Validate request parts.
    ///
    /// `body_present` reports whether the request carried any payload at all,
    /// which is independent of `body_json`: a non-JSON body of a declared media
    /// type satisfies `required` without being schema-validated.
    pub fn validate_request(
        &self,
        path_params: &HashMap<String, String>,
        query_params: &HashMap<String, Vec<String>>,
        headers: &HeaderMap,
        body_json: Option<&Value>,
        body_present: bool,
    ) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        for parameter in &self.parameters {
            match parameter.location {
                ParameterIn::Path => {
                    let raw = path_params.get(&parameter.name).cloned();
                    if parameter.required && raw.is_none() {
                        issues.push(ValidationIssue {
                            instance_pointer: format!("/{}", parameter.name),
                            schema_pointer: "#/parameters".to_owned(),
                            keyword: "required".to_owned(),
                            message: format!("missing required parameter '{}'", parameter.name),
                        });
                        continue;
                    }
                    if let Some(raw_value) = raw {
                        let parsed_value = parse_parameter_value(&raw_value, &parameter.schema);
                        match validate_instance(&parameter.schema, &parsed_value) {
                            Ok(mut parameter_issues) => issues.append(&mut parameter_issues),
                            Err(error) => issues.push(ValidationIssue {
                                instance_pointer: format!("/{}", parameter.name),
                                schema_pointer: "#/parameters".to_owned(),
                                keyword: "schema".to_owned(),
                                message: error.to_string(),
                            }),
                        }
                    }
                }
                ParameterIn::Query => {
                    let values = query_params.get(&parameter.name);
                    let is_missing = values.is_none_or(Vec::is_empty);

                    if parameter.required && is_missing {
                        issues.push(ValidationIssue {
                            instance_pointer: format!("/{}", parameter.name),
                            schema_pointer: "#/parameters".to_owned(),
                            keyword: "required".to_owned(),
                            message: format!("missing required parameter '{}'", parameter.name),
                        });
                        continue;
                    }

                    if let Some(vals) = values &&
                        !vals.is_empty()
                    {
                        let is_array = schema_type_is_array(&parameter.schema);
                        if is_array {
                            // Collect all values into a JSON array, parsing each
                            // element against the items sub-schema.
                            let items_schema = parameter
                                .schema
                                .get("items")
                                .cloned()
                                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                            let elements: Vec<Value> = vals
                                .iter()
                                .map(|v| parse_parameter_value(v, &items_schema))
                                .collect();
                            let parsed_value = Value::Array(elements);
                            match validate_instance(&parameter.schema, &parsed_value) {
                                Ok(mut parameter_issues) => {
                                    issues.append(&mut parameter_issues);
                                }
                                Err(error) => issues.push(ValidationIssue {
                                    instance_pointer: format!("/{}", parameter.name),
                                    schema_pointer: "#/parameters".to_owned(),
                                    keyword: "schema".to_owned(),
                                    message: error.to_string(),
                                }),
                            }
                        } else {
                            // Non-array: use first value.
                            let raw_value = &vals[0];
                            let parsed_value = parse_parameter_value(raw_value, &parameter.schema);
                            match validate_instance(&parameter.schema, &parsed_value) {
                                Ok(mut parameter_issues) => {
                                    issues.append(&mut parameter_issues);
                                }
                                Err(error) => issues.push(ValidationIssue {
                                    instance_pointer: format!("/{}", parameter.name),
                                    schema_pointer: "#/parameters".to_owned(),
                                    keyword: "schema".to_owned(),
                                    message: error.to_string(),
                                }),
                            }
                        }
                    }
                }
                ParameterIn::Header => {
                    let raw = headers
                        .get(&parameter.name)
                        .and_then(|value| value.to_str().ok())
                        .map(ToOwned::to_owned);
                    if parameter.required && raw.is_none() {
                        issues.push(ValidationIssue {
                            instance_pointer: format!("/{}", parameter.name),
                            schema_pointer: "#/parameters".to_owned(),
                            keyword: "required".to_owned(),
                            message: format!("missing required parameter '{}'", parameter.name),
                        });
                        continue;
                    }
                    if let Some(raw_value) = raw {
                        let parsed_value = parse_parameter_value(&raw_value, &parameter.schema);
                        match validate_instance(&parameter.schema, &parsed_value) {
                            Ok(mut parameter_issues) => issues.append(&mut parameter_issues),
                            Err(error) => issues.push(ValidationIssue {
                                instance_pointer: format!("/{}", parameter.name),
                                schema_pointer: "#/parameters".to_owned(),
                                keyword: "schema".to_owned(),
                                message: error.to_string(),
                            }),
                        }
                    }
                }
            }
        }

        if self.request_body_required && !body_present {
            issues.push(ValidationIssue {
                instance_pointer: "/body".to_owned(),
                schema_pointer: "#/requestBody".to_owned(),
                keyword: "required".to_owned(),
                message: "missing required request body".to_owned(),
            });
        }

        if let (Some(schema), Some(body)) = (&self.request_body_schema, body_json) {
            match validate_instance(schema, body) {
                Ok(mut body_issues) => issues.append(&mut body_issues),
                Err(error) => issues.push(ValidationIssue {
                    instance_pointer: "/body".to_owned(),
                    schema_pointer: "#/requestBody".to_owned(),
                    keyword: "schema".to_owned(),
                    message: error.to_string(),
                }),
            }
        }

        issues
    }

    /// Build a mocked response from OpenAPI response entries.
    ///
    /// The caller supplies [`PreferDirectives`] parsed from the request so the
    /// engine can honour `Prefer: code=…`, `Prefer: example=…`,
    /// `Prefer: dynamic=true`, and `Accept` content negotiation.
    pub fn mock_response(
        &self,
        seed: u64,
        prefer: &super::negotiate::PreferDirectives,
    ) -> Result<MockHttpResponse, RuntimeError> {
        let selected = super::negotiate::select_response(&self.responses, prefer)
            .ok_or_else(|| RuntimeError::NotFound("preferred code not found".to_owned()))?;

        // Content negotiation: an `Accept` header that matches nothing declared
        // is unsatisfiable and must not silently fall back to another type.
        // Responses without any declared content (`204`, headers-only errors)
        // stay bodyless instead of failing negotiation.
        let available = selected.media_types();
        let negotiated = if available.is_empty() {
            None
        } else {
            let media_type = super::negotiate::negotiate_media_type(
                &available,
                prefer.media_type.as_deref(),
            )
            .ok_or_else(|| {
                RuntimeError::NotAcceptable(format!(
                    "no acceptable representation for Accept header; available media types: {}",
                    available.join(", ")
                ))
            })?;

            let entry = selected.media_type_entry(&media_type).ok_or_else(|| {
                RuntimeError::NotAcceptable(format!("media type not declared: {media_type}"))
            })?;

            // Named example override.  An explicitly requested example that does
            // not exist is an error rather than a silent fallback, matching `code=`.
            let body = if let Some(name) = &prefer.example {
                match entry.named_examples.get(name) {
                    Some(value) => Some(value.clone()),
                    None => {
                        return Err(RuntimeError::NotFound(format!(
                            "example '{name}' not found for media type '{media_type}'"
                        )));
                    }
                }
            } else if prefer.dynamic {
                // Dynamic mode: always use the faker even when a static example exists.
                match &entry.schema {
                    Some(schema) => Some(
                        generate_json_value(schema, seed)
                            .map_err(|error| RuntimeError::Parse(error.to_string()))?,
                    ),
                    None => None,
                }
            } else {
                entry.resolve_body(seed)?
            };

            Some((media_type, entry.schema.clone(), body))
        };

        let (media_type, schema, body) = match negotiated {
            Some((media_type, schema, body)) => (Some(media_type), schema, body),
            None => (None, None, None),
        };

        Ok(MockHttpResponse {
            status: parse_status_code(&selected.status),
            media_type,
            schema,
            headers: generate_response_headers(&selected.headers, seed),
            body,
        })
    }

    /// Retrieve response schema by concrete status code with default fallback.
    pub fn response_schema_for_status(&self, status: u16) -> Option<&Value> {
        let status_text = status.to_string();
        if let Some(exact) = self
            .responses
            .iter()
            .find(|response| response.status == status_text)
            .and_then(ResponseSpec::json_schema)
        {
            return Some(exact);
        }
        self.responses
            .iter()
            .find(|response| response.status == "default")
            .and_then(ResponseSpec::json_schema)
    }
}

/// Render declared response headers into concrete values.
///
/// Each header value follows the same priority as response bodies:
/// `example` → faker over `schema`.  Determinism is preserved by deriving a
/// per-header seed from the response seed and header name.
fn generate_response_headers(
    headers: &BTreeMap<String, ResponseHeaderSpec>,
    seed: u64,
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, spec)| {
            let value = match &spec.example {
                Some(example) => Some(example.clone()),
                None => spec.schema.as_ref().and_then(|schema| {
                    let header_seed = crate::deterministic_hash(seed, name);
                    generate_json_value(schema, header_seed).ok()
                }),
            }?;
            Some((name.clone(), header_value_to_string(&value)))
        })
        .collect()
}

fn header_value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_parameters(
    parameters_node: Option<&Value>,
    openapi_version: &str,
) -> Result<Vec<ParameterSpec>, RuntimeError> {
    let Some(parameters_array) = parameters_node.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut parameters = Vec::new();
    for parameter_node in parameters_array {
        let Some(parameter_object) = parameter_node.as_object() else {
            continue;
        };
        let Some(name) = parameter_object.get("name").and_then(Value::as_str) else {
            continue;
        };

        let location = match parameter_object.get("in").and_then(Value::as_str) {
            Some("path") => ParameterIn::Path,
            Some("query") => ParameterIn::Query,
            Some("header") => ParameterIn::Header,
            _ => continue,
        };

        let required = parameter_object.get("required").and_then(Value::as_bool).unwrap_or(false) ||
            location == ParameterIn::Path;

        let schema = parameter_object
            .get("schema")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        let mut normalized = Value::Object(schema);
        normalize_schema(&mut normalized, openapi_version.starts_with("3.0"));

        parameters.push(ParameterSpec {
            name: name.to_owned(),
            location,
            required,
            schema: normalized,
        });
    }
    Ok(parameters)
}

fn parse_request_body(
    operation: &Map<String, Value>,
    openapi_version: &str,
) -> Result<RequestBodySpec, RuntimeError> {
    let empty = |required| RequestBodySpec { schema: None, required, media_types: Vec::new() };

    let Some(request_body) = operation.get("requestBody").and_then(Value::as_object) else {
        return Ok(empty(false));
    };

    let required = request_body.get("required").and_then(Value::as_bool).unwrap_or(false);

    let Some(content) = request_body.get("content").and_then(Value::as_object) else {
        return Ok(empty(required));
    };

    let media_types: Vec<String> = content.keys().cloned().collect();

    // Only `application/json` payloads are schema-validated; other media types
    // are accepted for `Content-Type` purposes but not parsed.
    let schema = content
        .get("application/json")
        .and_then(Value::as_object)
        .and_then(|media_type| media_type.get("schema").and_then(Value::as_object))
        .map(|schema| {
            let mut schema_value = Value::Object(schema.clone());
            normalize_schema(&mut schema_value, openapi_version.starts_with("3.0"));
            schema_value
        });

    Ok(RequestBodySpec { schema, required, media_types })
}

/// Parse an OpenAPI `example` / `examples` pair into a concrete payload.
fn parse_media_type_examples(
    media_type: &Map<String, Value>,
) -> (Option<Value>, BTreeMap<String, Value>) {
    let mut named_examples = BTreeMap::new();
    if let Some(examples_obj) = media_type.get("examples").and_then(Value::as_object) {
        for (example_name, example_entry) in examples_obj {
            // A named example may either be a bare value or an Example Object.
            let value =
                example_entry.get("value").cloned().unwrap_or_else(|| example_entry.clone());
            named_examples.insert(example_name.clone(), value);
        }
    }
    let example =
        media_type.get("example").cloned().or_else(|| named_examples.values().next().cloned());
    (example, named_examples)
}

fn parse_responses(
    operation: &Map<String, Value>,
    openapi_version: &str,
) -> Result<Vec<ResponseSpec>, RuntimeError> {
    let Some(responses_node) = operation.get("responses").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };

    let mut responses = Vec::new();
    for (status, response_node) in responses_node {
        let Some(response_object) = response_node.as_object() else {
            continue;
        };

        // Parse every declared media type, not just `application/json`: specs
        // commonly serve `text/plain`, `application/xml`, or binary payloads.
        let mut content: Vec<MediaTypeSpec> = Vec::new();
        if let Some(content_map) = response_object.get("content").and_then(Value::as_object) {
            for (media_type, media_type_node) in content_map {
                let Some(media_object) = media_type_node.as_object() else {
                    continue;
                };
                let schema =
                    media_object.get("schema").and_then(Value::as_object).map(|schema_object| {
                        let mut s = Value::Object(schema_object.clone());
                        normalize_schema(&mut s, openapi_version.starts_with("3.0"));
                        s
                    });
                let (example, named_examples) = parse_media_type_examples(media_object);
                content.push(MediaTypeSpec {
                    media_type: media_type.clone(),
                    schema,
                    example,
                    named_examples,
                });
            }
        }

        // Prefer the JSON family so `Accept`-less requests keep receiving JSON.
        content.sort_by_key(|entry| !super::media::is_json_media_type(&entry.media_type));

        let headers = parse_response_headers(response_object, openapi_version);

        responses.push(ResponseSpec { status: status.clone(), content, headers });
    }

    Ok(responses)
}

fn parse_response_headers(
    response_object: &Map<String, Value>,
    openapi_version: &str,
) -> BTreeMap<String, ResponseHeaderSpec> {
    let mut headers = BTreeMap::new();
    let Some(headers_node) = response_object.get("headers").and_then(Value::as_object) else {
        return headers;
    };

    for (name, header_node) in headers_node {
        let Some(header_object) = header_node.as_object() else {
            continue;
        };
        let schema = header_object.get("schema").and_then(Value::as_object).map(|schema_object| {
            let mut s = Value::Object(schema_object.clone());
            normalize_schema(&mut s, openapi_version.starts_with("3.0"));
            s
        });
        let example = header_object.get("example").cloned().or_else(|| {
            header_object.get("examples").and_then(Value::as_object).and_then(|examples| {
                examples.values().next().and_then(|entry| entry.get("value").cloned())
            })
        });
        headers.insert(name.clone(), ResponseHeaderSpec { schema, example });
    }

    headers
}

fn parse_callbacks(
    operation: &Map<String, Value>,
    openapi_version: &str,
) -> Result<Vec<CallbackSpec>, RuntimeError> {
    let Some(callbacks_node) = operation.get("callbacks").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };

    let mut callbacks = Vec::new();
    // Each entry: callbackName -> { expressionUrl -> pathItemObject }
    for (_callback_name, callback_value) in callbacks_node {
        let Some(callback_object) = callback_value.as_object() else {
            continue;
        };
        for (url_expression, path_item_value) in callback_object {
            let Some(path_item) = path_item_value.as_object() else {
                continue;
            };
            for method_name in ["get", "post", "put", "patch", "delete", "head", "options", "trace"]
            {
                let Some(cb_operation) = path_item.get(method_name).and_then(Value::as_object)
                else {
                    continue;
                };

                let method_upper = method_name.to_ascii_uppercase();
                let method = Method::from_bytes(method_upper.as_bytes())
                    .map_err(|error| RuntimeError::Parse(error.to_string()))?;

                let schema = cb_operation
                    .get("requestBody")
                    .and_then(|rb| rb.get("content"))
                    .and_then(Value::as_object)
                    .and_then(|content| {
                        content.get("application/json").and_then(Value::as_object).cloned()
                    })
                    .and_then(|media| media.get("schema").and_then(Value::as_object).cloned())
                    .map(|s| {
                        let mut sv = Value::Object(s);
                        normalize_schema(&mut sv, openapi_version.starts_with("3.0"));
                        sv
                    });

                callbacks.push(CallbackSpec {
                    callback_url_expression: url_expression.clone(),
                    method,
                    request_body_schema: schema,
                });
            }
        }
    }

    Ok(callbacks)
}

/// Resolve a callback URL runtime expression against the original request body.
///
/// Supports the `{$request.body#/jsonPointer}` syntax defined in OpenAPI 3.x.
/// Literal text outside `{…}` is preserved as-is.
pub fn resolve_callback_url(expression: &str, request_body: Option<&Value>) -> Option<String> {
    let mut result = String::with_capacity(expression.len());
    let mut remaining = expression;

    while let Some(open) = remaining.find('{') {
        result.push_str(&remaining[..open]);
        let after_open = &remaining[open + 1..];
        let close = after_open.find('}')?;
        let token = &after_open[..close];
        remaining = &after_open[close + 1..];

        if let Some(pointer_path) = token.strip_prefix("$request.body#") &&
            let Some(body) = request_body &&
            let Some(value) = resolve_pointer(body, pointer_path)
        {
            let text = value.as_str().map_or_else(|| value.to_string(), ToOwned::to_owned);
            result.push_str(&text);
            continue;
        }
        result.push('{');
        result.push_str(token);
        result.push('}');
    }
    result.push_str(remaining);

    if result.is_empty() { None } else { Some(result) }
}

fn normalize_schema(schema: &mut Value, use_nullable_transform: bool) {
    if let Some(object) = schema.as_object_mut() {
        for nested_key in ["properties", "$defs", "definitions"] {
            if let Some(properties) = object.get_mut(nested_key).and_then(Value::as_object_mut) {
                for value in properties.values_mut() {
                    normalize_schema(value, use_nullable_transform);
                }
            }
        }

        for nested_key in ["items", "additionalProperties", "not"] {
            if let Some(value) = object.get_mut(nested_key) {
                normalize_schema(value, use_nullable_transform);
            }
        }

        for nested_key in ["allOf", "anyOf", "oneOf"] {
            if let Some(items) = object.get_mut(nested_key).and_then(Value::as_array_mut) {
                for item in items {
                    normalize_schema(item, use_nullable_transform);
                }
            }
        }

        if use_nullable_transform &&
            object.get("nullable").and_then(Value::as_bool).unwrap_or(false) &&
            let Some(type_value) = object.get_mut("type")
        {
            match type_value {
                Value::String(original_type) => {
                    *type_value = Value::Array(vec![
                        Value::String(original_type.clone()),
                        Value::String("null".to_owned()),
                    ]);
                }
                Value::Array(types) => {
                    let has_null = types.iter().any(|item| item == "null");
                    if !has_null {
                        types.push(Value::String("null".to_owned()));
                    }
                }
                _value => {}
            }
            object.remove("nullable");
        }
    }
}

/// Returns `true` when the schema's `type` field is (or includes) `"array"`.
fn schema_type_is_array(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => t == "array",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("array")),
        _ => false,
    }
}

fn parse_parameter_value(raw: &str, schema: &Value) -> Value {
    let inferred_type = schema
        .get("type")
        .and_then(|value| {
            value.as_str().map(ToOwned::to_owned).or_else(|| {
                value.as_array().and_then(|types| {
                    types.iter().find_map(|entry| entry.as_str().map(ToOwned::to_owned))
                })
            })
        })
        .unwrap_or_else(|| "string".to_owned());

    match inferred_type.as_str() {
        "integer" => {
            raw.parse::<i64>().map_or_else(|_error| Value::String(raw.to_owned()), Value::from)
        }
        "number" => {
            raw.parse::<f64>().map_or_else(|_error| Value::String(raw.to_owned()), Value::from)
        }
        "boolean" => {
            raw.parse::<bool>().map_or_else(|_error| Value::String(raw.to_owned()), Value::from)
        }
        _ => Value::String(raw.to_owned()),
    }
}

fn parse_status_code(status: &str) -> u16 {
    if status == "default" {
        return 200;
    }
    status.parse::<u16>().unwrap_or(200)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use http::{HeaderMap, Method};
    use serde_json::json;

    use super::OpenApiRuntime;

    /// Build a runtime from an inline OpenAPI document.
    fn build_runtime(document: serde_json::Value) -> OpenApiRuntime {
        OpenApiRuntime::from_resolved(document).expect("runtime should parse")
    }

    #[test]
    fn operation_level_parameter_overrides_path_level_parameter() {
        let root = json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets/{id}": {
                    "parameters": [
                        {
                            "name": "id",
                            "in": "path",
                            "required": true,
                            "schema": {"type": "integer"}
                        }
                    ],
                    "get": {
                        "parameters": [
                            {
                                "name": "id",
                                "in": "path",
                                "required": true,
                                "schema": {
                                    "type": "string",
                                    "pattern": "^[a-z]+$"
                                }
                            }
                        ],
                        "responses": {
                            "200": {
                                "description": "ok"
                            }
                        }
                    }
                }
            }
        });

        let runtime = OpenApiRuntime::from_resolved(root).expect("runtime should parse");

        let alpha = runtime
            .match_operation(&Method::GET, "/pets/abc")
            .expect("operation should match alpha path");
        let alpha_issues = alpha.operation.validate_request(
            &alpha.path_params,
            &HashMap::new(),
            &HeaderMap::new(),
            None,
            false,
        );
        assert!(alpha_issues.is_empty(), "operation-level schema should accept alpha id");

        let numeric = runtime
            .match_operation(&Method::GET, "/pets/123")
            .expect("operation should match numeric path");
        let numeric_issues = numeric.operation.validate_request(
            &numeric.path_params,
            &HashMap::new(),
            &HeaderMap::new(),
            None,
            false,
        );
        assert!(!numeric_issues.is_empty(), "operation-level pattern should reject numeric id");
    }

    #[test]
    fn non_json_content_type_returns_no_schema() {
        let operation = serde_json::json!({
            "requestBody": {
                "content": {
                    "application/xml": {
                        "schema": {"type": "string"}
                    }
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let parsed = super::parse_request_body(&operation, "3.1.0").unwrap();
        assert!(parsed.schema.is_none(), "non-JSON content type should not produce a JSON schema");
        assert_eq!(parsed.media_types, vec!["application/xml".to_owned()]);
    }

    #[test]
    fn request_body_records_every_declared_media_type() {
        let operation = serde_json::json!({
            "requestBody": {
                "required": true,
                "content": {
                    "application/json": {"schema": {"type": "object"}},
                    "application/xml": {"schema": {"type": "string"}}
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let parsed = super::parse_request_body(&operation, "3.1.0").unwrap();
        assert!(parsed.required);
        assert_eq!(
            parsed.media_types,
            vec!["application/json".to_owned(), "application/xml".to_owned()]
        );
    }

    fn response_from_yaml_ish(json: serde_json::Value) -> super::ResponseSpec {
        let operation = json.as_object().unwrap().clone();
        let mut responses = super::parse_responses(&operation, "3.1.0").unwrap();
        responses.remove(0)
    }

    #[test]
    fn response_keeps_all_declared_media_types_with_json_first() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "description": "ok",
                    "content": {
                        "text/plain": {"schema": {"type": "string"}, "example": "hi"},
                        "application/json": {
                            "schema": {"type": "object"},
                            "example": {"a": 1}
                        }
                    }
                }
            }
        }));

        assert_eq!(
            response.media_types(),
            vec!["application/json".to_owned(), "text/plain".to_owned()],
            "JSON media type must be preferred for Accept-less requests"
        );
        assert_eq!(
            response.primary_media_type().map(|m| m.media_type.as_str()),
            Some("application/json")
        );
    }

    #[test]
    fn response_media_type_lookup_by_name() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "description": "ok",
                    "content": {
                        "application/xml": {"schema": {"type": "object"}},
                        "application/json": {"schema": {"type": "object"}}
                    }
                }
            }
        }));

        let xml = response.media_type_entry("application/xml").expect("xml entry");
        assert!(xml.schema.is_some());
        assert!(response.media_type_entry("text/csv").is_none());
    }

    #[test]
    fn response_headers_are_parsed() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "429": {
                    "description": "rate limited",
                    "headers": {
                        "X-RateLimit-Remaining": {
                            "schema": {"type": "integer", "minimum": 0, "maximum": 10}
                        },
                        "X-Trace": {"example": "abc-123"}
                    }
                }
            }
        }));

        assert_eq!(response.headers.len(), 2);
        assert!(response.headers["X-RateLimit-Remaining"].schema.is_some());
        assert_eq!(response.headers["X-Trace"].example, Some(serde_json::json!("abc-123")));
    }

    #[test]
    fn mock_response_negotiates_requested_media_type() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/report": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "object"},
                                        "example": {"kind": "json"}
                                    },
                                    "text/plain": {
                                        "schema": {"type": "string"},
                                        "example": "plain-text"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime
            .match_operation(&Method::GET, "/report")
            .expect("operation should match")
            .operation;
        let prefer = super::super::negotiate::PreferDirectives {
            media_type: Some("text/plain".to_owned()),
            ..Default::default()
        };

        let mock = operation.mock_response(1, &prefer).expect("mock response");
        assert_eq!(mock.media_type.as_deref(), Some("text/plain"));
        assert_eq!(mock.body, Some(serde_json::json!("plain-text")));
    }

    #[test]
    fn mock_response_returns_406_when_accept_is_unsatisfiable() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/report": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {"schema": {"type": "object"}}
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime
            .match_operation(&Method::GET, "/report")
            .expect("operation should match")
            .operation;
        let prefer = super::super::negotiate::PreferDirectives {
            media_type: Some("application/xml".to_owned()),
            ..Default::default()
        };

        let error = operation.mock_response(1, &prefer).expect_err("should not be acceptable");
        assert!(
            matches!(error, crate::RuntimeError::NotAcceptable(_)),
            "expected NotAcceptable, got {error:?}"
        );
    }

    #[test]
    fn mock_response_includes_declared_response_headers() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/limited": {
                    "get": {
                        "responses": {
                            "429": {
                                "description": "rate limited",
                                "headers": {
                                    "X-RateLimit-Remaining": {
                                        "schema": {"type": "integer", "minimum": 5, "maximum": 5}
                                    },
                                    "X-Trace": {"example": "abc-123"}
                                }
                            },
                            "200": {"description": "ok"}
                        }
                    }
                }
            }
        }));

        let operation = runtime
            .match_operation(&Method::GET, "/limited")
            .expect("operation should match")
            .operation;
        let prefer =
            super::super::negotiate::PreferDirectives { code: Some(429), ..Default::default() };

        let mock = operation.mock_response(1, &prefer).expect("mock response");
        assert_eq!(mock.status, 429);
        let headers: BTreeMap<&str, &str> =
            mock.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(headers.get("X-RateLimit-Remaining"), Some(&"5"));
        assert_eq!(headers.get("X-Trace"), Some(&"abc-123"));
    }

    #[test]
    fn mock_response_returns_404_for_unknown_named_example() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "object"},
                                        "examples": {
                                            "fluffy": {"value": {"name": "Fluffy"}}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime
            .match_operation(&Method::GET, "/pets")
            .expect("operation should match")
            .operation;

        let missing = super::super::negotiate::PreferDirectives {
            example: Some("nope".to_owned()),
            ..Default::default()
        };
        let error = operation.mock_response(1, &missing).expect_err("example should be missing");
        assert!(
            matches!(error, crate::RuntimeError::NotFound(_)),
            "expected NotFound, got {error:?}"
        );

        let present = super::super::negotiate::PreferDirectives {
            example: Some("fluffy".to_owned()),
            ..Default::default()
        };
        let mock = operation.mock_response(1, &present).expect("named example should resolve");
        assert_eq!(mock.body, Some(serde_json::json!({"name": "Fluffy"})));
    }

    #[test]
    fn callback_url_keeps_non_body_tokens_literal() {
        let body = serde_json::json!({"url": "https://cb.example.com/hook"});
        let result = super::resolve_callback_url("{$request.body#/url}/{$method}", Some(&body));
        assert_eq!(result.as_deref(), Some("https://cb.example.com/hook/{$method}"));
    }

    #[test]
    fn callback_url_only_non_body_tokens() {
        let result = super::resolve_callback_url("{$method}/{$url}/notify", None);
        assert_eq!(result.as_deref(), Some("{$method}/{$url}/notify"));
    }
}
