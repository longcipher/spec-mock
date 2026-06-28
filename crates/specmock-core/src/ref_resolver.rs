//! Protocol-agnostic `$ref` resolver for OpenAPI and AsyncAPI spec documents.
//!
//! Supports local JSON pointer refs (`#/components/schemas/Pet`) and
//! file-relative refs (`./schemas/Pet.yaml#/Pet`). URL-based refs are not
//! currently supported and will produce an error.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

use serde_json::Value;
use tracing::warn;

use crate::error::SpecMockCoreError;

/// Default maximum recursion depth for `$ref` resolution.
const DEFAULT_MAX_DEPTH: usize = 64;
const DEFAULT_EXTERNAL_DOC_CACHE_LIMIT: usize = 128;

/// Synchronous `$ref` resolver that handles local and file-relative references.
///
/// The resolver caches loaded external files to avoid redundant I/O and parses
/// both YAML and JSON source documents into [`serde_json::Value`] trees.
#[derive(Debug)]
pub struct RefResolver {
    /// Base directory for resolving relative file refs.
    base_dir: PathBuf,
    /// Canonicalized root directories that file refs must stay within.
    allowed_roots: Vec<PathBuf>,
    /// Cache of already-loaded external documents keyed by canonical path.
    cache: HashMap<String, Value>,
    /// FIFO insertion order of cache keys for bounded-size eviction.
    cache_order: VecDeque<String>,
    /// Maximum resolution depth to prevent cycles.
    max_depth: usize,
    /// Maximum number of external docs retained in memory.
    external_cache_limit: usize,
}

impl RefResolver {
    /// Create a new resolver rooted at `base_dir`.
    pub fn new(base_dir: PathBuf) -> Self {
        let allowed_root = std::fs::canonicalize(&base_dir).unwrap_or_else(|_| base_dir.clone());
        Self {
            base_dir,
            allowed_roots: vec![allowed_root],
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            max_depth: DEFAULT_MAX_DEPTH,
            external_cache_limit: DEFAULT_EXTERNAL_DOC_CACHE_LIMIT,
        }
    }

    /// Override the default cycle-detection depth limit.
    pub const fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Add an additional allowed root for file-relative refs.
    #[must_use]
    pub fn with_allowed_root(mut self, root: PathBuf) -> Self {
        let canonical = std::fs::canonicalize(&root).unwrap_or(root);
        if !self.allowed_roots.iter().any(|existing| existing == &canonical) {
            self.allowed_roots.push(canonical);
        }
        self
    }

    /// Add multiple allowed roots for file-relative refs.
    #[must_use]
    pub fn with_allowed_roots<I>(mut self, roots: I) -> Self
    where
        I: IntoIterator<Item = PathBuf>,
    {
        for root in roots {
            self = self.with_allowed_root(root);
        }
        self
    }

    /// Set maximum number of external docs cached by this resolver.
    #[must_use]
    pub const fn with_external_cache_limit(mut self, max_entries: usize) -> Self {
        self.external_cache_limit = if max_entries == 0 { 1 } else { max_entries };
        self
    }

    /// Load a spec file from `path`, resolve every `$ref`, and return the
    /// fully-inlined document.
    pub fn resolve(&mut self, path: &Path) -> Result<Value, SpecMockCoreError> {
        let canonical = std::fs::canonicalize(path).map_err(|e| {
            SpecMockCoreError::Ref(format!("cannot canonicalize {}: {e}", path.display()))
        })?;

        let mut root = load_file(&canonical)?;

        // Clone root so we can use it as a read-only lookup for local refs
        // while mutating the tree in-place.
        let root_snapshot = root.clone();

        let parent = canonical.parent().unwrap_or(&self.base_dir).to_path_buf();
        self.resolve_value(&mut root, &root_snapshot, &parent, 0)?;

        Ok(root)
    }

    /// Resolve a pre-parsed [`Value`] tree in-place, using `base_dir` as the
    /// root for any file-relative refs.
    pub fn resolve_value_tree(&mut self, value: &mut Value) -> Result<(), SpecMockCoreError> {
        let snapshot = value.clone();
        let base = self.base_dir.clone();
        self.resolve_value(value, &snapshot, &base, 0)
    }

