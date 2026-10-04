//! The scenario sidecar: parsing and validation.
//!
//! Behaviour lives beside the OpenAPI document rather than inside it. Requiring
//! `x-specmock-*` extensions would mean forking the input spec, and forking
//! breaks every `$ref` into shared schema files -- after which the mock is no
//! longer exercising the real contract. See ADR 0002.
//!
//! Validation is strict and happens at load time, matching how an unresolvable
//! `$ref` already fails startup. A dangling state reference is a typo and stops
//! the run; an unreachable state is merely dead configuration and does not.
//!
//! Loading from disk, including `$ref` resolution through the shared
//! [`crate::ref_resolver::RefResolver`], is the runtime's job -- this module
//! works from an already-resolved [`Value`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{scenario::State, store::StoreAction};

/// The only sidecar schema version this build understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// Prefix reserved for the mock's own control endpoints.
///
/// A sidecar declaring an operation beneath it would be silently unreachable:
/// control routes are registered explicitly and explicit routes beat the
/// fallback that serves OpenAPI paths.
pub const CONTROL_PREFIX: &str = "/_specmock";

/// Why a sidecar was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SidecarError {
    /// The document is not shaped like a sidecar at all.
    #[error("sidecar is not a valid scenario document: {0}")]
    Malformed(String),
    /// `version` is absent or not a supported value.
    #[error("unsupported sidecar version {found}; this build supports {SUPPORTED_VERSION}")]
    UnsupportedVersion {
        /// The version the document declared.
        found: String,
    },
    /// A scenario names a state it never declares.
    #[error("scenario '{scenario}': unknown state '{state}'")]
    UnknownState {
        /// Scenario that referenced it.
        scenario: String,
        /// The undeclared state.
        state: String,
    },
    /// A transition targets a state that is not declared.
    #[error("scenario '{scenario}', state '{state}': transition to unknown state '{target}'")]
    UnknownTransitionTarget {
        /// Scenario that referenced it.
        scenario: String,
        /// State holding the transition.
        state: String,
        /// The undeclared target.
        target: String,
    },
    /// A state references an operation absent from the catalogue.
    #[error("scenario '{scenario}', state '{state}': unknown operation '{operation}'")]
    UnknownOperation {
        /// Scenario that referenced it.
        scenario: String,
        /// State holding the reference.
        state: String,
        /// The undeclared operation.
        operation: String,
    },
    /// Two scenarios share a name.
    #[error("duplicate scenario name '{0}'")]
    DuplicateScenario(String),
    /// A scenario's initial state is not among its declared states.
    #[error("scenario '{scenario}': initial state '{initial}' is not declared")]
    UnknownInitialState {
        /// Scenario that referenced it.
        scenario: String,
        /// The undeclared initial state.
        initial: String,
    },
    /// An operation declares no store interaction and no response override.
    ///
    /// Not fatal on its own, but almost always a mistake, so it is surfaced
    /// rather than ignored.
    #[error("operation '{operation}' declares neither a store interaction nor a response")]
    InertOperation {
        /// The operation in question.
        operation: String,
    },
    /// A `create` declares no key source.
    #[error("operation '{operation}': store action 'create' needs keyFrom")]
    MissingCreateKey {
        /// The operation in question.
        operation: String,
    },
    /// A non-create action declares a key source it cannot use.
    #[error("operation '{operation}': store action '{action:?}' does not accept keyFrom")]
    UnexpectedCreateKey {
        /// The operation in question.
        operation: String,
        /// The action that rejected it.
        action: StoreAction,
    },
    /// A create's key pointer does not resolve against a generated body.
    #[error("operation '{operation}': keyFrom pointer '{pointer}' is not a valid JSON pointer")]
    BadKeyPointer {
        /// The operation in question.
        operation: String,
        /// The offending pointer.
        pointer: String,
    },
}

/// A store interaction declared by an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreSpec {
    /// Collection path template this operation's resources belong to.
    ///
    /// Doubles as the key namespace, so `/users/5/orders` and `/orders` never
    /// collide on a shared identifier.
    pub resource: String,
    /// What the operation does to the store.
    pub action: StoreActionWire,
    /// Path parameter holding the identifier, for read/update/delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Where a created resource's identifier comes from.
    #[serde(default, rename = "keyFrom", skip_serializing_if = "Option::is_none")]
    pub key_from: Option<KeySource>,
}

impl StoreSpec {
    /// The action in its typed form.
    #[must_use]
    pub fn action(&self) -> StoreAction {
        self.action.into()
    }
}

