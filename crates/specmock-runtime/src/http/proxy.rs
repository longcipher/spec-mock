//! Proxy handler for forwarding requests to upstream servers.

use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use serde_json::Value;
use specmock_core::{ValidationIssue, validate::validate_instance};

use super::{HttpRuntime, error_response, header_is_json};

const HEADER_HOST: &str = "host";
const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_AUTHORIZATION: &str = "authorization";
const HEADER_COOKIE: &str = "cookie";
const HEADER_PROXY_AUTHORIZATION: &str = "proxy-authorization";

/// Proxy a request to the upstream server and validate the response.
pub async fn proxy_request(
    runtime: &HttpRuntime,
    upstream: &url::Url,
    method: &Method,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
    body_bytes: &[u8],
    matched: &super::openapi::MatchedOperation<'_>,
) -> Response {
    let target_url = format!(
        "{}{}{}",
        upstream.as_str().trim_end_matches('/'),
        uri.path(),
        uri.query().map_or_else(String::new, |query| format!("?{query}"))
    );

    let mut request_builder =
        runtime.client.request(method.clone(), target_url).body(body_bytes.to_vec());
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == HEADER_HOST ||
            lower == HEADER_CONTENT_LENGTH ||
            lower == HEADER_AUTHORIZATION ||
            lower == HEADER_COOKIE ||
            lower == HEADER_PROXY_AUTHORIZATION
        {
            continue;
        }
        request_builder = request_builder.header(name, value);
    }

    // Set Host header from upstream URL so the proxy target receives the
    // correct virtual-host identity.
    if let Some(host) = upstream.host_str() {
        let host_value = if let Some(port) = upstream.port() {
            format!("{host}:{port}")
        } else {
            host.to_owned()
        };
        request_builder = request_builder.header("Host", host_value);
    }

    let upstream_response = match request_builder.send().await {
        Ok(response) => response,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                vec![ValidationIssue {
                    instance_pointer: "/proxy".to_owned(),
                    schema_pointer: "#".to_owned(),
                    keyword: "proxy".to_owned(),
                    message: format!("upstream request failed: {error}"),
                }],
            );
        }
    };

    let status = upstream_response.status();
    let response_headers = upstream_response.headers().clone();
    let response_bytes = match upstream_response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                vec![ValidationIssue {
                    instance_pointer: "/response/body".to_owned(),
                    schema_pointer: "#".to_owned(),
                    keyword: "proxy".to_owned(),
                    message: format!("failed to read upstream response body: {error}"),
                }],
            );
        }
    };

    if let Some(schema) = matched.operation.response_schema_for_status(status.as_u16()) &&
        header_is_json(&response_headers)
    {
        match serde_json::from_slice::<Value>(&response_bytes) {
            Ok(response_json) => match validate_instance(schema, &response_json) {
                Ok(issues) if !issues.is_empty() => {
                    return error_response(StatusCode::BAD_GATEWAY, issues);
                }
                Ok(_issues) => {}
                Err(error) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        vec![ValidationIssue {
                            instance_pointer: "/response".to_owned(),
                            schema_pointer: "#/responses".to_owned(),
                            keyword: "schema".to_owned(),
                            message: error.to_string(),
                        }],
                    );
                }
            },
            Err(error) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    vec![ValidationIssue {
                        instance_pointer: "/response/body".to_owned(),
                        schema_pointer: "#/responses".to_owned(),
                        keyword: "json".to_owned(),
                        message: format!("upstream response is not valid json: {error}"),
                    }],
                );
            }
        }
    }

    let mut builder = Response::builder().status(status);
    if let Some(target_headers) = builder.headers_mut() {
        for (name, value) in &response_headers {
            target_headers.append(name, value.clone());
        }
    }
    builder.body(Body::from(response_bytes)).unwrap_or_else(|_error| Response::new(Body::empty()))
}
