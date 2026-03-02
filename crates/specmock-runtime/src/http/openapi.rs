//! Minimal OpenAPI 3.0/3.1 runtime parser and request/response engine.

use std::{collections::HashMap, path::Path};

use http::{HeaderMap, Method};
use serde_json::{Map, Value};
use specmock_core::{
    ValidationIssue, faker::generate_json_value, ref_resolver::RefResolver,
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
    /// Request body schema.
    pub request_body_schema: Option<Value>,
    /// Whether request body is required.
    pub request_body_required: bool,
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

/// Response spec.
#[derive(Debug, Clone)]
pub struct ResponseSpec {
    /// Status selector (`200`, `default`).
    pub status: String,
    /// JSON schema.
    pub schema: Option<Value>,
    /// Explicit example payload.
    pub example: Option<Value>,
    /// Named examples keyed by example name.
    pub named_examples: HashMap<String, Value>,
}

/// Generated response.
#[derive(Debug, Clone)]
pub struct MockHttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Optional JSON body.
    pub body: Option<Value>,
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
        Self::from_resolved(resolved.root)
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
                let mut operation_params =
                    parse_parameters(operation_object.get("parameters"), version)?;
                parameters.append(&mut operation_params);

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
                    request_body_schema: request_body.0,
                    request_body_required: request_body.1,
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
    pub fn validate_request(
        &self,
        path_params: &HashMap<String, String>,
        query_params: &HashMap<String, Vec<String>>,
        headers: &HeaderMap,
        body_json: Option<&Value>,
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

        if self.request_body_required && body_json.is_none() {
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
    /// engine can honour `Prefer: code=…`, `Prefer: example=…`, and
    /// `Prefer: dynamic=true`.
    pub fn mock_response(
        &self,
        seed: u64,
        prefer: &super::negotiate::PreferDirectives,
    ) -> Result<MockHttpResponse, RuntimeError> {
        let selected = super::negotiate::select_response(&self.responses, prefer)
            .ok_or_else(|| RuntimeError::Parse("operation has no responses".to_owned()))?;

        // Named example override.
        if let Some(name) = &prefer.example &&
            let Some(value) = selected.named_examples.get(name)
        {
            return Ok(MockHttpResponse {
                status: parse_status_code(&selected.status),
                body: Some(value.clone()),
            });
        }

        // Dynamic mode: always use faker even when a static example exists.
        if prefer.dynamic &&
            let Some(schema) = &selected.schema
        {
            let value = generate_json_value(schema, seed)
                .map_err(|error| RuntimeError::Parse(error.to_string()))?;
            return Ok(MockHttpResponse {
                status: parse_status_code(&selected.status),
                body: Some(value),
            });
        }

        if let Some(example) = &selected.example {
            return Ok(MockHttpResponse {
                status: parse_status_code(&selected.status),
                body: Some(example.clone()),
            });
        }

        if let Some(schema) = &selected.schema {
            let value = generate_json_value(schema, seed)
                .map_err(|error| RuntimeError::Parse(error.to_string()))?;
            return Ok(MockHttpResponse {
                status: parse_status_code(&selected.status),
                body: Some(value),
            });
        }

        Ok(MockHttpResponse { status: parse_status_code(&selected.status), body: None })
    }

    /// Retrieve response schema by concrete status code with default fallback.
    pub fn response_schema_for_status(&self, status: u16) -> Option<&Value> {
        let status_text = status.to_string();
        if let Some(exact) = self
            .responses
            .iter()
            .find(|response| response.status == status_text)
            .and_then(|response| response.schema.as_ref())
        {
            return Some(exact);
        }
        self.responses
            .iter()
            .find(|response| response.status == "default")
            .and_then(|response| response.schema.as_ref())
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
        let normalized =
            normalize_schema(Value::Object(schema), openapi_version.starts_with("3.0"));

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
) -> Result<(Option<Value>, bool), RuntimeError> {
    let Some(request_body) = operation.get("requestBody").and_then(Value::as_object) else {
        return Ok((None, false));
    };

    let required = request_body.get("required").and_then(Value::as_bool).unwrap_or(false);

    let Some(content) = request_body.get("content").and_then(Value::as_object) else {
        return Ok((None, required));
    };
    let Some(media_type) = content
        .get("application/json")
        .and_then(Value::as_object)
        .cloned()
        .or_else(|| content.values().find_map(Value::as_object).cloned())
    else {
        return Ok((None, required));
    };

    let Some(schema) = media_type.get("schema").and_then(Value::as_object) else {
        return Ok((None, required));
    };

    Ok((
        Some(normalize_schema(Value::Object(schema.clone()), openapi_version.starts_with("3.0"))),
        required,
    ))
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

        let (schema, example, named_examples) = if let Some(content) =
            response_object.get("content").and_then(Value::as_object) &&
            let Some(media_type) = content
                .get("application/json")
                .and_then(Value::as_object)
                .cloned()
                .or_else(|| content.values().find_map(Value::as_object).cloned())
        {
            let schema = media_type.get("schema").and_then(Value::as_object).map(|schema_object| {
                normalize_schema(
                    Value::Object(schema_object.clone()),
                    openapi_version.starts_with("3.0"),
                )
            });
            // Collect named examples map.
            let mut named_examples = HashMap::new();
            if let Some(examples_obj) = media_type.get("examples").and_then(Value::as_object) {
                for (example_name, example_entry) in examples_obj {
                    if let Some(val) = example_entry.get("value") {
                        named_examples.insert(example_name.clone(), val.clone());
                    }
                }
            }

            let example = media_type
                .get("example")
                .cloned()
                .or_else(|| named_examples.values().next().cloned());
            (schema, example, named_examples)
        } else {
            (None, None, HashMap::new())
        };

        responses.push(ResponseSpec { status: status.clone(), schema, example, named_examples });
    }

    Ok(responses)
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
                        content
                            .get("application/json")
                            .and_then(Value::as_object)
                            .cloned()
                            .or_else(|| content.values().find_map(Value::as_object).cloned())
                    })
                    .and_then(|media| media.get("schema").and_then(Value::as_object).cloned())
                    .map(|s| {
                        normalize_schema(Value::Object(s), openapi_version.starts_with("3.0"))
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

        if let Some(pointer_path) = token.strip_prefix("$request.body#") {
            let body = request_body?;
            let value = json_pointer(body, pointer_path)?;
            let text = value.as_str().map_or_else(|| value.to_string(), ToOwned::to_owned);
            result.push_str(&text);
        } else {
            // Unsupported expression token – bail out.
            return None;
        }
    }
    result.push_str(remaining);

    if result.is_empty() { None } else { Some(result) }
}

/// Minimal JSON Pointer (RFC 6901) resolver.
fn json_pointer<'a>(value: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() || pointer == "/" {
        return Some(value);
    }
    let path = pointer.strip_prefix('/')?;
    let mut current = value;
    for segment in path.split('/') {
        let decoded = segment.replace("~1", "/").replace("~0", "~");
        match current {
            Value::Object(map) => current = map.get(&decoded)?,
            Value::Array(arr) => {
                let idx: usize = decoded.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn normalize_schema(mut schema: Value, use_nullable_transform: bool) -> Value {
    if let Some(object) = schema.as_object_mut() {
        for nested_key in ["properties", "$defs", "definitions"] {
            if let Some(properties) = object.get_mut(nested_key).and_then(Value::as_object_mut) {
                for value in properties.values_mut() {
                    let normalized = normalize_schema(value.clone(), use_nullable_transform);
                    *value = normalized;
                }
            }
        }

        for nested_key in ["items", "additionalProperties", "not"] {
            if let Some(value) = object.get_mut(nested_key) {
                let normalized = normalize_schema(value.clone(), use_nullable_transform);
                *value = normalized;
            }
        }

        for nested_key in ["allOf", "anyOf", "oneOf"] {
            if let Some(items) = object.get_mut(nested_key).and_then(Value::as_array_mut) {
                for item in items {
                    let normalized = normalize_schema(item.clone(), use_nullable_transform);
                    *item = normalized;
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
    schema
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