/// Serialisable form of [`StoreAction`], lowercase on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreActionWire {
    /// Store a new resource.
    Create,
    /// Read an existing resource.
    Read,
    /// Replace an existing resource.
    Update,
    /// Remove an existing resource.
    Delete,
    /// List a namespace.
    List,
}

impl From<StoreActionWire> for StoreAction {
    fn from(action: StoreActionWire) -> Self {
        match action {
            StoreActionWire::Create => Self::Create,
            StoreActionWire::Read => Self::Read,
            StoreActionWire::Update => Self::Update,
            StoreActionWire::Delete => Self::Delete,
            StoreActionWire::List => Self::List,
        }
    }
}

/// Where a created resource's identifier is read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySource {
    /// Which part of the response carries the identifier.
    #[serde(rename = "in")]
    pub location: KeyLocation,
    /// JSON pointer to the identifier within it.
    pub pointer: String,
}

/// Which part of a response holds a created resource's identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyLocation {
    /// The request payload, which for a create is what the client supplied.
    Request,
    /// The generated response payload.
    Response,
}

/// An operation's request matcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationMatch {
    /// HTTP method, uppercase.
    pub method: String,
    /// Path template as written in the OpenAPI document.
    pub path: String,
}

/// A declared response override, keyed by status code or `default`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseOverride {
    /// Literal body, used when the schema alone would generate something else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// One entry in the operation catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEntry {
    /// Which operation this entry configures.
    #[serde(rename = "match")]
    pub matcher: OperationMatch,
    /// Store interaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<StoreSpec>,
    /// Status-keyed response overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub responses: BTreeMap<String, ResponseOverride>,
}

impl OperationEntry {
    /// A stable identifier for this operation, used as the FSM's operation key.
    ///
    /// `operationId` is deliberately not used: it is optional in OpenAPI, so it
    /// cannot be relied on to exist.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{} {}", self.matcher.method.to_ascii_uppercase(), self.matcher.path)
    }
}

/// Resources a scenario starts with.
///
/// "An authenticated user hits a protected endpoint" is the canonical test for
/// a protected endpoint, and without pre-seeding it is forced through a login
/// flow it is not testing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedEntry {
    /// Namespace to seed.
    pub resource: String,
    /// Resources to place in it.
    pub items: Vec<Value>,
}

/// What an operation does in one state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepEntry {
    /// State to move to. Absent means the operation is legal here and the
    /// instance stays put -- not a self-loop, and not an error.
    #[serde(default, rename = "to", skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Status-keyed response overrides for this state only.
    ///
    /// This is how fault injection is expressed: an error is a transition into
    /// a state that happens to declare an error response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub responses: BTreeMap<String, ResponseOverride>,
}

/// A scenario as written in the sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioEntry {
    /// Scenario name, selected with `Prefer: scenario=`.
    pub name: String,
    /// State a fresh instance starts in.
    #[serde(rename = "initialState")]
    pub initial_state: String,
    /// Resources present before any request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seed: Vec<SeedEntry>,
    /// State to the operations legal there.
    pub states: BTreeMap<String, BTreeMap<String, StepEntry>>,
}

/// A parsed sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    /// Schema version. Required, so a future format change fails loudly instead
    /// of silently changing behaviour.
    pub version: u32,
    /// Operations scenarios refer to by name.
    #[serde(default)]
    pub operations: BTreeMap<String, OperationEntry>,
    /// Declared scenarios.
    #[serde(default)]
    pub scenarios: Vec<ScenarioEntry>,
}

/// Non-fatal observations worth reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarWarning {
    /// A declared state no transition can reach.
    UnreachableState {
        /// Scenario holding it.
        scenario: String,
        /// The dead state.
        state: String,
    },
    /// An operation no scenario references.
    UnusedOperation(String),
}

impl std::fmt::Display for SidecarWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnreachableState { scenario, state } => {
                write!(formatter, "scenario '{scenario}': state '{state}' is unreachable")
            }
            Self::UnusedOperation(operation) => {
                write!(formatter, "operation '{operation}' is declared but never used")
            }
        }
    }
}

/// A sidecar that parsed and passed validation.
#[derive(Debug, Clone)]
pub struct ValidatedSidecar {
    sidecar: Sidecar,
    warnings: Vec<SidecarWarning>,
}

impl ValidatedSidecar {
    /// The sidecar document.
    #[must_use]
    pub fn sidecar(&self) -> &Sidecar {
        &self.sidecar
    }

    /// Non-fatal observations.
    #[must_use]
    pub fn warnings(&self) -> &[SidecarWarning] {
        &self.warnings
    }

