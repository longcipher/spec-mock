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

    /// Returns the path template that a concrete route path would shadow.
    ///
    /// WebSocket paths are registered as explicit axum routes while OpenAPI
    /// paths are served by the router fallback, so an explicit route always
    /// wins. A declared operation beneath a WebSocket path is therefore
    /// unreachable rather than merely ambiguous. WebSocket upgrades are `GET`
    /// requests, so only `GET` operations can be shadowed.
    ///
    /// This exists so the collision can be reported at startup instead of
    /// silently swallowing the operation at request time.
    pub fn shadowed_path(&self, concrete_path: &str) -> Option<&str> {
        let RouteMatch { operation_index, .. } =
            self.router.match_route(&Method::GET, concrete_path)?;
        self.operations.get(operation_index).map(|operation| operation.path_template.as_str())
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

/// Re-render a declared `Location` header from the matched request path.
///
/// `Location` identifies the resource a request created, so in a mock it has to
/// come from the request. The two obvious alternatives are both wrong: a spec
/// `example` points at the spec author's own server, and a faker-generated
/// value is noise that merely resembles a URL. Either sends the client to a
/// resource that does not exist here, and a wrong `Location` is worse than an
/// absent one because it looks authoritative.
///
/// A `Location` the operation never declared is not added — inventing the
/// header would be a stronger claim than echoing it.
pub fn render_location_header(
    headers: &mut [(String, String)],
    path_template: &str,
    path_params: &HashMap<String, String>,
) {
    for (name, value) in headers.iter_mut() {
        if name.eq_ignore_ascii_case("location") {
            *value = substitute_path_params(path_template, path_params);
        }
    }
}

/// Substitute `{name}` placeholders in a path template with bound values.
///
/// An unbound placeholder is left verbatim rather than dropped, so a template
/// the router could not satisfy stays visible instead of silently collapsing
/// into a shorter, plausible-looking path.
fn substitute_path_params(template: &str, params: &HashMap<String, String>) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let Some(offset) = rest[open..].find('}') else {
            rendered.push_str(&rest[open..]);
            return rendered;
        };
        let name = &rest[open + 1..open + offset];
        match params.get(name) {
            Some(value) => rendered.push_str(value),
            None => {
                rendered.push('{');
                rendered.push_str(name);
                rendered.push('}');
            }
        }
        rest = &rest[open + offset + 1..];
    }

    rendered.push_str(rest);
    rendered
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
    use std::collections::{BTreeMap, BTreeSet, HashMap};

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

    // ── document validation ────────────────────────────────────────────

    #[test]
    fn missing_openapi_version_is_rejected() {
        let error = OpenApiRuntime::from_resolved(serde_json::json!({"paths": {}}))
            .expect_err("a version field is required");
        assert!(error.to_string().contains("version field missing"), "got {error}");
    }

    #[test]
    fn non_string_openapi_version_is_rejected() {
        let error = OpenApiRuntime::from_resolved(serde_json::json!({"openapi": 3, "paths": {}}))
            .expect_err("a numeric version is not a string");
        assert!(error.to_string().contains("version field missing"), "got {error}");
    }

    #[test]
    fn unsupported_openapi_version_is_rejected() {
        for version in ["2.0", "4.0.0", "swagger"] {
            let error =
                OpenApiRuntime::from_resolved(serde_json::json!({"openapi": version, "paths": {}}))
                    .expect_err("unsupported version must be rejected");
            assert!(
                error.to_string().contains("unsupported openapi version"),
                "{version}: {error}"
            );
        }
    }

    #[test]
    fn supported_openapi_versions_are_accepted() {
        for version in ["3.0.0", "3.0.3", "3.1.0", "3.1.1"] {
            let runtime =
                OpenApiRuntime::from_resolved(serde_json::json!({"openapi": version, "paths": {}}))
                    .unwrap_or_else(|error| panic!("{version} should be accepted: {error}"));
            assert!(runtime.match_operation(&Method::GET, "/anything").is_none());
        }
    }

    #[test]
    fn missing_paths_object_is_rejected() {
        let error = OpenApiRuntime::from_resolved(serde_json::json!({"openapi": "3.1.0"}))
            .expect_err("paths is required");
        assert!(error.to_string().contains("paths object missing"), "got {error}");
    }

    #[test]
    fn non_object_paths_is_rejected() {
        let error = OpenApiRuntime::from_resolved(serde_json::json!({
            "openapi": "3.1.0",
            "paths": ["/pets"]
        }))
        .expect_err("paths must be an object");
        assert!(error.to_string().contains("paths object missing"), "got {error}");
    }

    #[test]
    fn non_object_path_items_are_skipped() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/broken": "not an object",
                "/null": null,
                "/pets": {"get": {"responses": {"200": {"description": "ok"}}}}
            }
        }));

        assert!(runtime.match_operation(&Method::GET, "/broken").is_none());
        assert!(runtime.match_operation(&Method::GET, "/null").is_none());
        assert!(runtime.match_operation(&Method::GET, "/pets").is_some());
    }

    #[test]
    fn non_object_operations_are_skipped() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "get": "not an operation object",
                    "post": 42,
                    "delete": {"responses": {"200": {"description": "ok"}}}
                }
            }
        }));

        assert!(runtime.match_operation(&Method::GET, "/pets").is_none());
        assert!(runtime.match_operation(&Method::POST, "/pets").is_none());
        assert!(runtime.match_operation(&Method::DELETE, "/pets").is_some());
    }

    #[test]
    fn every_supported_method_key_is_parsed() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/thing": {
                    "get": {"responses": {"200": {"description": "ok"}}},
                    "put": {"responses": {"200": {"description": "ok"}}},
                    "patch": {"responses": {"200": {"description": "ok"}}},
                    "delete": {"responses": {"200": {"description": "ok"}}},
                    "head": {"responses": {"200": {"description": "ok"}}},
                    "options": {"responses": {"200": {"description": "ok"}}},
                    "trace": {"responses": {"200": {"description": "ok"}}}
                }
            }
        }));

        for method in [
            Method::GET,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
            Method::TRACE,
        ] {
            assert!(
                runtime.match_operation(&method, "/thing").is_some(),
                "{method} should be routed"
            );
        }
    }

    // ── parameter parsing ──────────────────────────────────────────────

    #[test]
    fn malformed_parameters_are_skipped() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets": {
                    "get": {
                        "parameters": [
                            "not an object",
                            {"in": "query"},
                            {"name": "x"},
                            {"name": "y", "in": "cookie"},
                            {"name": "limit", "in": "query"}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let matched = runtime.match_operation(&Method::GET, "/pets").expect("matched");
        let parameters = &matched.operation.parameters;
        assert_eq!(parameters.len(), 1, "only the well-formed parameter survives");
        assert_eq!(parameters[0].name, "limit");
    }

    #[test]
    fn non_array_parameters_yield_an_empty_list() {
        let parsed =
            super::parse_parameters(Some(&serde_json::json!("nope")), "3.1.0").expect("parse");
        assert!(parsed.is_empty());
        assert!(super::parse_parameters(None, "3.1.0").expect("parse").is_empty());
    }

    #[test]
    fn path_parameters_are_implicitly_required() {
        let parsed = super::parse_parameters(
            Some(&serde_json::json!([{"name": "id", "in": "path", "schema": {"type": "string"}}])),
            "3.1.0",
        )
        .expect("parse");
        assert!(parsed[0].required, "OpenAPI requires every path parameter to be required");
    }

    #[test]
    fn parameters_without_a_schema_default_to_any() {
        let parsed = super::parse_parameters(
            Some(&serde_json::json!([{"name": "q", "in": "query"}])),
            "3.1.0",
        )
        .expect("parse");
        assert!(parsed[0].schema.as_object().expect("schema object").is_empty());
        assert!(!parsed[0].required);
    }

    // ── nullable normalization ─────────────────────────────────────────

    #[test]
    fn nullable_string_type_becomes_a_union_for_openapi_30() {
        let parsed = super::parse_parameters(
            Some(&serde_json::json!([{
                "name": "nickname",
                "in": "query",
                "schema": {"type": "string", "nullable": true}
            }])),
            "3.0.3",
        )
        .expect("parse");

        assert_eq!(parsed[0].schema["type"], serde_json::json!(["string", "null"]));
        assert!(
            parsed[0].schema.get("nullable").is_none(),
            "the `nullable` keyword must be consumed by the transform"
        );
    }

    #[test]
    fn nullable_union_type_gains_null_without_duplicating() {
        let parsed = super::parse_parameters(
            Some(&serde_json::json!([{
                "name": "x",
                "in": "query",
                "schema": {"type": ["string", "null"], "nullable": true}
            }])),
            "3.0.3",
        )
        .expect("parse");
        assert_eq!(parsed[0].schema["type"], serde_json::json!(["string", "null"]));
    }

    #[test]
    fn nullable_is_ignored_for_openapi_31() {
        let parsed = super::parse_parameters(
            Some(&serde_json::json!([{
                "name": "nickname",
                "in": "query",
                "schema": {"type": "string", "nullable": true}
            }])),
            "3.1.0",
        )
        .expect("parse");
        assert_eq!(parsed[0].schema["type"], serde_json::json!("string"), "3.1 has no `nullable`");
        assert_eq!(parsed[0].schema["nullable"], serde_json::json!(true), "left untouched");
    }

    #[test]
    fn nullable_with_a_non_string_type_is_left_alone() {
        let mut schema = serde_json::json!({"type": 42, "nullable": true});
        super::normalize_schema(&mut schema, true);
        assert_eq!(schema["type"], serde_json::json!(42), "an untyped `type` is not rewritten");
        assert!(schema.get("nullable").is_none(), "the keyword is still consumed");
    }

    #[test]
    fn normalize_schema_recurses_into_every_known_keyword() {
        let mut schema = serde_json::json!({
            "properties": {
                "a": {"type": "string", "nullable": true},
                "b": {"type": "integer"}
            },
            "$defs": {"C": {"type": "string", "nullable": true}},
            "definitions": {"D": {"type": "string", "nullable": true}},
            "items": {"type": "string", "nullable": true},
            "additionalProperties": {"type": "string", "nullable": true},
            "not": {"type": "string", "nullable": true},
            "allOf": [{"type": "string", "nullable": true}],
            "anyOf": [{"type": "string", "nullable": true}],
            "oneOf": [{"type": "string", "nullable": true}]
        });

        super::normalize_schema(&mut schema, true);

        for pointer in [
            "/properties/a/type",
            "/$defs/C/type",
            "/definitions/D/type",
            "/items/type",
            "/additionalProperties/type",
            "/not/type",
            "/allOf/0/type",
            "/anyOf/0/type",
            "/oneOf/0/type",
        ] {
            assert_eq!(
                schema.pointer(pointer),
                Some(&serde_json::json!(["string", "null"])),
                "{pointer} must be normalized"
            );
        }
        assert_eq!(
            schema.pointer("/properties/b/type"),
            Some(&serde_json::json!("integer")),
            "a schema without `nullable` is untouched"
        );
    }

    #[test]
    fn normalize_schema_ignores_non_object_schemas() {
        for mut scalar in [serde_json::json!(5), serde_json::json!("x"), serde_json::json!([1, 2])]
        {
            let before = scalar.clone();
            super::normalize_schema(&mut scalar, true);
            assert_eq!(scalar, before);
        }
    }

    #[test]
    fn normalize_schema_skips_malformed_nested_containers() {
        // `properties` that is not an object, `items` that is absent, etc.
        let mut schema = serde_json::json!({"properties": ["not", "an", "object"]});
        super::normalize_schema(&mut schema, true);
        assert_eq!(schema, serde_json::json!({"properties": ["not", "an", "object"]}));

        let mut schema = serde_json::json!({"allOf": "not an array"});
        super::normalize_schema(&mut schema, true);
        assert_eq!(schema, serde_json::json!({"allOf": "not an array"}));
    }

    // ── parameter value coercion ───────────────────────────────────────

    #[test]
    fn parameter_values_are_coerced_to_the_declared_type() {
        let cases = [
            (serde_json::json!({"type": "integer"}), "42", serde_json::json!(42)),
            (serde_json::json!({"type": "number"}), "4.5", serde_json::json!(4.5)),
            (serde_json::json!({"type": "boolean"}), "true", serde_json::json!(true)),
            (serde_json::json!({"type": "string"}), "7", serde_json::json!("7")),
        ];
        for (schema, raw, expected) in cases {
            assert_eq!(super::parse_parameter_value(raw, &schema), expected);
        }
    }

    #[test]
    fn uncoercible_parameter_values_stay_strings() {
        let cases = [
            serde_json::json!({"type": "integer"}),
            serde_json::json!({"type": "number"}),
            serde_json::json!({"type": "boolean"}),
        ];
        for schema in cases {
            assert_eq!(
                super::parse_parameter_value("not-a-number", &schema),
                serde_json::json!("not-a-number"),
                "an uncoercible value must be surfaced as a string so validation can reject it"
            );
        }
    }

    #[test]
    fn parameter_value_defaults_to_a_string() {
        assert_eq!(
            super::parse_parameter_value("7", &serde_json::json!({})),
            serde_json::json!("7")
        );
        assert_eq!(
            super::parse_parameter_value("7", &serde_json::json!(42)),
            serde_json::json!("7")
        );
    }

    #[test]
    fn parameter_value_uses_the_first_entry_of_a_union_type() {
        let schema = serde_json::json!({"type": ["integer", "null"]});
        assert_eq!(super::parse_parameter_value("5", &schema), serde_json::json!(5));

        // `null` is not a coercible scalar type, so the raw string is kept and
        // the parameter schema — not the coercion — decides whether it is valid.
        let null_first = serde_json::json!({"type": ["null", "integer"]});
        assert_eq!(super::parse_parameter_value("5", &null_first), serde_json::json!("5"));
    }

    #[test]
    fn schema_type_is_array_detects_both_encodings() {
        assert!(super::schema_type_is_array(&serde_json::json!({"type": "array"})));
        assert!(super::schema_type_is_array(&serde_json::json!({"type": ["null", "array"]})));
        assert!(!super::schema_type_is_array(&serde_json::json!({"type": "string"})));
        assert!(!super::schema_type_is_array(&serde_json::json!({})));
        assert!(!super::schema_type_is_array(&serde_json::json!({"type": 5})));
    }

    // ── status codes ───────────────────────────────────────────────────

    #[test]
    fn status_codes_are_parsed_with_a_default_fallback() {
        assert_eq!(super::parse_status_code("200"), 200);
        assert_eq!(super::parse_status_code("599"), 599);
        assert_eq!(super::parse_status_code("default"), 200, "`default` maps onto 200");
        assert_eq!(super::parse_status_code("2XX"), 200, "an unparsable status falls back to 200");
        assert_eq!(super::parse_status_code(""), 200);
    }

    // ── request validation ─────────────────────────────────────────────

    #[test]
    fn missing_required_path_parameter_is_reported() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/pets/{id}": {
                    "get": {
                        "parameters": [
                            {"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::GET, "/pets/1").expect("matched").operation;

        let issues = operation.validate_request(
            &HashMap::new(),
            &HashMap::new(),
            &HeaderMap::new(),
            None,
            false,
        );
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].keyword, "required");
        assert_eq!(issues[0].instance_pointer, "/id");
    }

    #[test]
    fn missing_required_query_and_header_parameters_are_reported() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/search": {
                    "get": {
                        "parameters": [
                            {"name": "limit", "in": "query", "required": true, "schema": {"type": "integer"}},
                            {"name": "x-trace", "in": "header", "required": true, "schema": {"type": "string"}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::GET, "/search").expect("matched").operation;
        let issues = operation.validate_request(
            &HashMap::new(),
            &HashMap::new(),
            &HeaderMap::new(),
            None,
            false,
        );

        let pointers: BTreeSet<String> =
            issues.iter().map(|issue| issue.instance_pointer.clone()).collect();
        assert!(pointers.contains("/limit"), "got {pointers:?}");
        assert!(pointers.contains("/x-trace"), "got {pointers:?}");
    }

    #[test]
    fn optional_parameters_absent_are_not_reported() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/search": {
                    "get": {
                        "parameters": [
                            {"name": "limit", "in": "query", "schema": {"type": "integer"}},
                            {"name": "x-trace", "in": "header", "schema": {"type": "string"}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::GET, "/search").expect("matched").operation;
        assert!(
            operation
                .validate_request(&HashMap::new(), &HashMap::new(), &HeaderMap::new(), None, false)
                .is_empty()
        );
    }

    #[test]
    fn parameter_schema_violations_are_reported() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/search": {
                    "get": {
                        "parameters": [
                            {"name": "limit", "in": "query", "required": true, "schema": {"type": "integer"}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::GET, "/search").expect("matched").operation;

        let mut query = HashMap::new();
        query.insert("limit".to_owned(), vec!["not-a-number".to_owned()]);
        let issues =
            operation.validate_request(&HashMap::new(), &query, &HeaderMap::new(), None, false);

        assert!(!issues.is_empty(), "a non-integer limit must be rejected: {issues:?}");
        assert_eq!(issues.len(), 1, "exactly one violation, got {issues:?}");
        // The coerced scalar is validated on its own, so the instance pointer
        // is the (empty) root rather than the query key.
        assert_eq!(issues[0].keyword, "type");
    }

    #[test]
    fn array_query_parameter_collects_every_value() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/search": {
                    "get": {
                        "parameters": [
                            {
                                "name": "tag",
                                "in": "query",
                                "required": true,
                                "schema": {"type": "array", "items": {"type": "integer"}}
                            }
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::GET, "/search").expect("matched").operation;

        let mut query = HashMap::new();
        query.insert("tag".to_owned(), vec!["1".to_owned(), "2".to_owned()]);
        assert!(
            operation
                .validate_request(&HashMap::new(), &query, &HeaderMap::new(), None, false)
                .is_empty()
        );

        // One bad element fails the whole array.
        query.insert("tag".to_owned(), vec!["1".to_owned(), "oops".to_owned()]);
        assert!(
            !operation
                .validate_request(&HashMap::new(), &query, &HeaderMap::new(), None, false)
                .is_empty()
        );
    }

    #[test]
    fn array_query_parameter_without_items_schema_accepts_anything() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/search": {
                    "get": {
                        "parameters": [
                            {"name": "tag", "in": "query", "required": true, "schema": {"type": "array"}}
                        ],
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::GET, "/search").expect("matched").operation;
        let mut query = HashMap::new();
        query.insert("tag".to_owned(), vec!["anything".to_owned()]);
        assert!(
            operation
                .validate_request(&HashMap::new(), &query, &HeaderMap::new(), None, false)
                .is_empty()
        );
    }

    #[test]
    fn required_body_must_be_present() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/items": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {"application/json": {"schema": {"type": "object"}}}
                        },
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::POST, "/items").expect("matched").operation;

        let issues = operation.validate_request(
            &HashMap::new(),
            &HashMap::new(),
            &HeaderMap::new(),
            None,
            false,
        );
        assert!(issues.iter().any(|issue| issue.message.contains("missing required request body")));

        // A non-JSON body still satisfies `required`.
        assert!(
            operation
                .validate_request(&HashMap::new(), &HashMap::new(), &HeaderMap::new(), None, true)
                .is_empty()
        );
    }

    #[test]
    fn body_schema_violations_are_reported() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/items": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["name"],
                                        "properties": {"name": {"type": "string"}}
                                    }
                                }
                            }
                        },
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::POST, "/items").expect("matched").operation;

        let issues = operation.validate_request(
            &HashMap::new(),
            &HashMap::new(),
            &HeaderMap::new(),
            Some(&serde_json::json!({})),
            true,
        );
        assert!(!issues.is_empty(), "a body missing `name` must be rejected");
    }

    #[test]
    fn non_json_request_body_content_is_accepted_without_schema_checks() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/upload": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {"text/plain": {"schema": {"type": "string"}}}
                        },
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::POST, "/upload").expect("matched").operation;
        assert!(
            operation
                .validate_request(&HashMap::new(), &HashMap::new(), &HeaderMap::new(), None, true)
                .is_empty()
        );
    }

    #[test]
    fn request_body_without_content_is_still_required() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/items": {
                    "post": {
                        "requestBody": {"required": true},
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::POST, "/items").expect("matched").operation;
        assert!(operation.request_body_required);
        assert!(operation.request_body_schema.is_none());
        assert!(operation.request_body_media_types.is_empty());
        assert!(
            !operation
                .validate_request(&HashMap::new(), &HashMap::new(), &HeaderMap::new(), None, false)
                .is_empty()
        );
    }

    // ── responses ──────────────────────────────────────────────────────

    #[test]
    fn malformed_response_entries_are_skipped() {
        let parsed = super::parse_responses(
            &serde_json::json!({"responses": {"200": "not an object"}})
                .as_object()
                .cloned()
                .expect("obj"),
            "3.1.0",
        )
        .expect("parse");
        assert!(parsed.is_empty());
    }

    #[test]
    fn operation_without_responses_yields_an_empty_list() {
        let parsed = super::parse_responses(&serde_json::Map::new(), "3.1.0").expect("parse");
        assert!(parsed.is_empty());

        let parsed = super::parse_responses(
            &serde_json::json!({"responses": "nope"}).as_object().cloned().expect("obj"),
            "3.1.0",
        )
        .expect("parse");
        assert!(parsed.is_empty());
    }

    #[test]
    fn malformed_content_entries_are_skipped() {
        let parsed = super::parse_responses(
            &serde_json::json!({
                "responses": {
                    "200": {
                        "content": {
                            "application/json": "not an object",
                            "text/plain": {"schema": {"type": "string"}}
                        }
                    }
                }
            })
            .as_object()
            .cloned()
            .expect("obj"),
            "3.1.0",
        )
        .expect("parse");

        assert_eq!(parsed[0].content.len(), 1, "only the well-formed media type is kept");
        assert_eq!(parsed[0].content[0].media_type, "text/plain");
    }

    #[test]
    fn bare_named_examples_are_accepted() {
        let parsed = super::parse_responses(
            &serde_json::json!({
                "responses": {
                    "200": {
                        "content": {
                            "application/json": {
                                "examples": {
                                    "first": {"value": 1},
                                    "bare": 2
                                }
                            }
                        }
                    }
                }
            })
            .as_object()
            .cloned()
            .expect("obj"),
            "3.1.0",
        )
        .expect("parse");

        let entry = &parsed[0].content[0];
        assert_eq!(entry.named_examples.get("first"), Some(&serde_json::json!(1)));
        assert_eq!(entry.named_examples.get("bare"), Some(&serde_json::json!(2)));
        assert_eq!(
            entry.example,
            Some(serde_json::json!(2)),
            "named examples are stored in a sorted map, so `bare` wins over `first`"
        );
    }

    #[test]
    fn explicit_example_wins_over_named_examples() {
        let parsed = super::parse_responses(
            &serde_json::json!({
                "responses": {
                    "200": {
                        "content": {
                            "application/json": {
                                "example": "explicit",
                                "examples": {"named": {"value": "from-named"}}
                            }
                        }
                    }
                }
            })
            .as_object()
            .cloned()
            .expect("obj"),
            "3.1.0",
        )
        .expect("parse");
        assert_eq!(parsed[0].content[0].example, Some(serde_json::json!("explicit")));
    }

    #[test]
    fn response_header_examples_variants_are_all_supported() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "headers": {
                        "X-Direct": {"example": "a"},
                        "X-Named": {"examples": {"only": {"value": "b"}}},
                        "X-Schema": {"schema": {"type": "string"}},
                        "X-Bare": "not an object",
                        "X-Empty": {}
                    }
                }
            }
        }));

        assert_eq!(response.headers.len(), 4, "the malformed header is skipped");
        assert_eq!(response.headers["X-Direct"].example, Some(serde_json::json!("a")));
        assert_eq!(response.headers["X-Named"].example, Some(serde_json::json!("b")));
        assert!(response.headers["X-Schema"].schema.is_some());
        assert_eq!(response.headers["X-Empty"].example, None);
        assert_eq!(response.headers["X-Empty"].schema, None);
    }

    #[test]
    fn header_values_are_rendered_as_strings() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "headers": {
                        "X-Int": {"example": 42},
                        "X-Null": {"example": null},
                        "X-Bool": {"example": true}
                    }
                }
            }
        }));

        let rendered = super::generate_response_headers(&response.headers, 1);
        let map: BTreeMap<&str, &str> =
            rendered.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        assert_eq!(map.get("X-Int"), Some(&"42"));
        assert_eq!(map.get("X-Null"), Some(&""));
        assert_eq!(map.get("X-Bool"), Some(&"true"));
    }

    #[test]
    fn header_generation_skips_headers_with_no_renderable_value() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {"200": {"headers": {"X-Empty": {}, "X-Ok": {"example": "v"}}}}
        }));

        let rendered = super::generate_response_headers(&response.headers, 1);
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].0, "X-Ok");
    }

    #[test]
    fn schema_generated_headers_are_deterministic_per_name() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "headers": {
                        "X-A": {"schema": {"type": "string", "minLength": 4, "maxLength": 4}},
                        "X-B": {"schema": {"type": "string", "minLength": 4, "maxLength": 4}}
                    }
                }
            }
        }));

        let first = super::generate_response_headers(&response.headers, 7);
        let second = super::generate_response_headers(&response.headers, 7);
        assert_eq!(first, second, "the same seed must reproduce header values");

        let map: BTreeMap<&str, &str> =
            first.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        assert_eq!(map.get("X-A").map(|value| value.len()), Some(4));
        assert_ne!(
            map.get("X-A"),
            map.get("X-B"),
            "each header derives its own seed, so two headers must not collide"
        );
    }

    #[test]
    fn json_schema_prefers_the_json_family() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "content": {
                        "text/plain": {"schema": {"type": "string", "title": "text"}},
                        "application/json": {"schema": {"type": "object", "title": "json"}}
                    }
                }
            }
        }));
        assert_eq!(
            response.json_schema().and_then(|s| s.get("title")),
            Some(&serde_json::json!("json"))
        );
    }

    #[test]
    fn json_schema_falls_back_to_the_first_declared_entry() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {
                "200": {
                    "content": {
                        "text/plain": {"schema": {"type": "string", "title": "text"}}
                    }
                }
            }
        }));
        assert_eq!(
            response.json_schema().and_then(|s| s.get("title")),
            Some(&serde_json::json!("text")),
            "a non-JSON-only response still exposes its schema"
        );
    }

    #[test]
    fn json_schema_is_none_when_no_entry_declares_one() {
        let response = response_from_yaml_ish(serde_json::json!({
            "responses": {"200": {"content": {"text/plain": {}}}}
        }));
        assert!(response.json_schema().is_none());
    }

    // ── mock response generation ───────────────────────────────────────

    #[test]
    fn bodyless_response_stays_bodyless() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {"/x": {"get": {"responses": {"204": {"description": "no content"}}}}}
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let mock = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect("mock");
        assert_eq!(mock.status, 204);
        assert_eq!(mock.media_type, None);
        assert_eq!(mock.body, None);
        assert_eq!(mock.schema, None);
    }

    #[test]
    fn default_response_is_selected_when_no_2xx_exists() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "default": {
                                "description": "fallback",
                                "content": {"application/json": {"example": {"k": 1}}}
                            },
                            "418": {"description": "teapot"}
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let mock = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect("mock");

        // `default` maps onto HTTP 200 (see `parse_status_code`), which is why it
        // wins here even though a non-2xx entry is also declared.
        assert_eq!(mock.status, 200);
        assert_eq!(mock.body, Some(serde_json::json!({"k": 1})));
    }

    #[test]
    fn response_with_no_2xx_and_no_default_uses_the_first_declared() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "418": {
                                "description": "teapot",
                                "content": {"application/json": {"example": {"k": 1}}}
                            },
                            "500": {"description": "boom"}
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let mock = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect("mock");
        assert_eq!(mock.status, 418, "with no 2xx and no default the first entry is used");
    }

    #[test]
    fn any_2xx_status_is_preferred_over_default() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "default": {"description": "fallback"},
                            "202": {"description": "accepted"}
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let mock = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect("mock");
        assert_eq!(mock.status, 202);
    }

    #[test]
    fn media_type_without_a_schema_produces_no_body() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "200": {"content": {"application/json": {"description": "no schema"}}}
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let mock = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect("mock");
        assert_eq!(mock.body, None, "with no schema and no example there is nothing to generate");
        assert!(mock.schema.is_none());
    }

    #[test]
    fn dynamic_mode_without_a_schema_produces_no_body() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {"schema": {"type": "string"}, "example": "static"}
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;

        let dynamic =
            super::super::negotiate::PreferDirectives { dynamic: true, ..Default::default() };
        let mock = operation.mock_response(1, &dynamic).expect("mock");
        assert_ne!(
            mock.body,
            Some(serde_json::json!("static")),
            "dynamic=true bypasses the example"
        );
    }

    #[test]
    fn first_named_example_is_used_when_no_explicit_example_exists() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "object"},
                                        "examples": {"only": {"value": {"from": "named"}}}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let mock = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect("mock");
        assert_eq!(mock.body, Some(serde_json::json!({"from": "named"})));
    }

    #[test]
    fn example_override_is_scoped_to_the_negotiated_media_type() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {"examples": {"j": {"value": {"kind": "json"}}}},
                                    "text/plain": {"examples": {"t": {"value": "text"}}}
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;

        let prefer = super::super::negotiate::PreferDirectives {
            example: Some("t".to_owned()),
            media_type: Some("text/plain".to_owned()),
            ..Default::default()
        };
        let mock = operation.mock_response(1, &prefer).expect("mock");
        assert_eq!(mock.media_type.as_deref(), Some("text/plain"));
        assert_eq!(mock.body, Some(serde_json::json!("text")));

        // The same example name does not exist on the JSON entry, so asking for
        // it while negotiating JSON is an error rather than a silent fallback.
        let mismatch = super::super::negotiate::PreferDirectives {
            example: Some("t".to_owned()),
            media_type: Some("application/json".to_owned()),
            ..Default::default()
        };
        let error =
            operation.mock_response(1, &mismatch).expect_err("example is scoped per media type");
        assert!(
            matches!(error, crate::RuntimeError::NotFound(_)),
            "expected NotFound, got {error:?}"
        );
    }

    #[test]
    fn faker_errors_surface_as_parse_failures() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {"schema": {"type": "banana"}}
                                }
                            }
                        }
                    }
                }
            }
        }));

        let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
        let error = operation
            .mock_response(1, &super::super::negotiate::PreferDirectives::default())
            .expect_err("an ungeneratable schema must surface as an error");
        assert!(matches!(error, crate::RuntimeError::Parse(_)), "got {error:?}");
    }

    // ── callbacks ──────────────────────────────────────────────────────

    #[test]
    fn callbacks_are_parsed_from_every_method() {
        let runtime = build_runtime(serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/subscribe": {
                    "post": {
                        "callbacks": {
                            "onEvent": {
                                "{$request.body#/callbackUrl}/notify": {
                                    "post": {
                                        "requestBody": {
                                            "content": {
                                                "application/json": {
                                                    "schema": {"type": "object", "title": "Payload"}
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }));

        let operation =
            runtime.match_operation(&Method::POST, "/subscribe").expect("matched").operation;
        assert_eq!(operation.callbacks.len(), 1);
        assert_eq!(
            operation.callbacks[0].callback_url_expression,
            "{$request.body#/callbackUrl}/notify"
        );
        assert_eq!(operation.callbacks[0].method, Method::POST);
        assert_eq!(
            operation.callbacks[0].request_body_schema.as_ref().and_then(|s| s.get("title")),
            Some(&serde_json::json!("Payload"))
        );
    }

    #[test]
    fn malformed_callback_nodes_are_skipped() {
        let parsed = super::parse_callbacks(
            &serde_json::json!({
                "callbacks": {
                    "a": "not an object",
                    "b": {"expr": "not an object"},
                    "c": {"expr": {"get": "not an operation object"}}
                }
            })
            .as_object()
            .cloned()
            .expect("obj"),
            "3.1.0",
        )
        .expect("parse");
        assert!(parsed.is_empty());

        assert!(
            super::parse_callbacks(&serde_json::Map::new(), "3.1.0").expect("parse").is_empty()
        );
    }

    #[test]
    fn callback_without_a_json_request_body_has_no_schema() {
        let parsed = super::parse_callbacks(
            &serde_json::json!({
                "callbacks": {"a": {"expr": {"post": {"requestBody": {"content": {"text/plain": {}}}}}}}
            })
            .as_object()
            .cloned()
            .expect("obj"),
            "3.1.0",
        )
        .expect("parse");
        assert_eq!(parsed[0].request_body_schema, None);
    }

    #[test]
    fn callback_url_resolves_body_pointers() {
        let body = serde_json::json!({"callbackUrl": "https://cb.example.com/hook", "port": 8080});
        assert_eq!(
            super::resolve_callback_url("{$request.body#/callbackUrl}/done", Some(&body)),
            Some("https://cb.example.com/hook/done".to_owned())
        );
        assert_eq!(
            super::resolve_callback_url("http://x/{$request.body#/port}", Some(&body)),
            Some("http://x/8080".to_owned()),
            "a non-string body value is rendered with its JSON form"
        );
    }

    #[test]
    fn callback_url_keeps_unresolvable_tokens_verbatim() {
        let body = serde_json::json!({"other": 1});
        assert_eq!(
            super::resolve_callback_url("{$request.body#/missing}/x", Some(&body)),
            Some("{$request.body#/missing}/x".to_owned())
        );
        assert_eq!(
            super::resolve_callback_url("{$request.body#/a}", None),
            Some("{$request.body#/a}".to_owned())
        );
    }

    #[test]
    fn callback_url_with_an_unclosed_brace_is_none() {
        assert_eq!(
            super::resolve_callback_url("{$request.body#/a", Some(&serde_json::json!({}))),
            None
        );
    }

    #[test]
    fn callback_url_with_no_tokens_returns_the_literal() {
        assert_eq!(
            super::resolve_callback_url("https://static.example/hook", None),
            Some("https://static.example/hook".to_owned())
        );
    }

    #[test]
    fn callback_url_with_an_empty_expression_is_none() {
        assert_eq!(super::resolve_callback_url("", None), None);
    }

    #[test]
    fn callback_url_handles_multiple_tokens() {
        let body = serde_json::json!({"a": "A", "b": "B"});
        assert_eq!(
            super::resolve_callback_url("{$request.body#/a}-{$request.body#/b}", Some(&body)),
            Some("A-B".to_owned())
        );
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Recursive strategy for arbitrary JSON values.
    ///
    /// `serde_json::Value` does not implement `proptest::Arbitrary`, so the
    /// generator is assembled from bounded leaf and container shapes.
    fn any_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(serde_json::Value::from),
            any::<i64>().prop_map(serde_json::Value::from),
            "[ -~]{0,8}".prop_map(serde_json::Value::from),
        ];
        leaf.prop_recursive(3, 12, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                prop::collection::hash_map("[a-z]{1,6}", inner, 0..4).prop_map(|members| {
                    let map: serde_json::Map<String, serde_json::Value> =
                        members.into_iter().collect();
                    serde_json::Value::Object(map)
                }),
            ]
        })
    }

    /// A minimal but valid OpenAPI document with a free-form status code.
    fn document_with_status(status: u16) -> serde_json::Value {
        serde_json::json!({
            "openapi": "3.1.0",
            "paths": {
                "/x": {"get": {"responses": {status.to_string(): {"description": "ok"}}}}
            }
        })
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// Parsing any document never panics and never leaves the runtime in a
        /// state where `match_operation` disagrees with the router.
        #[test]
        fn parsing_any_document_never_panics(document in any_json())
        {
            let _ = OpenApiRuntime::from_resolved(document);
        }

        /// Status-code parsing is total: any declared status round-trips to the
        /// same numeric code, and anything else degrades to 200.
        #[test]
        fn status_codes_round_trip(status in 100u16..600)
        {
            let runtime = OpenApiRuntime::from_resolved(document_with_status(status))
                .expect("document should parse");
            let operation = runtime.match_operation(&Method::GET, "/x").expect("matched").operation;
            let prefer = super::super::negotiate::PreferDirectives { code: Some(status), ..Default::default() };
            let mock = operation.mock_response(1, &prefer).expect("mock");
            prop_assert_eq!(mock.status, status);
        }

        /// Parameter coercion is total: whatever the raw text and declared
        /// type, the coerced value either matches the declared type or falls
        /// back to a string.
        #[test]
        fn parameter_coercion_never_panics(
            raw in ".{0,12}",
            type_name in prop::sample::select(vec![
                serde_json::json!("integer"),
                serde_json::json!("number"),
                serde_json::json!("boolean"),
                serde_json::json!("string"),
                serde_json::json!(["null", "integer"]),
            ]),
        ) {
            let schema = serde_json::json!({"type": type_name.clone()});
            let parsed = super::parse_parameter_value(&raw, &schema);
            // Round-trip through JSON to guarantee the value is representable.
            let encoded = serde_json::to_string(&parsed).expect("coerced value must serialize");
            let _ = serde_json::from_str::<serde_json::Value>(&encoded).expect("must re-parse");
        }

        /// Nullable normalization is total: whatever the declared `type`, the
        /// result is still valid JSON and keeps the original when `nullable`
        /// is absent or the version is 3.1.
        #[test]
        fn schema_normalization_never_panics(
            schema in any_json(),
            is_30 in any::<bool>(),
        ) {
            let mut normalized = schema.clone();
            super::normalize_schema(&mut normalized, is_30);
            let _ = serde_json::to_string(&normalized).expect("normalized schema must serialize");
        }

        /// Callback expression resolution is total: any expression either
        /// yields `None` or a non-empty URL string.
        #[test]
        fn callback_url_resolution_never_panics(
            expression in "[a-zA-Z0-9{}$#/_.-]{0,40}",
            body in prop::option::of(any_json()),
        ) {
            let body = body.unwrap_or(serde_json::json!({"callbackUrl": "https://cb.example.com"}));
            match super::resolve_callback_url(&expression, Some(&body)) {
                Some(url) => prop_assert!(!url.is_empty()),
                None => prop_assert!(
                    expression.is_empty() || expression.contains('{'),
                    "an unresolvable expression must retain its token: {:?}",
                    expression
                ),
            }
        }

        /// `validate_request` is total: whatever the parameter shapes, it either
        /// reports fully-formed issues or succeeds.
        #[test]
        fn request_validation_never_panics(
            value in any_json(),
            name in "[a-z]{1,6}",
        ) {
            let operation = super::OperationSpec {
                method: Method::GET,
                path_template: "/x".to_owned(),
                operation_id: None,
                parameters: vec![super::ParameterSpec {
                    name,
                    location: super::ParameterIn::Query,
                    required: true,
                    schema: value,
                }],
                request_body_schema: None,
                request_body_required: false,
                request_body_media_types: Vec::new(),
                responses: Vec::new(),
                callbacks: Vec::new(),
            };

            let mut query = HashMap::new();
            query.insert("a".to_owned(), vec!["1".to_owned()]);
            let issues = operation.validate_request(
                &HashMap::new(),
                &query,
                &HeaderMap::new(),
                None,
                false,
            );
            for issue in issues {
                prop_assert!(!issue.keyword.is_empty());
            }
        }
    }
}
