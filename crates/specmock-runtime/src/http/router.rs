//! Radix-tree based path router built at spec load time.
//!
//! Replaces the previous linear-scan `match_operation` approach with an
//! O(depth) lookup over a tree of path segments.

use std::collections::HashMap;

use http::Method;

use super::openapi::OperationSpec;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Pre-compiled radix-tree router for HTTP path matching.
#[derive(Debug, Clone)]
pub struct PathRouter {
    root: RadixNode,
}

/// Result of a successful route match.
#[derive(Debug)]
pub struct RouteMatch {
    /// Index into the originating `Vec<OperationSpec>`.
    pub operation_index: usize,
    /// Extracted path parameters.
    pub path_params: HashMap<String, String>,
}

/// A single segment matcher used when building the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentMatcher {
    /// Exact literal segment (e.g. `pets`).
    Literal(String),
    /// Named path parameter (e.g. `{petId}` → `petId`).
    Param(String),
}

// ---------------------------------------------------------------------------
// Private types
// ---------------------------------------------------------------------------

/// A node in the radix tree.
#[derive(Debug, Clone, Default)]
struct RadixNode {
    /// Outgoing edges to child nodes.
    children: Vec<RadixEdge>,
    /// Operations that terminate at this node: `(Method, operation_index)`.
    operations: Vec<(Method, usize)>,
}

/// An edge connecting two `RadixNode`s via a `SegmentMatcher`.
#[derive(Debug, Clone)]
struct RadixEdge {
    segment: SegmentMatcher,
    child: RadixNode,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl PathRouter {
    /// Build a `PathRouter` from a slice of `OperationSpec`.
    ///
    /// Each operation's `path_template` is split into segments and inserted
    /// into the tree. Lookup time is O(number of segments in the request
    /// path).
    pub fn build(operations: &[OperationSpec]) -> Self {
        let mut root = RadixNode::default();

        for (index, operation) in operations.iter().enumerate() {
            let segments = parse_segments(&operation.path_template);
            insert(&mut root, &segments, operation.method.clone(), index);
        }

        Self { root }
    }

    /// Match an HTTP method and path against the tree.
    ///
    /// Returns a [`RouteMatch`] on success or `None` when no route matches.
    pub fn match_route(&self, method: &Method, path: &str) -> Option<RouteMatch> {
        let segments = split_path(path);
        let mut params = HashMap::new();
        let node = walk(&self.root, &segments, &mut params)?;

        node.operations
            .iter()
            .find(|(m, _)| m == method)
            .map(|&(_, operation_index)| RouteMatch { operation_index, path_params: params })
    }
}

// ---------------------------------------------------------------------------
// Helpers — segment parsing
// ---------------------------------------------------------------------------

/// Parse an OpenAPI path template into a list of `SegmentMatcher`s.
fn parse_segments(template: &str) -> Vec<SegmentMatcher> {
    template
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') && s.len() > 2 {
                SegmentMatcher::Param(s[1..s.len() - 1].to_owned())
            } else {
                SegmentMatcher::Literal(s.to_owned())
            }
        })
        .collect()
}

/// Split a concrete request path into segments.
fn split_path(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect()
}

// ---------------------------------------------------------------------------
// Helpers — tree insertion
// ---------------------------------------------------------------------------

fn insert(node: &mut RadixNode, segments: &[SegmentMatcher], method: Method, index: usize) {
    if segments.is_empty() {
        node.operations.push((method, index));
        return;
    }

    let Some((head, tail)) = segments.split_first() else {
        return;
    };

    // Find an existing edge that matches this segment matcher exactly.
    for edge in &mut node.children {
        if &edge.segment == head {
            insert(&mut edge.child, tail, method, index);
            return;
        }
    }

    // No existing edge — create a new one.
    let mut child = RadixNode::default();
    insert(&mut child, tail, method, index);
    node.children.push(RadixEdge { segment: head.clone(), child });
}