    /// Look up a scenario by name.
    #[must_use]
    pub fn scenario(&self, name: &str) -> Option<&ScenarioEntry> {
        self.sidecar.scenarios.iter().find(|scenario| scenario.name == name)
    }

    /// The scenario used when a request names none.
    #[must_use]
    pub fn default_scenario(&self) -> Option<&ScenarioEntry> {
        self.sidecar.scenarios.first()
    }

    /// Declared scenario names, for error messages.
    #[must_use]
    pub fn scenario_names(&self) -> Vec<&str> {
        self.sidecar.scenarios.iter().map(|scenario| scenario.name.as_str()).collect()
    }
}

/// Parse and validate a resolved sidecar document.
pub fn parse(value: &Value) -> Result<ValidatedSidecar, SidecarError> {
    let sidecar: Sidecar = serde_json::from_value(value.clone())
        .map_err(|error| SidecarError::Malformed(error.to_string()))?;

    if sidecar.version != SUPPORTED_VERSION {
        return Err(SidecarError::UnsupportedVersion { found: sidecar.version.to_string() });
    }

    validate(&sidecar)?;
    let warnings = collect_warnings(&sidecar);
    Ok(ValidatedSidecar { sidecar, warnings })
}

fn validate(sidecar: &Sidecar) -> Result<(), SidecarError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();

    for (name, entry) in &sidecar.operations {
        validate_operation(name, entry)?;
        if entry.matcher.path.starts_with(CONTROL_PREFIX) {
            return Err(SidecarError::UnknownOperation {
                scenario: "-".to_owned(),
                state: "-".to_owned(),
                operation: format!("{name} declares '{}', which is reserved", entry.matcher.path),
            });
        }
    }

    for scenario in &sidecar.scenarios {
        if !seen.insert(scenario.name.as_str()) {
            return Err(SidecarError::DuplicateScenario(scenario.name.clone()));
        }

        if !scenario.states.contains_key(&scenario.initial_state) {
            return Err(SidecarError::UnknownInitialState {
                scenario: scenario.name.clone(),
                initial: scenario.initial_state.clone(),
            });
        }

        for (state, operations) in &scenario.states {
            for (operation, step) in operations {
                if !sidecar.operations.contains_key(operation) {
                    return Err(SidecarError::UnknownOperation {
                        scenario: scenario.name.clone(),
                        state: state.clone(),
                        operation: operation.clone(),
                    });
                }
                if let Some(target) = &step.target &&
                    !scenario.states.contains_key(target)
                {
                    return Err(SidecarError::UnknownTransitionTarget {
                        scenario: scenario.name.clone(),
                        state: state.clone(),
                        target: target.clone(),
                    });
                }
            }
        }
    }

    Ok(())
}

fn validate_operation(name: &str, entry: &OperationEntry) -> Result<(), SidecarError> {
    let Some(store) = &entry.store else {
        if entry.responses.is_empty() {
            return Err(SidecarError::InertOperation { operation: name.to_owned() });
        }
        return Ok(());
    };

    match store.action() {
        StoreAction::Create => {
            let Some(source) = &store.key_from else {
                return Err(SidecarError::MissingCreateKey { operation: name.to_owned() });
            };
            if !is_valid_pointer(&source.pointer) {
                return Err(SidecarError::BadKeyPointer {
                    operation: name.to_owned(),
                    pointer: source.pointer.clone(),
                });
            }
        }
        StoreAction::List => {}
        StoreAction::Read | StoreAction::Update | StoreAction::Delete => {
            if store.key_from.is_some() {
                return Err(SidecarError::UnexpectedCreateKey {
                    operation: name.to_owned(),
                    action: store.action(),
                });
            }
            if store.key.is_none() {
                return Err(SidecarError::MissingCreateKey { operation: name.to_owned() });
            }
        }
    }
    Ok(())
}

