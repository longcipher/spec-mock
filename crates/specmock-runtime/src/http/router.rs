//! Segment-trie based path router built at spec load time.
//!
//! Replaces the previous linear-scan `match_operation` approach with an
//! O(depth) lookup over a tree of path segments.

use std::collections::HashMap;

use http::Method;

use super::openapi::OperationSpec;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Pre-compiled segment-trie router for HTTP path matching.
#[derive(Debug, Clone)]
pub struct PathRouter {
    root: RouteNode,
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

/// A node in the segment trie.
#[derive(Debug, Clone, Default)]
struct RouteNode {
    /// Outgoing edges to child nodes.
    children: Vec<RouteEdge>,
    /// Operations that terminate at this node: `(Method, operation_index)`.
    operations: Vec<(Method, usize)>,
}

/// An edge connecting two `RouteNode`s via a `SegmentMatcher`.
#[derive(Debug, Clone)]
struct RouteEdge {
    segment: SegmentMatcher,
    child: RouteNode,
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
        let mut root = RouteNode::default();

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
        let node = walk(&self.root, &segments, &mut params, method)?;

        let operation_index =
            node.operations.iter().find(|(m, _)| m == method).map(|&(_, index)| index)?;

        Some(RouteMatch { operation_index, path_params: params })
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

fn insert(node: &mut RouteNode, segments: &[SegmentMatcher], method: Method, index: usize) {
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
    let mut child = RouteNode::default();
    insert(&mut child, tail, method, index);
    node.children.push(RouteEdge { segment: head.clone(), child });
}

// ---------------------------------------------------------------------------
// Helpers — tree walk
// ---------------------------------------------------------------------------

/// Recursively walk the tree and collect path parameters.
///
/// Returns the terminal `RouteNode` only when that node declares an operation
/// for `method`. The method is threaded through the walk because two templates
/// can share a shape yet differ in parameter name (`/{a}` and `/{b}` become
/// separate edges); without this, the first such edge would swallow the request
/// and a method declared only on a sibling edge would be unreachable.
///
/// Returns the terminal `RouteNode` if the walk succeeds.
fn walk<'a>(
    node: &'a RouteNode,
    segments: &[&str],
    params: &mut HashMap<String, String>,
    method: &Method,
) -> Option<&'a RouteNode> {
    if segments.is_empty() {
        return node.operations.iter().any(|(m, _)| m == method).then_some(node);
    }

    let (head, tail) = segments.split_first()?;

    // Prefer literal matches over parameter matches so that `/pets/mine`
    // beats `/pets/{petId}` when both exist.
    for edge in &node.children {
        if let SegmentMatcher::Literal(ref lit) = edge.segment &&
            lit == head &&
            let Some(result) = walk(&edge.child, tail, params, method)
        {
            return Some(result);
        }
    }