    /// Recursively walk `value`, replacing every `$ref` node with its resolved
    /// target.
    fn resolve_value(
        &mut self,
        value: &mut Value,
        root_doc: &Value,
        current_base: &Path,
        depth: usize,
    ) -> Result<(), SpecMockCoreError> {
        if depth > self.max_depth {
            return Err(SpecMockCoreError::Ref(format!(
                "maximum $ref resolution depth ({}) exceeded — possible cycle",
                self.max_depth,
            )));
        }

        match value {
            Value::Object(map) => {
                if let Some(ref_val) = map.get("$ref").cloned() {
                    let ref_str = ref_val.as_str().ok_or_else(|| {
                        SpecMockCoreError::Ref("$ref value is not a string".to_owned())
                    })?;

                    // Collect sibling properties (everything except $ref itself).
                    let siblings: serde_json::Map<String, Value> = map
                        .iter()
                        .filter(|(k, _)| k.as_str() != "$ref")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();

                    let resolved = self.resolve_ref(ref_str, root_doc, current_base, depth + 1)?;

                    *value = resolved;

                    // Merge sibling properties into the resolved value.
                    if !siblings.is_empty() {
                        if let Value::Object(resolved_map) = value {
                            for (k, v) in siblings {
                                resolved_map.entry(&k).or_insert(v);
                            }
                        } else {
                            warn!(
                                ref_str,
                                "resolved $ref target is not an object; sibling properties dropped"
                            );
                        }
                    }

                    // Recurse into the freshly-inlined value.
                    self.resolve_value(value, root_doc, current_base, depth + 1)?;
                } else {
                    // Regular object — recurse into each value.
                    let keys: Vec<String> = map.keys().cloned().collect();
                    for key in keys {
                        if let Some(child) = map.get_mut(&key) {
                            self.resolve_value(child, root_doc, current_base, depth + 1)?;
                        }
                    }
                }
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    self.resolve_value(item, root_doc, current_base, depth + 1)?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Resolve a single `$ref` string into a [`Value`].
    fn resolve_ref(
        &mut self,
        ref_str: &str,
        root_doc: &Value,
        current_base: &Path,
        depth: usize,
    ) -> Result<Value, SpecMockCoreError> {
        if ref_str.starts_with("http://") || ref_str.starts_with("https://") {
            return Err(SpecMockCoreError::Ref(format!(
                "URL-based $ref is not supported: {ref_str}"
            )));
        }

        let (file_part, pointer) = split_ref(ref_str);

        if file_part.is_empty() {
            // Local ref — resolve the JSON pointer against the current root.
            resolve_pointer(root_doc, pointer)
                .ok_or_else(|| SpecMockCoreError::Ref(format!("local $ref not found: {ref_str}")))
        } else {
            // File-relative ref.
            let file_path = current_base.join(file_part);
            let canonical = std::fs::canonicalize(&file_path).map_err(|e| {
                SpecMockCoreError::Ref(format!(
                    "cannot resolve file ref {}: {e}",
                    file_path.display()
                ))
            })?;
            if !self.allowed_roots.iter().any(|root| canonical.starts_with(root)) {
                let allowed = self
                    .allowed_roots
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(SpecMockCoreError::Ref(format!(
                    "file ref points outside allowed roots [{allowed}]: {}",
                    canonical.display()
                )));
            }
            let cache_key = canonical.to_string_lossy().to_string();

            // Load into cache if not present.
            if !self.cache.contains_key(&cache_key) {
                self.evict_external_cache_if_needed();
                let doc = load_file(&canonical)?;
                self.cache.insert(cache_key.clone(), doc);
                self.cache_order.push_back(cache_key.clone());
            }

            let external_doc = self
                .cache
                .get(&cache_key)
                .ok_or_else(|| SpecMockCoreError::Ref(format!("cache miss for {cache_key}")))?
                .clone();

            let parent = canonical.parent().unwrap_or(current_base).to_path_buf();

            let mut resolved = if pointer.is_empty() {
                external_doc.clone()
            } else {
                resolve_pointer(&external_doc, pointer).ok_or_else(|| {
                    SpecMockCoreError::Ref(format!(
                        "pointer {pointer} not found in {}",
                        canonical.display()
                    ))
                })?
            };

            // Recursively resolve refs inside the external value.
            self.resolve_value(&mut resolved, &external_doc, &parent, depth)?;

            Ok(resolved)
        }
    }

    fn evict_external_cache_if_needed(&mut self) {
        while self.cache_order.len() >= self.external_cache_limit {
            if let Some(evicted_key) = self.cache_order.pop_front() {
                self.cache.remove(&evicted_key);
            } else {
                break;
            }
        }
    }

    #[cfg(test)]
    fn external_cache_size_for_tests(&self) -> usize {
        self.cache.len()
    }
}

/// Split a `$ref` string into `(file_path, json_pointer)`.
///
/// Examples:
/// - `#/components/schemas/Pet` → `("", "/components/schemas/Pet")`
/// - `./schemas/Pet.yaml#/Pet`  → `("./schemas/Pet.yaml", "/Pet")`
/// - `./schemas/Pet.yaml`       → `("./schemas/Pet.yaml", "")`
fn split_ref(ref_str: &str) -> (&str, &str) {
    if let Some(idx) = ref_str.find('#') {
        let file = &ref_str[..idx];
        let pointer = &ref_str[idx + 1..];
        (file, pointer)
    } else {
        (ref_str, "")
    }
}

/// Navigate a JSON pointer (RFC 6901) against a [`Value`] tree.
///
/// The pointer must start with `/` (the leading `/` is stripped before
/// splitting). Returns `None` when the path cannot be followed.
pub fn resolve_pointer(root: &Value, pointer: &str) -> Option<Value> {
    if pointer.is_empty() || pointer == "/" {
        return Some(root.clone());
    }

    let segments = pointer
        .strip_prefix('/')
        .unwrap_or(pointer)
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"));

    let mut current = root;
    for seg in segments {
        match current {
            Value::Object(map) => {
                current = map.get(&seg)?;
            }
            Value::Array(arr) => {
                let idx: usize = seg.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }

    Some(current.clone())
}

/// Load a file as [`serde_json::Value`], selecting the parser by extension.
fn load_file(path: &Path) -> Result<Value, SpecMockCoreError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| SpecMockCoreError::Ref(format!("cannot read {}: {e}", path.display())))?;

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "yaml" | "yml" => {
            let yaml_value: serde_yml::Value = serde_yml::from_str(&content).map_err(|e| {
                SpecMockCoreError::Ref(format!("YAML parse error in {}: {e}", path.display()))
            })?;
            serde_json::to_value(yaml_value).map_err(|e| {
                SpecMockCoreError::Ref(format!(
                    "YAML→JSON conversion error in {}: {e}",
                    path.display()
                ))
            })
        }
        _ => serde_json::from_str(&content).map_err(|e| {
            SpecMockCoreError::Ref(format!("JSON parse error in {}: {e}", path.display()))
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// Helper: write a JSON file into a temp directory.
    fn write_json(dir: &Path, name: &str, value: &Value) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut f = std::fs::File::create(&path).expect("create file");
        f.write_all(serde_json::to_string_pretty(value).expect("serialize").as_bytes())
            .expect("write");
        path
    }

    /// Helper: write a YAML file into a temp directory.
    fn write_yaml(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, content).expect("write yaml");
        path
    }

    // ── Local ref ──────────────────────────────────────────────────────

    #[test]
    fn resolve_local_ref() {
        let tmp = TempDir::new().expect("tmpdir");
        let doc = json!({
            "components": {
                "schemas": {
                    "Pet": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                }
            },
            "paths": {
                "/pets": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "#/components/schemas/Pet" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let main_file = write_json(tmp.path(), "api.json", &doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let resolved = resolver.resolve(&main_file).expect("resolve");

        let schema = &resolved["paths"]["/pets"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["name"]["type"], "string");
        assert!(schema.get("$ref").is_none(), "$ref should be removed");
    }

    // ── File-relative ref ──────────────────────────────────────────────

    #[test]
    fn resolve_file_relative_ref() {
        let tmp = TempDir::new().expect("tmpdir");

        // External schema file.
        let pet_schema = json!({
            "Pet": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "name": { "type": "string" }
                }
            }
        });
        write_json(tmp.path(), "schemas/pet.json", &pet_schema);

        // Main spec referencing the external file.
        let main_doc = json!({
            "paths": {
                "/pets": {
                    "get": {
                        "responses": {
                            "200": {
                                "schema": { "$ref": "./schemas/pet.json#/Pet" }
                            }
                        }
                    }
                }
            }
        });
        let main_file = write_json(tmp.path(), "api.json", &main_doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let resolved = resolver.resolve(&main_file).expect("resolve");

        let schema = &resolved["paths"]["/pets"]["get"]["responses"]["200"]["schema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["id"]["type"], "integer");
    }

    // ── YAML file ref ──────────────────────────────────────────────────

    #[test]
    fn resolve_yaml_file_ref() {
        let tmp = TempDir::new().expect("tmpdir");

        write_yaml(
            tmp.path(),
            "schemas/pet.yaml",
            "Pet:\n  type: object\n  properties:\n    name:\n      type: string\n",
        );

        let main_doc = json!({
            "definitions": {
                "pet": { "$ref": "./schemas/pet.yaml#/Pet" }
            }
        });
        let main_file = write_json(tmp.path(), "api.json", &main_doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let resolved = resolver.resolve(&main_file).expect("resolve");

        let pet = &resolved["definitions"]["pet"];
        assert_eq!(pet["type"], "object");
        assert_eq!(pet["properties"]["name"]["type"], "string");
    }

    // ── Cycle detection ────────────────────────────────────────────────

    #[test]
    fn detect_cycle_via_depth_limit() {
        let tmp = TempDir::new().expect("tmpdir");

        // A refers to B, B refers to A.
        let a = json!({ "value": { "$ref": "./b.json#/value" } });
        let b = json!({ "value": { "$ref": "./a.json#/value" } });
        write_json(tmp.path(), "a.json", &a);
        write_json(tmp.path(), "b.json", &b);

        let main_doc = json!({ "root": { "$ref": "./a.json#/value" } });
        let main_file = write_json(tmp.path(), "main.json", &main_doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf()).with_max_depth(10);
        let err = resolver.resolve(&main_file).expect_err("should detect cycle");

        let msg = err.to_string();
        assert!(
            msg.contains("depth") || msg.contains("cycle"),
            "error should mention depth or cycle: {msg}"
        );
    }

    // ── Missing ref ────────────────────────────────────────────────────

    #[test]
    fn missing_local_ref_returns_error() {
        let tmp = TempDir::new().expect("tmpdir");
        let doc = json!({
            "schema": { "$ref": "#/components/schemas/DoesNotExist" }
        });
        let main_file = write_json(tmp.path(), "api.json", &doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let err = resolver.resolve(&main_file).expect_err("should fail");
        assert!(err.to_string().contains("not found"), "error: {err}");
    }

    #[test]
    fn missing_file_ref_returns_error() {
        let tmp = TempDir::new().expect("tmpdir");
        let doc = json!({
            "schema": { "$ref": "./nonexistent.json#/Foo" }
        });
        let main_file = write_json(tmp.path(), "api.json", &doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let err = resolver.resolve(&main_file).expect_err("should fail");
        assert!(
            err.to_string().contains("cannot resolve") || err.to_string().contains("cannot read"),
            "error: {err}"
        );
    }

    // ── Deeply nested refs (5+ levels) ─────────────────────────────────

    #[test]
    fn resolve_deeply_nested_refs() {
        let tmp = TempDir::new().expect("tmpdir");

        // Chain: main → level1 → level2 → level3 → level4 → level5 (leaf).
        write_json(
            tmp.path(),
            "level5.json",
            &json!({ "leaf": { "type": "string", "example": "deep" } }),
        );
        write_json(
            tmp.path(),
            "level4.json",
            &json!({ "next": { "$ref": "./level5.json#/leaf" } }),
        );
        write_json(
            tmp.path(),
            "level3.json",
            &json!({ "next": { "$ref": "./level4.json#/next" } }),
        );
        write_json(
            tmp.path(),
            "level2.json",
            &json!({ "next": { "$ref": "./level3.json#/next" } }),
        );
        write_json(
            tmp.path(),
            "level1.json",
            &json!({ "next": { "$ref": "./level2.json#/next" } }),
        );

        let main_doc = json!({
            "result": { "$ref": "./level1.json#/next" }
        });
        let main_file = write_json(tmp.path(), "main.json", &main_doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let resolved = resolver.resolve(&main_file).expect("resolve");

        assert_eq!(resolved["result"]["type"], "string");
        assert_eq!(resolved["result"]["example"], "deep");
    }

    // ── Sibling property preservation ──────────────────────────────────

    #[test]
    fn sibling_properties_are_preserved() {
        let tmp = TempDir::new().expect("tmpdir");
        let doc = json!({
            "components": {
                "schemas": {
                    "Pet": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }
                }
            },
            "paths": {
                "/pets": {
                    "schema": {
                        "$ref": "#/components/schemas/Pet",
                        "description": "A pet object"
                    }
                }
            }
        });
        let main_file = write_json(tmp.path(), "api.json", &doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let resolved = resolver.resolve(&main_file).expect("resolve");

        let schema = &resolved["paths"]["/pets"]["schema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["description"], "A pet object");
    }

    // ── URL ref rejection ──────────────────────────────────────────────

    #[test]
    fn url_ref_returns_error() {
        let tmp = TempDir::new().expect("tmpdir");
        let doc = json!({
            "schema": { "$ref": "https://example.com/schemas/Pet.json#/Pet" }
        });
        let main_file = write_json(tmp.path(), "api.json", &doc);

        let mut resolver = RefResolver::new(tmp.path().to_path_buf());
        let err = resolver.resolve(&main_file).expect_err("should fail");
        assert!(err.to_string().contains("URL-based"), "error: {err}");
    }

    #[test]
    fn external_file_ref_outside_base_dir_returns_error() {
        let root = TempDir::new().expect("root tmpdir");
        let outside = TempDir::new().expect("outside tmpdir");

        let outside_schema = json!({
            "Pet": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            }
        });
        let outside_path = write_json(outside.path(), "pet.json", &outside_schema);

        let main_doc = json!({
            "paths": {
                "/pets": {
                    "get": {
                        "responses": {
                            "200": {
                                "schema": {
                                    "$ref": format!("{}#/Pet", outside_path.display())
                                }
                            }
                        }
                    }
                }
            }
        });
        let main_file = write_json(root.path(), "api.json", &main_doc);

        let mut resolver = RefResolver::new(root.path().to_path_buf());
        let err = resolver.resolve(&main_file).expect_err("should reject refs outside base dir");
        assert!(err.to_string().contains("outside allowed roots"), "unexpected error: {err}");
    }

    #[test]
    fn additional_allowed_root_allows_external_file_ref() {
        let root = TempDir::new().expect("root tmpdir");
        let outside = TempDir::new().expect("outside tmpdir");

        let outside_schema = json!({
            "Pet": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            }
        });
        let outside_path = write_json(outside.path(), "pet.json", &outside_schema);

        let main_doc = json!({
            "paths": {
                "/pets": {
                    "get": {
                        "responses": {
                            "200": {
                                "schema": {
                                    "$ref": format!("{}#/Pet", outside_path.display())
                                }
                            }
                        }
                    }
                }
            }
        });
        let main_file = write_json(root.path(), "api.json", &main_doc);

        let mut resolver = RefResolver::new(root.path().to_path_buf())
            .with_allowed_root(outside.path().to_path_buf());
        let resolved = resolver.resolve(&main_file).expect("should allow external ref");
        assert_eq!(
            resolved["paths"]["/pets"]["get"]["responses"]["200"]["schema"]["type"],
            "object"
        );
    }

    #[test]
    fn external_doc_cache_respects_limit() {
        let root = TempDir::new().expect("root tmpdir");

        write_json(root.path(), "schemas/a.json", &json!({"A":{"type":"string"}}));
        write_json(root.path(), "schemas/b.json", &json!({"B":{"type":"integer"}}));
        write_json(root.path(), "schemas/c.json", &json!({"C":{"type":"boolean"}}));

        let main_a =
            write_json(root.path(), "a.json", &json!({"schema":{"$ref":"./schemas/a.json#/A"}}));
        let main_b =
            write_json(root.path(), "b.json", &json!({"schema":{"$ref":"./schemas/b.json#/B"}}));
        let main_c =
            write_json(root.path(), "c.json", &json!({"schema":{"$ref":"./schemas/c.json#/C"}}));

        let mut resolver = RefResolver::new(root.path().to_path_buf()).with_external_cache_limit(2);
        let _ra = resolver.resolve(&main_a).expect("resolve a");
        let _rb = resolver.resolve(&main_b).expect("resolve b");
        let _rc = resolver.resolve(&main_c).expect("resolve c");

        assert!(
            resolver.external_cache_size_for_tests() <= 2,
            "external doc cache should not exceed configured limit"
        );
    }

    // ── split_ref unit tests ───────────────────────────────────────────

    #[test]
    fn split_ref_local() {
        assert_eq!(split_ref("#/components/schemas/Pet"), ("", "/components/schemas/Pet"));
    }

    #[test]
    fn split_ref_file_with_pointer() {
        assert_eq!(split_ref("./schemas/Pet.yaml#/Pet"), ("./schemas/Pet.yaml", "/Pet"));
    }

    #[test]
    fn split_ref_file_without_pointer() {
        assert_eq!(split_ref("./schemas/Pet.yaml"), ("./schemas/Pet.yaml", ""));
    }

    // ── resolve_pointer unit tests ─────────────────────────────────────

    #[test]
    fn resolve_pointer_root() {
        let v = json!({"a": 1});
        assert_eq!(resolve_pointer(&v, ""), Some(v.clone()));
    }

    #[test]
    fn resolve_pointer_nested() {
        let v = json!({"a": {"b": {"c": 42}}});
        assert_eq!(resolve_pointer(&v, "/a/b/c"), Some(json!(42)));
    }

    #[test]
    fn resolve_pointer_missing() {
        let v = json!({"a": 1});
        assert_eq!(resolve_pointer(&v, "/b"), None);
    }
}