fn collect_warnings(sidecar: &Sidecar) -> Vec<SidecarWarning> {
    let mut warnings = Vec::new();

    for scenario in &sidecar.scenarios {
        let used: BTreeSet<&str> = scenario
            .states
            .values()
            .flat_map(|operations| operations.keys().map(String::as_str))
            .collect();

        let machine = crate::scenario::Scenario::new(
            scenario.name.clone(),
            State::new(scenario.initial_state.clone()),
            scenario
                .states
                .iter()
                .map(|(state, operations)| {
                    (
                        State::new(state.clone()),
                        operations
                            .iter()
                            .map(|(operation, step)| {
                                (
                                    operation.clone(),
                                    step.target.as_ref().map_or_else(
                                        crate::scenario::Transition::stay,
                                        |target| {
                                            crate::scenario::Transition::to(State::new(
                                                target.clone(),
                                            ))
                                        },
                                    ),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
        );

        for state in machine.unreachable_states() {
            warnings.push(SidecarWarning::UnreachableState {
                scenario: scenario.name.clone(),
                state: state.name().to_owned(),
            });
        }

        for operation in sidecar.operations.keys() {
            if !used.contains(operation.as_str()) {
                warnings.push(SidecarWarning::UnusedOperation(operation.clone()));
            }
        }
    }

    warnings
}

/// A JSON pointer is either empty (the whole document) or `/`-rooted.
fn is_valid_pointer(pointer: &str) -> bool {
    pointer.is_empty() || pointer.starts_with('/')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn minimal() -> Value {
        json!({
            "version": 1,
            "operations": {
                "readUser": {
                    "match": { "method": "get", "path": "/users/{id}" },
                    "store": { "resource": "/users", "action": "read", "key": "{id}" }
                }
            },
            "scenarios": [{
                "name": "browse",
                "initialState": "anonymous",
                "states": { "anonymous": { "readUser": {} } }
            }]
        })
    }

    /// Parse expecting rejection, returning the error for comparison.
    fn err(value: &Value) -> SidecarError {
        match parse(value) {
            Ok(_) => panic!("expected the sidecar to be rejected"),
            Err(error) => error,
        }
    }

    #[test]
    fn a_minimal_sidecar_parses() {
        let validated = parse(&minimal()).expect("minimal sidecar should parse");
        assert_eq!(validated.sidecar().version, 1);
        assert_eq!(validated.scenario_names(), ["browse"]);
    }

    #[test]
    fn operation_keys_uppercase_the_method_and_ignore_the_catalogue_name() {
        let validated = parse(&minimal()).expect("parse");
        let entry = &validated.sidecar().operations["readUser"];
        assert_eq!(entry.key(), "GET /users/{id}");
    }

    #[test]
    fn a_missing_version_is_rejected() {
        let mut value = minimal();
        value.as_object_mut().expect("object").remove("version");
        assert!(matches!(parse(&value), Err(SidecarError::Malformed(_))));
    }

    #[test]
    fn an_unsupported_version_names_what_is_supported() {
        let mut value = minimal();
        value["version"] = json!(99);
        let Err(SidecarError::UnsupportedVersion { found }) = parse(&value) else {
            panic!("expected an unsupported version error");
        };
        assert_eq!(found, "99");
    }

    #[test]
    fn an_unknown_transition_target_is_rejected() {
        let mut value = minimal();
        value["scenarios"][0]["states"]["anonymous"]["readUser"] = json!({"to": "nowhere"});
        assert_eq!(
            err(&value),
            SidecarError::UnknownTransitionTarget {
                scenario: "browse".to_owned(),
                state: "anonymous".to_owned(),
                target: "nowhere".to_owned(),
            }
        );
    }

    #[test]
    fn an_unknown_operation_reference_is_rejected() {
        let mut value = minimal();
        value["scenarios"][0]["states"]["anonymous"] = json!({"missing": {}});
        assert_eq!(
            err(&value),
            SidecarError::UnknownOperation {
                scenario: "browse".to_owned(),
                state: "anonymous".to_owned(),
                operation: "missing".to_owned(),
            }
        );
    }

    #[test]
    fn an_undeclared_initial_state_is_rejected() {
        let mut value = minimal();
        value["scenarios"][0]["initialState"] = json!("absent");
        assert_eq!(
            err(&value),
            SidecarError::UnknownInitialState {
                scenario: "browse".to_owned(),
                initial: "absent".to_owned(),
            }
        );
    }

    #[test]
    fn a_create_without_a_key_source_is_rejected() {
        let mut value = minimal();
        value["operations"]["createUser"] = json!({
            "match": { "method": "POST", "path": "/users" },
            "store": { "resource": "/users", "action": "create" }
        });
        value["scenarios"][0]["states"]["anonymous"]["createUser"] = json!({});
        assert_eq!(
            err(&value),
            SidecarError::MissingCreateKey { operation: "createUser".to_owned() }
        );
    }

    #[test]
    fn a_create_with_a_malformed_key_pointer_is_rejected() {
        let mut value = minimal();
        value["operations"]["createUser"] = json!({
            "match": { "method": "POST", "path": "/users" },
            "store": {
                "resource": "/users",
                "action": "create",
                "keyFrom": { "in": "response", "pointer": "id" }
            }
        });
        value["scenarios"][0]["states"]["anonymous"]["createUser"] = json!({});
        assert_eq!(
            err(&value),
            SidecarError::BadKeyPointer {
                operation: "createUser".to_owned(),
                pointer: "id".to_owned(),
            }
        );
    }

    #[test]
    fn a_read_that_declares_key_from_is_rejected() {
        let mut value = minimal();
        value["operations"]["readUser"]["store"]["keyFrom"] =
            json!({ "in": "response", "pointer": "/id" });
        assert!(matches!(parse(&value), Err(SidecarError::UnexpectedCreateKey { .. })));
    }

    #[test]
    fn an_inert_operation_is_rejected() {
        let mut value = minimal();
        value["operations"]["noop"] = json!({
            "match": { "method": "GET", "path": "/nothing" }
        });
        value["scenarios"][0]["states"]["anonymous"]["noop"] = json!({});
        assert_eq!(err(&value), SidecarError::InertOperation { operation: "noop".to_owned() });
    }

    #[test]
    fn duplicate_scenario_names_are_rejected() {
        let mut value = minimal();
        let scenario = value["scenarios"][0].clone();
        value["scenarios"].as_array_mut().expect("array").push(scenario);
        assert_eq!(err(&value), SidecarError::DuplicateScenario("browse".to_owned()));
    }

    #[test]
    fn an_operation_beneath_the_control_prefix_is_rejected() {
        let mut value = minimal();
        value["operations"]["readUser"]["match"]["path"] = json!("/_specmock/state");
        assert!(parse(&value).is_err(), "the control prefix must be reserved");
    }

    #[test]
    fn an_unreachable_state_warns_but_does_not_fail() {
        let mut value = minimal();
        value["scenarios"][0]["states"]["orphan"] = json!({});
        let validated = parse(&value).expect("an unreachable state is not fatal");
        assert_eq!(
            validated.warnings(),
            [SidecarWarning::UnreachableState {
                scenario: "browse".to_owned(),
                state: "orphan".to_owned(),
            }]
        );
    }

    #[test]
    fn an_operation_no_scenario_uses_warns() {
        let mut value = minimal();
        value["operations"]["spare"] = json!({
            "match": { "method": "GET", "path": "/spare" },
            "responses": { "200": {} }
        });
        let validated = parse(&value).expect("an unused operation is not fatal");
        assert!(
            validated.warnings().contains(&SidecarWarning::UnusedOperation("spare".to_owned())),
            "got {:?}",
            validated.warnings()
        );
    }

    #[test]
    fn seed_entries_round_trip() {
        let mut value = minimal();
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let validated = parse(&value).expect("seed entries should parse");
        let seed = &validated.scenario("browse").expect("scenario").seed;
        assert_eq!(seed[0].resource, "/users");
        assert_eq!(seed[0].items, vec![json!({"id": "u1"})]);
    }

    #[test]
    fn a_state_specific_response_override_is_preserved() {
        let mut value = minimal();
        value["scenarios"][0]["states"]["anonymous"]["readUser"] =
            json!({ "responses": { "503": { "body": { "down": true } } } });
        let validated = parse(&value).expect("parse");
        let step = &validated.scenario("browse").expect("scenario").states["anonymous"]["readUser"];
        assert_eq!(
            step.responses["503"].body,
            Some(json!({"down": true})),
            "a state-specific override is how fault injection is expressed"
        );
    }

    #[test]
    fn store_actions_parse_from_their_wire_names() {
        // `create` takes `keyFrom`; the keyed actions take `key`; `list` takes
        // neither, so each case gets a sidecar shaped for it.
        for (wire, expected, keyed) in [
            ("create", StoreAction::Create, false),
            ("read", StoreAction::Read, true),
            ("update", StoreAction::Update, true),
            ("delete", StoreAction::Delete, true),
            ("list", StoreAction::List, true),
        ] {
            let store = if keyed {
                json!({ "resource": "/users", "action": wire, "key": "{id}" })
            } else {
                json!({
                    "resource": "/users",
                    "action": wire,
                    "keyFrom": { "in": "response", "pointer": "/id" }
                })
            };
            let mut value = minimal();
            value["operations"]["readUser"]["store"] = store;

            let validated = parse(&value).unwrap_or_else(|e| panic!("{wire} should parse: {e}"));
            assert_eq!(
                validated.sidecar().operations["readUser"].store.as_ref().expect("store").action(),
                expected
            );
        }
    }

    #[test]
    fn json_pointers_must_be_rooted_or_empty() {
        assert!(is_valid_pointer(""));
        assert!(is_valid_pointer("/id"));
        assert!(is_valid_pointer("/a/b/0"));
        assert!(!is_valid_pointer("id"));
        assert!(!is_valid_pointer("#/id"));
    }
}