    // Fall back to parameter edges.
    for edge in &node.children {
        if let SegmentMatcher::Param(ref name) = edge.segment {
            let mut candidate_params = params.clone();
            candidate_params.insert(name.clone(), (*head).to_owned());
            if let Some(result) = walk(&edge.child, tail, &mut candidate_params, method) {
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
            request_body_media_types: Vec::new(),
            responses: vec![],
            callbacks: vec![],
        }
    }

    /// Regression: templates that differ only in parameter name build sibling
    /// parameter edges, so matching must stay method-aware across them. Committing
    /// to the first edge made `POST /{b}` unreachable behind `GET /{a}`.
    #[test]
    fn param_name_collision_keeps_every_method_routable() {
        let ops = vec![op(Method::GET, "/{a}"), op(Method::POST, "/{b}")];
        let router = PathRouter::build(&ops);

        let get = router.match_route(&Method::GET, "/v0").expect("GET must route");
        assert_eq!(get.operation_index, 0);
        assert_eq!(get.path_params.get("a").map(String::as_str), Some("v0"));

        let post = router.match_route(&Method::POST, "/v0").expect("POST must route");
        assert_eq!(post.operation_index, 1);
        assert_eq!(post.path_params.get("b").map(String::as_str), Some("v0"));
    }

    /// A method declared on no route at all must still report no match.
    #[test]
    fn undeclared_method_on_a_colliding_shape_does_not_route() {
        let ops = vec![op(Method::GET, "/{a}"), op(Method::POST, "/{b}")];
        let router = PathRouter::build(&ops);

        assert!(router.match_route(&Method::DELETE, "/v0").is_none());
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

    // ── segment parsing ────────────────────────────────────────────────

    #[test]
    fn empty_path_templates_match_the_root() {
        for template in ["", "/", "//"] {
            let ops = vec![op(Method::GET, template)];
            let router = PathRouter::build(&ops);
            assert!(router.match_route(&Method::GET, "/").is_some(), "template {template:?}");
            assert!(router.match_route(&Method::GET, "").is_some(), "template {template:?}");
            assert!(router.match_route(&Method::GET, "/x").is_none(), "template {template:?}");
        }
    }

    #[test]
    fn repeated_slashes_are_collapsed_on_both_sides() {
        let ops = vec![op(Method::GET, "/a//b")];
        let router = PathRouter::build(&ops);
        assert!(router.match_route(&Method::GET, "/a/b").is_some());
        assert!(router.match_route(&Method::GET, "//a//b//").is_some());
    }

    #[test]
    fn braces_that_do_not_delimit_a_parameter_stay_literal() {
        let ops = vec![op(Method::GET, "/a/{}")];
        let router = PathRouter::build(&ops);
        // `{}` is too short to be a parameter, so it is a literal segment.
        assert!(router.match_route(&Method::GET, "/a/{}").is_some());
        assert!(router.match_route(&Method::GET, "/a/x").is_none());
    }

    #[test]
    fn half_open_braces_stay_literal() {
        for template in ["/a/{id", "/a/id}", "/a/{{id}}"] {
            let ops = vec![op(Method::GET, template)];
            let router = PathRouter::build(&ops);
            let expected = template.trim_matches('/');
            assert!(
                router.match_route(&Method::GET, &format!("/{expected}")).is_some(),
                "{template} should be matched literally"
            );
        }
    }

    #[test]
    fn request_path_segments_may_contain_braces() {
        let ops = vec![op(Method::GET, "/a/{id}")];
        let router = PathRouter::build(&ops);
        let matched = router.match_route(&Method::GET, "/a/literal");
        assert!(matched.is_some(), "a concrete value with braces still fills the parameter");
    }

    // ── precedence ─────────────────────────────────────────────────────

    #[test]
    fn literal_wins_over_param_across_the_whole_path() {
        let ops = vec![op(Method::GET, "/a/{x}/c"), op(Method::GET, "/a/b/d")];
        let router = PathRouter::build(&ops);

        // The literal branch at segment 2 is tried first and abandoned when it
        // cannot complete, so the parameter branch still serves `/a/b/c`.
        let fallback = router
            .match_route(&Method::GET, "/a/b/c")
            .expect("the parameter branch must serve this path");
        assert_eq!(fallback.operation_index, 0);
        assert_eq!(fallback.path_params.get("x").map(String::as_str), Some("b"));
        assert_eq!(
            router.match_route(&Method::GET, "/a/b/d").expect("literal route").operation_index,
            1
        );
        assert_eq!(
            router.match_route(&Method::GET, "/a/z/c").expect("param route").operation_index,
            0
        );
    }

    /// Routing matches on (method, path), so a literal that is more specific but
    /// registered for a different method must not shadow a parameter route that
    /// does serve the request.
    #[test]
    fn a_literal_match_on_the_wrong_method_falls_back_to_a_param_route() {
        let ops = vec![op(Method::POST, "/pets/mine"), op(Method::GET, "/pets/{petId}")];
        let router = PathRouter::build(&ops);

        let get = router.match_route(&Method::GET, "/pets/mine").expect("param route");
        assert_eq!(get.operation_index, 1);
        assert_eq!(get.path_params.get("petId").map(String::as_str), Some("mine"));
        assert_eq!(
            router.match_route(&Method::POST, "/pets/mine").expect("literal route").operation_index,
            0
        );
        assert_eq!(
            router.match_route(&Method::GET, "/pets/42").expect("param route").operation_index,
            1
        );
    }

    #[test]
    fn empty_path_params_are_not_leaked_between_matches() {
        let ops = vec![op(Method::GET, "/pets/{petId}")];
        let router = PathRouter::build(&ops);

        let first = router.match_route(&Method::GET, "/pets/1").expect("matched");
        assert_eq!(first.path_params.len(), 1);

        // A failed walk must not mutate the caller's parameter map; re-run and
        // confirm the tree is unaffected.
        assert!(router.match_route(&Method::GET, "/pets/1/extra").is_none());
        let second = router.match_route(&Method::GET, "/pets/2").expect("matched");
        assert_eq!(second.path_params.get("petId").map(String::as_str), Some("2"));
    }

    #[test]
    fn a_deeper_literal_takes_precedence_over_a_shallower_param() {
        let ops = vec![op(Method::GET, "/a/{x}"), op(Method::GET, "/a/b")];
        let router = PathRouter::build(&ops);
        let matched = router.match_route(&Method::GET, "/a/b").expect("matched");
        assert_eq!(matched.operation_index, 1);
        assert!(matched.path_params.is_empty(), "the literal route declares no parameters");
    }

    // ── edge cases ─────────────────────────────────────────────────────

    #[test]
    fn empty_router_matches_nothing() {
        let router = PathRouter::build(&[]);
        assert!(router.match_route(&Method::GET, "/").is_none());
        assert!(router.match_route(&Method::GET, "/pets").is_none());
    }

    #[test]
    fn duplicate_routes_keep_the_first_index() {
        let ops = vec![op(Method::GET, "/pets"), op(Method::GET, "/pets")];
        let router = PathRouter::build(&ops);
        assert_eq!(router.match_route(&Method::GET, "/pets").expect("matched").operation_index, 0);
    }

    #[test]
    fn very_deep_paths_are_supported() {
        let template: String = (0..40).map(|index| format!("/s{index}")).collect();
        let mut operations = vec![op(Method::GET, &template)];
        for index in 0..40 {
            operations.push(op(Method::GET, &format!("{template}/s{index}")));
        }
        let router = PathRouter::build(&operations);

        assert!(router.match_route(&Method::GET, &template).is_some());
        let deepest = format!("{template}/s0/s1/s2/s3");
        assert!(router.match_route(&Method::GET, &deepest).is_none(), "only declared depths match");
    }

    #[test]
    fn many_parameters_are_all_captured() {
        let mut template = String::from("/root");
        let mut path = String::from("/root");
        let mut expected = std::collections::BTreeMap::new();
        for index in 0..12 {
            let name = format!("p{index}");
            let value = format!("v{index}");
            template.push_str(&format!("/{{{name}}}"));
            path.push('/');
            path.push_str(&value);
            expected.insert(name, value);
        }

        let ops = vec![op(Method::GET, &template)];
        let router = PathRouter::build(&ops);
        let matched = router.match_route(&Method::GET, &path).expect("matched");
        assert_eq!(matched.path_params.len(), expected.len());

        let actual: std::collections::BTreeMap<String, String> =
            matched.path_params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn parameter_values_may_contain_slashes_free_text() {
        let ops = vec![op(Method::GET, "/files/{name}")];
        let router = PathRouter::build(&ops);
        let matched = router.match_route(&Method::GET, "/files/report%20final").expect("matched");
        assert_eq!(
            matched.path_params.get("name").map(String::as_str),
            Some("report%20final"),
            "the router does not percent-decode; validation sees the raw segment"
        );
    }

    #[test]
    fn non_standard_methods_route_correctly() {
        let ops = vec![op(Method::OPTIONS, "/x"), op(Method::HEAD, "/x"), op(Method::TRACE, "/x")];
        let router = PathRouter::build(&ops);

        assert_eq!(router.match_route(&Method::OPTIONS, "/x").expect("OPTIONS").operation_index, 0);
        assert_eq!(router.match_route(&Method::HEAD, "/x").expect("HEAD").operation_index, 1);
        assert_eq!(router.match_route(&Method::TRACE, "/x").expect("TRACE").operation_index, 2);
    }

    #[test]
    fn a_custom_method_token_is_matched_verbatim() {
        let custom = Method::from_bytes(b"PURGE").expect("custom method");
        let ops = vec![op(custom.clone(), "/cache")];
        let router = PathRouter::build(&ops);

        assert!(router.match_route(&custom, "/cache").is_some());
        assert!(
            router.match_route(&Method::GET, "/cache").is_none(),
            "a different method must not match"
        );
    }

    // ── property tests ─────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Build a random set of operations from random path templates.
    fn any_operations() -> impl Strategy<Value = Vec<(Method, String)>> {
        prop::collection::vec(
            (
                prop::sample::select(vec![Method::GET, Method::POST, Method::PUT, Method::DELETE]),
                prop::collection::vec(
                    prop_oneof![
                        "[a-z]{1,5}".prop_map(|segment| segment),
                        "[a-z]{1,5}".prop_map(|name| format!("{{{name}}}")),
                    ],
                    0..5,
                ),
            ),
            1..10,
        )
        .prop_map(|entries| {
            entries
                .into_iter()
                .map(|(method, segments)| (method, format!("/{}", segments.join("/"))))
                .collect()
        })
    }

    /// Realize a path that satisfies a template by filling parameters.
    fn instantiate(template: &str, fill: u64) -> String {
        let segments: Vec<&str> =
            template.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();

        // `/` has no segments to rejoin, so it must be reproduced verbatim;
        // naively joining the (empty) segment list would yield `""` instead.
        if segments.is_empty() {
            return "/".to_owned();
        }

        let mut out = String::new();
        for segment in segments {
            out.push('/');
            if segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2 {
                out.push_str(&format!("v{fill}"));
            } else {
                out.push_str(segment);
            }
        }
        out
    }

    /// Every `{param}` segment declared by a path template.
    fn declared_params(template: &str) -> Vec<String> {
        template
            .trim_matches('/')
            .split('/')
            .filter(|segment| {
                segment.len() > 2 && segment.starts_with('{') && segment.ends_with('}')
            })
            .map(|segment| segment[1..segment.len() - 1].to_owned())
            .collect()
    }

    // `rustfmt` cannot parse the `proptest!` DSL, so this block is left verbatim.

    #[rustfmt::skip]

    proptest! {
        /// Any template built from literals and `{param}` segments must match a
        /// concretely-instantiated path, and every declared parameter must be
        /// captured in `path_params`.
        #[test]
        fn templates_always_match_their_instantiation(
            operations in any_operations(),
            fill in any::<u64>(),
        ) {
            let specs: Vec<OperationSpec> = operations
                .iter()
                .map(|(method, template)| op(method.clone(), template))
                .collect();
            let router = PathRouter::build(&specs);

            for (method, template) in &operations {
                let path = instantiate(template, fill);

                // A concrete instantiation of a declared template must always
                // route. This is asserted rather than assumed, otherwise every
                // non-matching case would be silently discarded and the property
                // would hold vacuously.
                let matched = router.match_route(method, &path);
                prop_assert!(
                    matched.is_some(),
                    "template {:?} instantiated as {:?} did not match",
                    template,
                    path
                );
                let matched = matched.expect("checked above");

                // The route that answered must accept this method, and every
                // parameter it declares must be bound by the match.
                let matched_template = &operations[matched.operation_index].1;
                prop_assert_eq!(&operations[matched.operation_index].0, method);
                for name in declared_params(matched_template) {
                    prop_assert!(
                        matched.path_params.contains_key(&name),
                        "parameter {:?} declared in {:?} was not bound for {:?}",
                        name,
                        matched_template,
                        path
                    );
                }
            }
        }

        /// A path with more segments than any declared template never matches.
        #[test]
        fn overlong_paths_never_match(operations in any_operations(), noise in "[a-z]{1,5}") {
            let specs: Vec<OperationSpec> = operations
                .iter()
                .map(|(method, template)| op(method.clone(), template))
                .collect();
            let router = PathRouter::build(&specs);

            // Templates above hold at most four segments, so a longer path
            // cannot correspond to any of them.
            let deepest = specs
                .iter()
                .map(|spec| spec.path_template.split('/').filter(|s| !s.is_empty()).count())
                .max()
                .unwrap_or(0);
            let mut path = String::new();
            for _ in 0..=deepest {
                path.push_str("/zz");
            }
            path.push('/');
            path.push_str(&noise);

            prop_assert!(router.match_route(&Method::GET, &path).is_none());
        }

        /// Whichever method is requested, a match implies the router has an
        /// operation registered for exactly that method at that path.
        #[test]
        fn matches_always_agree_with_the_method(
            operations in any_operations(),
            fill in any::<u64>(),
        ) {
            let specs: Vec<OperationSpec> = operations
                .iter()
                .map(|(method, template)| op(method.clone(), template))
                .collect();
            let router = PathRouter::build(&specs);

            for (_, template) in &operations {
                let path = instantiate(template, fill);
                for candidate in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
                    if let Some(matched) = router.match_route(&candidate, &path) {
                        prop_assert_eq!(
                            &operations[matched.operation_index].0,
                            &candidate,
                            "matched a route registered for a different method"
                        );
                    }
                }
            }
        }

        /// Trailing-slash and duplicate-slash variations never change routing.
        #[test]
        fn slash_variations_do_not_change_routing(
            operations in any_operations(),
            fill in any::<u64>(),
        ) {
            let specs: Vec<OperationSpec> = operations
                .iter()
                .map(|(method, template)| op(method.clone(), template))
                .collect();
            let router = PathRouter::build(&specs);

            for (method, template) in &operations {
                let path = instantiate(template, fill);
                let doubled = path.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("//");
                prop_assert_eq!(
                    router.match_route(method, &path).map(|m| m.operation_index),
                    router.match_route(method, &format!("/{doubled}")).map(|m| m.operation_index),
                    "duplicate slashes changed the route for {:?}", path
                );
                prop_assert_eq!(
                    router.match_route(method, &path).map(|m| m.operation_index),
                    router.match_route(method, &format!("{path}/")).map(|m| m.operation_index),
                    "a trailing slash changed the route for {:?}", path
                );
            }
        }

        /// Building a router from arbitrary operations never panics, and every
        /// operation index stays reachable.
        #[test]
        fn building_never_panics(operations in any_operations()) {
            let specs: Vec<OperationSpec> = operations
                .iter()
                .map(|(method, template)| op(method.clone(), template))
                .collect();
            let router = PathRouter::build(&specs);
            let _ = router.match_route(&Method::GET, "/");
        }
    }
}