// ---------------------------------------------------------------------------
// Helpers — tree walk
// ---------------------------------------------------------------------------

/// Recursively walk the tree and collect path parameters.
///
/// Returns the terminal `RadixNode` if the walk succeeds.
fn walk<'a>(
    node: &'a RadixNode,
    segments: &[&str],
    params: &mut HashMap<String, String>,
) -> Option<&'a RadixNode> {
    if segments.is_empty() {
        return Some(node);
    }

    let (head, tail) = segments.split_first()?;

    // Prefer literal matches over parameter matches so that `/pets/mine`
    // beats `/pets/{petId}` when both exist.
    for edge in &node.children {
        if let SegmentMatcher::Literal(ref lit) = edge.segment &&
            lit == head &&
            let Some(result) = walk(&edge.child, tail, params)
        {
            return Some(result);
        }
    }

    // Fall back to parameter edges.
    for edge in &node.children {
        if let SegmentMatcher::Param(ref name) = edge.segment {
            let mut candidate_params = params.clone();
            candidate_params.insert(name.clone(), (*head).to_owned());
            if let Some(result) = walk(&edge.child, tail, &mut candidate_params) {
                *params = candidate_params;
                return Some(result);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a minimal `OperationSpec` with only method + path.
    fn op(method: Method, path: &str) -> OperationSpec {
        OperationSpec {
            method,
            path_template: path.to_owned(),
            operation_id: None,
            parameters: vec![],
            request_body_schema: None,
            request_body_required: false,
            responses: vec![],
            callbacks: vec![],
        }
    }

    #[test]
    fn literal_match() {
        let ops = vec![op(Method::GET, "/pets")];
        let router = PathRouter::build(&ops);

        let m = router.match_route(&Method::GET, "/pets");
        assert!(m.is_some());
        let m = m.unwrap_or_else(|| unreachable!());
        assert_eq!(m.operation_index, 0);
        assert!(m.path_params.is_empty());
    }

    #[test]
    fn param_match() {
        let ops = vec![op(Method::GET, "/pets/{petId}")];
        let router = PathRouter::build(&ops);

        let m = router.match_route(&Method::GET, "/pets/42");
        assert!(m.is_some());
        let m = m.unwrap_or_else(|| unreachable!());
        assert_eq!(m.operation_index, 0);
        assert_eq!(m.path_params.get("petId").map(String::as_str), Some("42"));
    }

    #[test]
    fn no_match_wrong_path() {
        let ops = vec![op(Method::GET, "/pets")];
        let router = PathRouter::build(&ops);
        assert!(router.match_route(&Method::GET, "/dogs").is_none());
    }

    #[test]
    fn no_match_wrong_method() {
        let ops = vec![op(Method::GET, "/pets")];
        let router = PathRouter::build(&ops);
        assert!(router.match_route(&Method::POST, "/pets").is_none());
    }

    #[test]
    fn method_disambiguation() {
        let ops = vec![
            op(Method::GET, "/pets"),
            op(Method::POST, "/pets"),
            op(Method::DELETE, "/pets/{petId}"),
        ];
        let router = PathRouter::build(&ops);

        let get = router.match_route(&Method::GET, "/pets");
        assert!(get.is_some());
        assert_eq!(get.unwrap_or_else(|| unreachable!()).operation_index, 0);

        let post = router.match_route(&Method::POST, "/pets");
        assert!(post.is_some());
        assert_eq!(post.unwrap_or_else(|| unreachable!()).operation_index, 1);

        let delete = router.match_route(&Method::DELETE, "/pets/7");
        assert!(delete.is_some());
        let delete = delete.unwrap_or_else(|| unreachable!());
        assert_eq!(delete.operation_index, 2);
        assert_eq!(delete.path_params.get("petId").map(String::as_str), Some("7"));
    }

    #[test]
    fn coexisting_paths() {
        let ops = vec![
            op(Method::GET, "/pets"),
            op(Method::GET, "/pets/{petId}"),
            op(Method::GET, "/pets/{petId}/toys"),
            op(Method::GET, "/pets/{petId}/toys/{toyId}"),
        ];
        let router = PathRouter::build(&ops);

        let m0 = router.match_route(&Method::GET, "/pets");
        assert_eq!(m0.unwrap_or_else(|| unreachable!()).operation_index, 0);

        let m1 = router.match_route(&Method::GET, "/pets/3");
        let m1 = m1.unwrap_or_else(|| unreachable!());
        assert_eq!(m1.operation_index, 1);
        assert_eq!(m1.path_params.get("petId").map(String::as_str), Some("3"));

        let m2 = router.match_route(&Method::GET, "/pets/3/toys");
        let m2 = m2.unwrap_or_else(|| unreachable!());
        assert_eq!(m2.operation_index, 2);
        assert_eq!(m2.path_params.get("petId").map(String::as_str), Some("3"));

        let m3 = router.match_route(&Method::GET, "/pets/3/toys/99");
        let m3 = m3.unwrap_or_else(|| unreachable!());
        assert_eq!(m3.operation_index, 3);
        assert_eq!(m3.path_params.get("petId").map(String::as_str), Some("3"));
        assert_eq!(m3.path_params.get("toyId").map(String::as_str), Some("99"));
    }

    #[test]
    fn trailing_slash_normalisation() {
        let ops = vec![op(Method::GET, "/pets")];
        let router = PathRouter::build(&ops);

        // Trailing slash on request should still match.
        assert!(router.match_route(&Method::GET, "/pets/").is_some());
        // Path template with trailing slash should also work.
        let ops2 = vec![op(Method::GET, "/pets/")];
        let router2 = PathRouter::build(&ops2);
        assert!(router2.match_route(&Method::GET, "/pets").is_some());
    }

    #[test]
    fn literal_preferred_over_param() {
        let ops = vec![op(Method::GET, "/pets/{petId}"), op(Method::GET, "/pets/mine")];
        let router = PathRouter::build(&ops);

        let mine = router.match_route(&Method::GET, "/pets/mine");
        let mine = mine.unwrap_or_else(|| unreachable!());
        // Should match the literal `/pets/mine` (index 1), not the param.
        assert_eq!(mine.operation_index, 1);
        assert!(mine.path_params.is_empty());

        // Other values should still match the param route.
        let other = router.match_route(&Method::GET, "/pets/42");
        let other = other.unwrap_or_else(|| unreachable!());
        assert_eq!(other.operation_index, 0);
        assert_eq!(other.path_params.get("petId").map(String::as_str), Some("42"));
    }

    #[test]
    fn root_path() {
        let ops = vec![op(Method::GET, "/")];
        let router = PathRouter::build(&ops);
        assert!(router.match_route(&Method::GET, "/").is_some());
    }

    #[test]
    fn multi_param_segments() {
        let ops = vec![op(Method::GET, "/orgs/{orgId}/repos/{repoId}/issues/{issueId}")];
        let router = PathRouter::build(&ops);

        let m = router.match_route(&Method::GET, "/orgs/acme/repos/widget/issues/123");
        let m = m.unwrap_or_else(|| unreachable!());
        assert_eq!(m.path_params.get("orgId").map(String::as_str), Some("acme"));
        assert_eq!(m.path_params.get("repoId").map(String::as_str), Some("widget"));
        assert_eq!(m.path_params.get("issueId").map(String::as_str), Some("123"));
    }

    #[test]
    fn extra_segments_no_match() {
        let ops = vec![op(Method::GET, "/pets")];
        let router = PathRouter::build(&ops);
        assert!(router.match_route(&Method::GET, "/pets/1/extra").is_none());
    }

    #[test]
    fn fewer_segments_no_match() {
        let ops = vec![op(Method::GET, "/pets/{petId}/toys")];
        let router = PathRouter::build(&ops);
        assert!(router.match_route(&Method::GET, "/pets/1").is_none());
    }
}
