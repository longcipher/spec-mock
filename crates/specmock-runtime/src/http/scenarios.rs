//! Session-scoped scenario execution for the HTTP runtime.
//!
//! A scenario instance pairs a current state with a store. Requests reach one
//! through two `Prefer` directives: `scenario=` names the flow (absent, the
//! first declared scenario is used) and `session=` names an isolated instance
//! (absent, one default instance serves everything).
//!
//! The singleton default exists because embedded mode already gives each test
//! its own server while process mode shares one child across the whole session.
//! Keying is opt-in so parallel tests can isolate themselves without every test
//! having to opt in.
//!
//! Two directives bypass this module entirely: `code=` and `dynamic=true`. Both
//! are read-only escape hatches for looking at a mock from outside the flow it
//! is in, so neither advances state. Letting them change behaviour based on
//! hidden server state would give one header two meanings.
//!
//! Locking discipline: the instance state is read and written under a short
//! critical section each, never nested. Legality is checked against a snapshot
//! taken under one lock, and the advance takes the lock again afterwards, so no
//! path ever holds two guards on the same mutex.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

use scc::HashMap as ConcurrentMap;
use serde_json::Value;
use specmock_core::{
    scenario::{Scenario, State, Transition},
    sidecar::{
        KeyLocation, ResponseOverride, SeedEntry, SidecarError, StoreSpec, ValidatedSidecar,
    },
    store::{Store, StoreAction, StoreKey},
};

/// Session key used when a request supplies none.
pub const DEFAULT_SESSION: &str = "__default__";

/// A failure that is not a schema violation.
///
/// Reported as RFC 7807 with structured extension members rather than as a
/// `ValidationIssue`: every field of a `ValidationIssue` describes a JSON Schema
/// keyword, and a refused transition has no instance and no schema pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatefulFailure {
    /// The named scenario does not exist.
    UnknownScenario {
        /// What the client asked for.
        requested: String,
        /// Scenarios that do exist.
        available: Vec<String>,
    },
    /// The operation is not legal in the current state.
    IllegalOperation {
        /// Scenario holding the state.
        scenario: String,
        /// Session being served.
        session: String,
        /// Current state.
        state: String,
        /// Operation that was refused.
        operation: String,
    },
    /// The store holds no such resource.
    ResourceNotFound {
        /// Namespaced key that was looked up.
        key: String,
    },
    /// A create collided with an existing resource.
    ResourceExists {
        /// Namespaced key that was taken.
        key: String,
    },
    /// Too many live sessions.
    SessionCapExceeded {
        /// The configured limit.
        limit: usize,
    },
    /// A store reached its entry limit.
    StoreCapExceeded {
        /// The configured limit.
        limit: usize,
    },
}

impl StatefulFailure {
    /// The HTTP status this failure reports.
    ///
    /// The session/store asymmetry is deliberate: exceeding the session cap is
    /// client-driven, so `429`. Exceeding the store cap usually means the
    /// scenario itself is wrong, so `500`. `409` is RFC 9110's "conflict with the
    /// current state", which is precisely an operation the FSM refuses.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::UnknownScenario { .. } | Self::ResourceNotFound { .. } => 404,
            Self::IllegalOperation { .. } | Self::ResourceExists { .. } => 409,
            Self::SessionCapExceeded { .. } => 429,
            Self::StoreCapExceeded { .. } => 500,
        }
    }

    /// A human-readable explanation naming the fix where one exists.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::UnknownScenario { requested, available } => {
                if available.is_empty() {
                    format!("no scenarios are declared, so '{requested}' cannot be resolved")
                } else {
                    format!(
                        "unknown scenario '{requested}'; declared scenarios: {}",
                        available.join(", ")
                    )
                }
            }
            Self::IllegalOperation { scenario, session, state, operation } => format!(
                "operation '{operation}' is not legal in state '{state}' of scenario \
                 '{scenario}' (session '{session}')"
            ),
            Self::ResourceNotFound { key } => format!("no resource exists at {key}"),
            Self::ResourceExists { key } => format!("a resource already exists at {key}"),
            Self::SessionCapExceeded { limit } => {
                format!("live session cap of {limit} reached; raise --max-sessions")
            }
            Self::StoreCapExceeded { limit } => {
                format!("store entry cap of {limit} reached; raise --max-store-entries")
            }
        }
    }

    /// Structured extension members naming the scenario context.
    #[must_use]
    pub fn extensions(&self) -> Vec<(String, Value)> {
        match self {
            Self::IllegalOperation { scenario, session, state, operation } => vec![
                ("scenario".to_owned(), Value::String(scenario.clone())),
                ("session".to_owned(), Value::String(session.clone())),
                ("state".to_owned(), Value::String(state.clone())),
                ("operation".to_owned(), Value::String(operation.clone())),
            ],
            Self::UnknownScenario { requested, .. } => {
                vec![("scenario".to_owned(), Value::String(requested.clone()))]
            }
            Self::ResourceNotFound { key } | Self::ResourceExists { key } => {
                vec![("resource".to_owned(), Value::String(key.clone()))]
            }
            Self::SessionCapExceeded { limit } | Self::StoreCapExceeded { limit } => {
                vec![("limit".to_owned(), Value::from(*limit))]
            }
        }
    }
}

/// What the handler should do with a governed request.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// No scenario governs this request. Behave exactly as before scenarios
    /// existed: an operation no scenario names is unmocked, not an error.
    Ungoverned,
    /// Serve this body instead of a generated one.
    Body(Value),
    /// Serve no body, for a delete.
    Empty,
    /// Serve this status and body, for a state-specific override.
    Forced {
        /// Status to report.
        status: u16,
        /// Body to report, if the override supplied one.
        body: Option<Value>,
    },
    /// Report a stateful failure.
    Failed(StatefulFailure),
}

/// One isolated run of a scenario.
#[derive(Debug)]
struct Instance {
    scenario: String,
    initial: State,
    state: Mutex<State>,
    store: Store,
    seed: Vec<SeedEntry>,
}

impl Instance {
    fn new(scenario: &str, initial: State, seed: Vec<SeedEntry>, store_limit: usize) -> Self {
        let instance = Self {
            scenario: scenario.to_owned(),
            initial: initial.clone(),
            state: Mutex::new(initial),
            store: Store::new(store_limit),
            seed,
        };
        instance.apply_seed();
        instance
    }

    fn apply_seed(&self) {
        for entry in &self.seed {
            let namespace = entry.resource.clone();
            for (index, item) in entry.items.iter().enumerate() {
                let key = StoreKey::new(namespace.clone(), seed_identifier(item, index));
                if self.store.create(&key, item.clone()).is_err() {
                    // Silently dropping a later item would hide a duplicate id,
                    // which is the author's mistake worth surfacing.
                    tracing::warn!(
                        resource = %namespace,
                        "seed item skipped: duplicate identifier within one namespace"
                    );
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Poisoning would mean a panic inside a transition, which is a bug rather
        // than a runtime condition; recovering keeps the mock serving.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A snapshot of the current state under one short lock.
    fn current(&self) -> State {
        self.lock().clone()
    }

    /// Move to `next`, under one short lock.
    fn move_to(&self, next: State, operation: &str, from: &State) {
        let mut guard = self.lock();
        *guard = next;
        let reached = guard.name().to_owned();
        drop(guard);
        tracing::info!(
            scenario = %self.scenario,
            from = %from.name(),
            to = %reached,
            operation = %operation,
            "scenario transition"
        );
    }

    fn reset(&self) {
        *self.lock() = self.initial.clone();
        self.store.clear();
        self.apply_seed();
    }
}

/// Derive a seed item's identifier, preferring an `id` field.
fn seed_identifier(item: &Value, index: usize) -> String {
    item.get("id")
        .map(|id| match id {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| index.to_string())
}

/// Scenario execution for one running server.
///
/// Held behind an `Arc` rather than cloned. `scc`'s `Clone` takes a *snapshot* of
/// the current entries into a fresh map, so a clone would be a disconnected copy
/// that never sees requests served through the original.
#[derive(Debug)]
pub struct ScenarioRuntime {
    sidecar: ValidatedSidecar,
    /// Catalogue keyed by request signature so lookup is a direct hit.
    by_request: HashMap<String, ResolvedOperation>,
    /// Live instances, keyed by `(scenario, session)`.
    ///
    /// The scenario is part of the key because an instance belongs to the flow
    /// it was created for. Keying by session alone would let a request naming a
    /// different scenario silently adopt the first one's state and store, which
    /// then reports a state from an unrelated flow.
    sessions: ConcurrentMap<(String, String), Arc<Instance>>,
    max_sessions: usize,
    max_store_entries: usize,
}

#[derive(Debug, Clone)]
struct ResolvedOperation {
    /// Catalogue name, which is the FSM's operation key.
    name: String,
    store: Option<StoreSpec>,
    responses: BTreeMap<String, ResponseOverride>,
}

impl ScenarioRuntime {
    /// Build a runtime from a validated sidecar.
    #[must_use]
    pub fn new(sidecar: ValidatedSidecar, max_sessions: usize, max_store_entries: usize) -> Self {
        let by_request = sidecar
            .sidecar()
            .operations
            .iter()
            .map(|(name, entry)| {
                (
                    request_key(&entry.matcher.method, &entry.matcher.path),
                    ResolvedOperation {
                        name: name.clone(),
                        store: entry.store.clone(),
                        responses: entry.responses.clone(),
                    },
                )
            })
            .collect();

        Self {
            sidecar,
            by_request,
            sessions: ConcurrentMap::new(),
            max_sessions,
            max_store_entries,
        }
    }

    /// Parse and build from a resolved sidecar document.
    pub fn from_value(
        value: &Value,
        max_sessions: usize,
        max_store_entries: usize,
    ) -> Result<Self, SidecarError> {
        Ok(Self::new(specmock_core::sidecar::parse(value)?, max_sessions, max_store_entries))
    }

    /// The scenario used when a request names none.
    #[must_use]
    pub fn default_scenario_name(&self) -> Option<&str> {
        self.sidecar.default_scenario().map(|scenario| scenario.name.as_str())
    }

    /// Declared scenario names, for diagnostics.
    #[must_use]
    pub fn scenario_names(&self) -> Vec<String> {
        self.sidecar.scenario_names().into_iter().map(str::to_owned).collect()
    }

    /// Number of live instances.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Drop one instance, used when a WebSocket connection closes.
    ///
    /// A connection is the natural scope for a WebSocket session, so its instance
    /// has to go when the connection does. Left behind, it would keep occupying
    /// the session cap for the life of the server.
    pub fn release(&self, scenario: &str, session: &str) -> bool {
        self.sessions.remove_sync(&(scenario.to_owned(), session.to_owned())).is_some()
    }

    /// Reset every live instance to its initial state and empty its store.
    ///
    /// State and store are reset together and are not separable: a caller able
    /// to clear one without the other would find "reset" meaning two things.
    pub fn reset_all(&self) -> usize {
        let mut count = 0;
        self.sessions.iter_sync(|_, instance| {
            instance.reset();
            count += 1;
            true
        });
        count
    }

    /// Current scenario and state for a session of the default scenario.
    ///
    /// Pass an empty string for the default instance.
    #[must_use]
    pub fn session_state(&self, session: &str) -> Option<(String, String)> {
        let scenario = self.default_scenario_name()?.to_owned();
        self.session_state_for(&scenario, session)
    }

    /// Current scenario and state for one instance.
    #[must_use]
    pub fn session_state_for(&self, scenario: &str, session: &str) -> Option<(String, String)> {
        let key = (
            scenario.to_owned(),
            if session.is_empty() { DEFAULT_SESSION.to_owned() } else { session.to_owned() },
        );
        let instance = self.sessions.get_sync(&key)?;
        let state = instance.current();
        Some((instance.scenario.clone(), state.name().to_owned()))
    }

    /// Live `(scenario, session)` pairs, for the control endpoint.
    #[must_use]
    pub fn sessions(&self) -> Vec<(String, String)> {
        let mut found = Vec::new();
        self.sessions.iter_sync(|(scenario, session), _| {
            found.push((scenario.clone(), session.clone()));
            true
        });
        found.sort();
        found
    }

    /// Resolve a request that arrived on an AsyncAPI channel.
    ///
    /// A channel is addressed as `channel <name>`, so the same catalogue shape
    /// covers all three protocols: `post /widgets` for HTTP, `unary /pkg.Svc/Method`
    /// for gRPC, `channel chat.send` for WebSocket.
    ///
    /// The payload is used as both the request body and the generated response,
    /// because a channel carries one message rather than a request and a separate
    /// response document.
    pub fn resolve_channel(
        &self,
        scenario: &str,
        session: &str,
        channel: &str,
        incoming: Option<&Value>,
        generated: &Value,
    ) -> Resolution {
        self.resolve(
            Some(scenario),
            Some(session),
            "channel",
            channel,
            &HashMap::new(),
            incoming,
            generated,
            false,
            true,
        )
    }

    /// Resolve a unary gRPC call.
    ///
    /// A method is addressed as `unary /package.Service/Method`, matching the
    /// catalogue shape the other adapters use.
    ///
    /// The store holds *response*-shaped messages here, not request-shaped ones.
    /// A gRPC request and its response are distinct protobuf types, so storing
    /// the request and handing it back as the response would be a type confusion.
    /// Storing the response the client will see is what makes "create, then get"
    /// return something coherent.
    ///
    /// `request` and `response` are the JSON projections of the decoded and the
    /// generated messages.
    pub fn resolve_grpc(
        &self,
        scenario: Option<&str>,
        session: Option<&str>,
        method_path: &str,
        request: &Value,
        response: &Value,
    ) -> Resolution {
        // gRPC has no `Prefer` convention of its own, but the directives arrive as
        // lowercase metadata, so the same names carry over.
        self.resolve_with(
            scenario,
            session,
            "unary",
            method_path,
            &HashMap::new(),
            Some(request),
            response,
            false,
            true,
            // Store what the client will see, not what it sent.
            CreateValue::Response,
        )
    }

    fn machine(&self, name: &str) -> Option<Scenario> {
        let entry = self.sidecar.scenario(name)?;
        Some(Scenario::new(
            entry.name.clone(),
            State::new(entry.initial_state.clone()),
            entry
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
                                    step.target.as_ref().map_or_else(Transition::stay, |target| {
                                        Transition::to(State::new(target.clone()))
                                    }),
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
        ))
    }

    /// Resolve a governed request.
    ///
    /// `validated` gates whether anything moves. The caller validates first: a
    /// rejected request must leave state and store untouched, because the client
    /// is the thing under test and a client bug must not poison every later
    /// assertion. A store `404`, by contrast, is a legitimate outcome of a valid
    /// request and does advance.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &self,
        scenario_name: Option<&str>,
        session_key: Option<&str>,
        method: &str,
        path_template: &str,
        path_params: &HashMap<String, String>,
        request_body: Option<&Value>,
        response_body: &Value,
        prefer_code: bool,
        validated: bool,
    ) -> Resolution {
        self.resolve_with(
            scenario_name,
            session_key,
            method,
            path_template,
            path_params,
            request_body,
            response_body,
            prefer_code,
            validated,
            CreateValue::FollowKeySource,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_with(
        &self,
        scenario_name: Option<&str>,
        session_key: Option<&str>,
        method: &str,
        path_template: &str,
        path_params: &HashMap<String, String>,
        request_body: Option<&Value>,
        response_body: &Value,
        prefer_code: bool,
        validated: bool,
        create_value: CreateValue,
    ) -> Resolution {
        // `Prefer: code=` bypasses the machine entirely (ADR 0001).
        if prefer_code {
            return Resolution::Ungoverned;
        }

        let name = scenario_name
            .or_else(|| self.default_scenario_name())
            .map(str::to_owned)
            .unwrap_or_default();
        let Some(machine) = self.machine(&name) else {
            return if scenario_name.is_some() {
                Resolution::Failed(StatefulFailure::UnknownScenario {
                    requested: scenario_name.unwrap_or_default().to_owned(),
                    available: self.scenario_names(),
                })
            } else {
                // No scenarios at all: behave exactly as before.
                Resolution::Ungoverned
            };
        };

        let Some(operation) = self.by_request.get(&request_key(method, path_template)) else {
            return Resolution::Ungoverned;
        };

        let session_name = session_key.unwrap_or(DEFAULT_SESSION).to_owned();
        let instance = match self.instance(&machine, &session_name) {
            Ok(instance) => instance,
            Err(failure) => return Resolution::Failed(failure),
        };

        let state = instance.current();
        if !machine.allows(&state, &operation.name) {
            return Resolution::Failed(StatefulFailure::IllegalOperation {
                scenario: machine.name().to_owned(),
                session: session_name,
                state: state.name().to_owned(),
                operation: operation.name.clone(),
            });
        }

        if !validated {
            return Resolution::Ungoverned;
        }

        let store_result = operation.store.as_ref().map(|spec| {
            apply_store(
                &instance.store,
                spec,
                path_params,
                request_body,
                response_body,
                create_value,
            )
        });

        let mut outcome = match store_result {
            Some(Err(failure)) => Resolution::Failed(failure),
            Some(Ok(outcome)) => outcome,
            None => Resolution::Ungoverned,
        };

        // A state-specific override pins the status for this operation. It is checked
        // after the store so an override cannot mask a store failure -- reporting
        // "500" for a resource that was never created would be a lie.
        //
        // An override with no body pins the status only. The catalogue routinely
        // declares `responses: {"200": {}}` purely to say which status an
        // operation returns, and that must not erase a body the store supplied.
        if !matches!(outcome, Resolution::Failed(_) | Resolution::Forced { .. }) {
            let state_override = self.step_responses(machine.name(), &state, &operation.name);
            let chosen = state_override.iter().next().or_else(|| operation.responses.iter().next());

            if let Some((declared_status, spec)) = chosen {
                let status = declared_status.parse::<u16>().unwrap_or(500);
                outcome = match outcome {
                    Resolution::Body(body) => {
                        Resolution::Forced { status, body: spec.body.clone().or(Some(body)) }
                    }
                    _ => Resolution::Forced { status, body: spec.body.clone() },
                };
            }
        }

        // A failed store operation is still a valid request, so the machine
        // advances either way; only request validation gates the transition.
        if let Some(next) = machine.next_state(&state, &operation.name) {
            instance.move_to(next, &operation.name, &state);
        }

        outcome
    }

    /// The response overrides a scenario declares for one operation in one state.
    ///
    /// This is how fault injection is expressed: a state that declares an error
    /// response is a state the flow can be driven into, rather than a separate
    /// mechanism bolted beside the state machine (ADR 0001).
    fn step_responses(
        &self,
        scenario: &str,
        state: &State,
        operation: &str,
    ) -> BTreeMap<String, ResponseOverride> {
        self.sidecar
            .scenario(scenario)
            .and_then(|entry| entry.states.get(state.name()))
            .and_then(|operations| operations.get(operation))
            .map(|step| step.responses.clone())
            .unwrap_or_default()
    }

    fn instance(
        &self,
        machine: &Scenario,
        session: &str,
    ) -> Result<Arc<Instance>, StatefulFailure> {
        let key = (machine.name().to_owned(), session.to_owned());
        if let Some(existing) = self.sessions.get_sync(&key) {
            return Ok(Arc::clone(&existing));
        }
        if self.sessions.len() >= self.max_sessions {
            return Err(StatefulFailure::SessionCapExceeded { limit: self.max_sessions });
        }

        let entry = self.sidecar.scenario(machine.name()).ok_or_else(|| {
            StatefulFailure::UnknownScenario {
                requested: machine.name().to_owned(),
                available: self.scenario_names(),
            }
        })?;
        let fresh = Arc::new(Instance::new(
            machine.name(),
            machine.initial_state().clone(),
            entry.seed.clone(),
            self.max_store_entries,
        ));

        // A concurrent request may have won the race. Adopt its instance rather
        // than replacing it, so both callers share one run.
        if let Some(existing) = self.sessions.get_sync(&key) {
            return Ok(Arc::clone(&existing));
        }
        match self.sessions.insert_sync(key, Arc::clone(&fresh)) {
            Ok(()) => Ok(fresh),
            Err((_, existing)) => Ok(existing),
        }
    }
}

/// `"method /path"`, the key the catalogue is indexed by.
///
/// The method is lower-cased on both sides so `POST` in a sidecar matches `post`
/// off the wire, and so a protocol whose "method" is really a kind -- `unary` for
/// gRPC, `channel` for a WebSocket channel -- reads naturally in a config file
/// instead of arriving shouted.
fn request_key(method: &str, path_template: &str) -> String {
    format!("{} {}", method.to_ascii_lowercase(), path_template)
}

/// Bind a collection path template's parameters, giving the store's namespace.
///
/// This is why `/users/5/orders` and `/orders` never collide on a shared
/// identifier, and why same-id-different-tenant stays distinct without having
/// to infer a type from the schema.
fn bind_namespace(template: &str, path_params: &HashMap<String, String>) -> String {
    let mut bound = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        bound.push_str(&rest[..open]);
        let Some(offset) = rest[open..].find('}') else {
            bound.push_str(&rest[open..]);
            return bound;
        };
        let parameter = &rest[open + 1..open + offset];
        match path_params.get(parameter) {
            Some(value) => bound.push_str(value),
            None => {
                bound.push('{');
                bound.push_str(parameter);
                bound.push('}');
            }
        }
        rest = &rest[open + offset + 1..];
    }
    bound.push_str(rest);
    bound
}

/// Resolve the identifier a keyed action addresses.
///
/// Two sources, because no single one covers every protocol: `key` names a path
/// parameter, which suits HTTP, and `keyFrom` names a field in the message,
/// which is the only option for a WebSocket channel or a gRPC method, where
/// nothing in the address identifies a resource.
fn resolve_key(
    spec: &StoreSpec,
    path_params: &HashMap<String, String>,
    request_body: Option<&Value>,
    response_body: &Value,
) -> Option<String> {
    if let Some(name) = &spec.key {
        let parameter =
            name.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')).unwrap_or(name);
        if let Some(bound) = path_params.get(parameter) {
            return Some(bound.clone());
        }
    }
    // A keyed action may still find its resource in the message it was sent.
    if spec.key_from.is_some() {
        return resolve_create_key(spec, request_body, response_body);
    }
    None
}

/// Resolve a create's identifier from the pointer it declares.
fn resolve_create_key(
    spec: &StoreSpec,
    request_body: Option<&Value>,
    response_body: &Value,
) -> Option<String> {
    let source = spec.key_from.as_ref()?;
    let subject = match source.location {
        KeyLocation::Request => request_body?,
        KeyLocation::Response => response_body,
    };
    pointer_value(subject, &source.pointer)
}

/// Write `identifier` into `value` at `pointer`, adding the field if absent.
///
/// A no-op for the empty pointer, which addresses the whole document and so has
/// nowhere to put a field.
fn align_identifier(value: &mut Value, pointer: &str, identifier: &str) {
    if pointer.is_empty() {
        return;
    }
    let segments: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    let Some((last, parents)) = segments.split_last() else {
        return;
    };

    let mut node = value;
    for segment in parents {
        if !node.is_object() {
            *node = Value::Object(serde_json::Map::new());
        }
        node = node
            .as_object_mut()
            .expect("just ensured an object")
            .entry((*segment).to_owned())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
    if !node.is_object() {
        *node = Value::Object(serde_json::Map::new());
    }
    node.as_object_mut()
        .expect("just ensured an object")
        .insert((*last).to_owned(), Value::String(identifier.to_owned()));
}

/// Follow a JSON pointer, returning `None` if any segment is absent.
fn pointer_value(subject: &Value, pointer: &str) -> Option<String> {
    let mut node = subject;
    if !pointer.is_empty() {
        for segment in pointer.trim_start_matches('/').split('/') {
            node = node.get(segment)?;
        }
    }
    Some(match node {
        Value::String(text) => text.clone(),
        Value::Null => return None,
        other => other.to_string(),
    })
}

/// What a `create` puts in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateValue {
    /// Whatever `keyFrom` points at, which is right for HTTP where a request and
    /// its response share a schema.
    FollowKeySource,
    /// Always the generated response.
    ///
    /// gRPC needs this: a request and its response are distinct protobuf types,
    /// so storing the request and returning it as the response would hand the
    /// client bytes of the wrong message. Storing the response is what makes
    /// "create, then get" return something coherent.
    Response,
}

fn apply_store(
    store: &Store,
    spec: &StoreSpec,
    path_params: &HashMap<String, String>,
    request_body: Option<&Value>,
    response_body: &Value,
    create_value: CreateValue,
) -> Result<Resolution, StatefulFailure> {
    let namespace = bind_namespace(&spec.resource, path_params);

    if spec.action() == StoreAction::List {
        return Ok(Resolution::Body(Value::Array(store.list(&namespace))));
    }

    if spec.action() == StoreAction::Create {
        let Some(id) = resolve_create_key(spec, request_body, response_body) else {
            // The declared pointer found nothing, so there is no identity to
            // store under. Refusing beats inventing a key.
            return Err(StatefulFailure::ResourceNotFound { key: namespace });
        };
        let key = StoreKey::new(namespace, id.clone());
        let from_request = create_value == CreateValue::FollowKeySource &&
            spec.key_from.as_ref().is_some_and(|source| source.location == KeyLocation::Request);

        // When the generated response is what gets stored, its identifier has to
        // agree with the key it is filed under. Otherwise a gRPC create files the
        // client's id alongside a faker id, and reading it back by the id the
        // client was just handed misses.
        let value = if from_request {
            request_body.cloned().unwrap_or(Value::Null)
        } else {
            let mut value = response_body.clone();
            if create_value == CreateValue::Response &&
                let Some(source) = spec.key_from.as_ref()
            {
                align_identifier(&mut value, &source.pointer, &id);
            }
            value
        };
        return store
            .create(&key, value.clone())
            .map(|()| Resolution::Body(value))
            .map_err(|error| map_store_error(error, &key));
    }

    let Some(id) = resolve_key(spec, path_params, request_body, response_body) else {
        return Err(StatefulFailure::ResourceNotFound { key: namespace });
    };
    let key = StoreKey::new(namespace, id);

    match spec.action() {
        StoreAction::Read => store
            .read(&key)
            .map(Resolution::Body)
            .ok_or_else(|| StatefulFailure::ResourceNotFound { key: key.to_string() }),
        StoreAction::Update => {
            let value = request_body.cloned().unwrap_or(Value::Null);
            store
                .update(&key, value.clone())
                .map(|()| Resolution::Body(value))
                .map_err(|error| map_store_error(error, &key))
        }
        StoreAction::Delete => store
            .delete(&key)
            .map(|()| Resolution::Empty)
            .map_err(|error| map_store_error(error, &key)),
        // `List` returned above and `Create` above; both are covered.
        StoreAction::Create | StoreAction::List => Ok(Resolution::Ungoverned),
    }
}

fn map_store_error(error: specmock_core::store::StoreError, key: &StoreKey) -> StatefulFailure {
    match error {
        specmock_core::store::StoreError::NotFound(_) => {
            StatefulFailure::ResourceNotFound { key: key.to_string() }
        }
        specmock_core::store::StoreError::AlreadyExists(_) => {
            StatefulFailure::ResourceExists { key: key.to_string() }
        }
        specmock_core::store::StoreError::CapacityExceeded { limit } => {
            StatefulFailure::StoreCapExceeded { limit }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sidecar_value() -> Value {
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

    fn runtime(value: &Value) -> ScenarioRuntime {
        ScenarioRuntime::from_value(value, 8, 32).unwrap_or_else(|e| panic!("sidecar: {e}"))
    }

    fn params(id: &str) -> HashMap<String, String> {
        HashMap::from([("id".to_owned(), id.to_owned())])
    }

    fn read(runtime: &ScenarioRuntime, id: &str) -> Resolution {
        runtime.resolve(
            None,
            None,
            "GET",
            "/users/{id}",
            &params(id),
            None,
            &json!({}),
            false,
            true,
        )
    }

    #[test]
    fn failure_statuses_follow_the_designed_mapping() {
        assert_eq!(
            StatefulFailure::UnknownScenario {
                requested: "nope".to_owned(),
                available: vec!["browse".to_owned()]
            }
            .status(),
            404
        );
        assert_eq!(
            StatefulFailure::IllegalOperation {
                scenario: "s".to_owned(),
                session: "x".to_owned(),
                state: "a".to_owned(),
                operation: "o".to_owned()
            }
            .status(),
            409
        );
        assert_eq!(StatefulFailure::ResourceNotFound { key: "/a".to_owned() }.status(), 404);
        assert_eq!(StatefulFailure::ResourceExists { key: "/a".to_owned() }.status(), 409);
        assert_eq!(StatefulFailure::SessionCapExceeded { limit: 1 }.status(), 429);
        assert_eq!(StatefulFailure::StoreCapExceeded { limit: 1 }.status(), 500);
    }

    #[test]
    fn an_unknown_scenario_names_the_ones_that_exist() {
        let detail = StatefulFailure::UnknownScenario {
            requested: "ghost".to_owned(),
            available: vec!["browse".to_owned(), "checkout".to_owned()],
        }
        .detail();
        assert!(detail.contains("ghost"));
        assert!(detail.contains("browse, checkout"));
    }

    #[test]
    fn an_illegal_operation_reports_its_scenario_context() {
        let extensions = StatefulFailure::IllegalOperation {
            scenario: "checkout".to_owned(),
            session: "t1".to_owned(),
            state: "cart".to_owned(),
            operation: "pay".to_owned(),
        }
        .extensions();
        let names: Vec<&str> = extensions.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["scenario", "session", "state", "operation"]);
    }

    #[test]
    fn a_seed_item_prefers_its_id_field() {
        assert_eq!(seed_identifier(&json!({"id": "u1"}), 0), "u1");
        assert_eq!(seed_identifier(&json!({"id": 7}), 0), "7");
        assert_eq!(seed_identifier(&json!({"name": "x"}), 3), "3", "fall back to position");
    }

    #[test]
    fn namespaces_bind_their_path_parameters() {
        let bound = bind_namespace("/users/{id}/orders", &params("5"));
        assert_eq!(bound, "/users/5/orders");
    }

    #[test]
    fn an_unbound_parameter_is_left_visible_rather_than_dropped() {
        assert_eq!(
            bind_namespace("/users/{id}", &HashMap::new()),
            "/users/{id}",
            "dropping it would collapse into a plausible-looking shorter path"
        );
    }

    #[test]
    fn an_operation_outside_the_catalogue_is_ungoverned() {
        assert_eq!(
            read(&runtime(&sidecar_value()), "1"),
            Resolution::Failed(StatefulFailure::ResourceNotFound { key: "/users/1".to_owned() }),
            "the read is governed and fails on the empty store"
        );
        assert_eq!(
            runtime(&sidecar_value()).resolve(
                None,
                None,
                "GET",
                "/pets/{id}",
                &params("1"),
                None,
                &json!({}),
                false,
                true
            ),
            Resolution::Ungoverned,
            "an operation no scenario names is unmocked, not an error"
        );
    }

    #[test]
    fn a_pre_seed_entry_is_readable_without_creating_it_first() {
        let mut value = sidecar_value();
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        assert_eq!(read(&runtime(&value), "u1"), Resolution::Body(json!({"id": "u1"})));
    }

    #[test]
    fn create_then_read_returns_the_created_resource() {
        let mut value = sidecar_value();
        value["operations"]["createUser"] = json!({
            "match": { "method": "POST", "path": "/users" },
            "store": {
                "resource": "/users",
                "action": "create",
                "keyFrom": { "in": "response", "pointer": "/id" }
            }
        });
        value["scenarios"][0]["states"]["anonymous"]["createUser"] = json!({});
        let runtime = runtime(&value);

        let created = json!({"id": "u1", "name": "Ada"});
        assert_eq!(
            runtime.resolve(
                None,
                None,
                "POST",
                "/users",
                &HashMap::new(),
                Some(&created),
                &created,
                false,
                true
            ),
            Resolution::Body(created.clone())
        );
        assert_eq!(
            read(&runtime, "u1"),
            Resolution::Body(created),
            "the created resource comes back"
        );
    }

    #[test]
    fn creating_the_same_identifier_twice_conflicts() {
        let mut value = sidecar_value();
        value["operations"]["createUser"] = json!({
            "match": { "method": "POST", "path": "/users" },
            "store": {
                "resource": "/users",
                "action": "create",
                "keyFrom": { "in": "response", "pointer": "/id" }
            }
        });
        value["scenarios"][0]["states"]["anonymous"]["createUser"] = json!({});
        let runtime = runtime(&value);
        let body = json!({"id": "u1"});

        let post = |runtime: &ScenarioRuntime| {
            runtime.resolve(
                None,
                None,
                "POST",
                "/users",
                &HashMap::new(),
                Some(&body),
                &body,
                false,
                true,
            )
        };
        post(&runtime);
        assert_eq!(
            post(&runtime),
            Resolution::Failed(StatefulFailure::ResourceExists { key: "/users/u1".to_owned() })
        );
    }

    #[test]
    fn delete_then_read_reports_not_found() {
        let mut value = sidecar_value();
        value["operations"]["deleteUser"] = json!({
            "match": { "method": "DELETE", "path": "/users/{id}" },
            "store": { "resource": "/users", "action": "delete", "key": "{id}" }
        });
        value["scenarios"][0]["states"]["anonymous"]["deleteUser"] = json!({});
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        assert_eq!(
            runtime.resolve(
                None,
                None,
                "DELETE",
                "/users/{id}",
                &params("u1"),
                None,
                &json!({}),
                false,
                true
            ),
            Resolution::Empty
        );
        assert_eq!(
            read(&runtime, "u1"),
            Resolution::Failed(StatefulFailure::ResourceNotFound { key: "/users/u1".to_owned() }),
            "a deleted resource stays deleted"
        );
    }

    #[test]
    fn list_returns_creation_order_and_an_empty_store_returns_an_empty_array() {
        let mut value = sidecar_value();
        value["operations"]["createUser"] = json!({
            "match": { "method": "POST", "path": "/users" },
            "store": {
                "resource": "/users",
                "action": "create",
                "keyFrom": { "in": "response", "pointer": "/id" }
            }
        });
        value["operations"]["listUsers"] = json!({
            "match": { "method": "GET", "path": "/users" },
            "store": { "resource": "/users", "action": "list" }
        });
        value["scenarios"][0]["states"]["anonymous"]["createUser"] = json!({});
        value["scenarios"][0]["states"]["anonymous"]["listUsers"] = json!({});
        let runtime = runtime(&value);

        let list = |runtime: &ScenarioRuntime| {
            runtime.resolve(
                None,
                None,
                "GET",
                "/users",
                &HashMap::new(),
                None,
                &json!({}),
                false,
                true,
            )
        };
        assert_eq!(list(&runtime), Resolution::Body(json!([])), "an empty store lists nothing");

        for id in ["c", "a", "b"] {
            let body = json!({ "id": id });
            runtime.resolve(
                None,
                None,
                "POST",
                "/users",
                &HashMap::new(),
                Some(&body),
                &body,
                false,
                true,
            );
        }
        assert_eq!(
            list(&runtime),
            Resolution::Body(json!([{"id": "c"}, {"id": "a"}, {"id": "b"}])),
            "list follows creation, not sort order"
        );
    }

    #[test]
    fn sessions_are_isolated() {
        let mut value = sidecar_value();
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        let in_session = |session: &str| {
            runtime.resolve(
                None,
                Some(session),
                "GET",
                "/users/{id}",
                &params("u1"),
                None,
                &json!({}),
                false,
                true,
            )
        };
        assert_eq!(in_session("t1"), Resolution::Body(json!({"id": "u1"})));
        assert_eq!(in_session("t2"), Resolution::Body(json!({"id": "u1"})));
        assert_eq!(runtime.session_count(), 2);
    }

    #[test]
    fn exceeding_the_session_cap_is_reported_not_evicted() {
        let runtime = ScenarioRuntime::from_value(&sidecar_value(), 1, 32).expect("sidecar");

        // The first request creates the only permitted session.
        let _ = runtime.resolve(
            None,
            Some("t1"),
            "GET",
            "/users/{id}",
            &params("1"),
            None,
            &json!({}),
            false,
            true,
        );
        assert_eq!(runtime.session_count(), 1);

        assert_eq!(
            runtime.resolve(
                None,
                Some("t2"),
                "GET",
                "/users/{id}",
                &params("1"),
                None,
                &json!({}),
                false,
                true
            ),
            Resolution::Failed(StatefulFailure::SessionCapExceeded { limit: 1 }),
            "a new session key must be refused, not recycled from the existing one"
        );
        assert_eq!(runtime.session_count(), 1, "the existing session is untouched");
    }

    #[test]
    fn a_state_specific_response_forces_its_status() {
        let mut value = sidecar_value();
        value["scenarios"][0]["states"] = json!({
            "anonymous": { "readUser": { "to": "broken", "responses": { "500": { "body": { "down": true } } } } },
            "broken": {}
        });
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        assert_eq!(
            read(&runtime, "u1"),
            Resolution::Forced { status: 500, body: Some(json!({"down": true})) },
            "an error is a transition into a state that declares an error"
        );
    }

    #[test]
    fn prefer_code_overrides_a_state_specific_status() {
        let mut value = sidecar_value();
        value["scenarios"][0]["states"] = json!({
            "anonymous": { "readUser": { "responses": { "500": {} } } }
        });
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        assert_eq!(
            runtime.resolve(
                None,
                None,
                "GET",
                "/users/{id}",
                &params("u1"),
                None,
                &json!({}),
                true,
                true
            ),
            Resolution::Ungoverned,
            "the explicit header wins over the state's declared status"
        );
    }

    #[test]
    fn reset_returns_instances_to_their_initial_state_and_empties_their_stores() {
        let mut value = sidecar_value();
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);
        read(&runtime, "u1");

        assert_eq!(runtime.reset_all(), 1);
        let (_, state) = runtime.session_state("").expect("instance survives reset");
        assert_eq!(state, "anonymous");
    }

    #[test]
    fn an_invalid_request_leaves_the_store_untouched() {
        let mut value = sidecar_value();
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        // `validated: false` must not consume or advance anything.
        assert_eq!(
            runtime.resolve(
                None,
                None,
                "GET",
                "/users/{id}",
                &params("u1"),
                None,
                &json!({}),
                false,
                false
            ),
            Resolution::Ungoverned
        );
        assert_eq!(read(&runtime, "u1"), Resolution::Body(json!({"id": "u1"})));
    }

    #[test]
    fn an_empty_response_override_pins_the_status_without_erasing_a_stored_body() {
        // `responses: {"200": {}}` says which status an operation returns. It
        // must not replace the body the store supplied.
        let mut value = sidecar_value();
        value["operations"]["readUser"]["responses"] = json!({ "200": {} });
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1", "name": "Ada" }] }]);
        let runtime = runtime(&value);

        assert_eq!(
            read(&runtime, "u1"),
            Resolution::Forced { status: 200, body: Some(json!({"id": "u1", "name": "Ada"})) },
            "the stored resource must survive an empty override"
        );
    }

    #[test]
    fn a_body_bearing_override_replaces_the_stored_body() {
        let mut value = sidecar_value();
        value["scenarios"][0]["states"] = json!({
            "anonymous": { "readUser": { "responses": { "503": { "body": { "error": "down" } } } } }
        });
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        assert_eq!(
            read(&runtime, "u1"),
            Resolution::Forced { status: 503, body: Some(json!({"error": "down"})) }
        );
    }

    #[test]
    fn prefer_code_bypasses_the_machine_without_advancing_state() {
        let mut value = sidecar_value();
        value["scenarios"][0]["states"] = json!({
            "anonymous": { "readUser": { "to": "seen" } },
            "seen": {}
        });
        let runtime = runtime(&value);

        assert_eq!(
            runtime.resolve(
                None,
                None,
                "GET",
                "/users/{id}",
                &params("u1"),
                None,
                &json!({}),
                true,
                true
            ),
            Resolution::Ungoverned,
            "`Prefer: code=` short-circuits before the machine"
        );
        assert_eq!(runtime.session_count(), 0, "a bypassed request creates no instance");
    }

    #[test]
    fn a_transition_moves_the_instance_to_the_target_state() {
        let mut value = sidecar_value();
        value["scenarios"][0]["states"] = json!({
            "anonymous": { "readUser": { "to": "seen" } },
            "seen": { "readUser": {} }
        });
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        assert_eq!(read(&runtime, "u1"), Resolution::Body(json!({"id": "u1"})));
        let (_, state) = runtime.session_state("").expect("instance");
        assert_eq!(state, "seen", "a declared target moves the machine");
    }

    #[test]
    fn an_operation_illegal_in_the_current_state_is_refused() {
        let mut value = sidecar_value();
        value["scenarios"][0]["states"] = json!({
            "anonymous": { "readUser": { "to": "seen" } },
            "seen": {}
        });
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users", "items": [{ "id": "u1" }] }]);
        let runtime = runtime(&value);

        read(&runtime, "u1");
        assert_eq!(
            read(&runtime, "u1"),
            Resolution::Failed(StatefulFailure::IllegalOperation {
                scenario: "browse".to_owned(),
                session: DEFAULT_SESSION.to_owned(),
                state: "seen".to_owned(),
                operation: "readUser".to_owned(),
            }),
            "the second read is not legal once the machine has moved on"
        );
    }

    #[test]
    fn an_unknown_scenario_name_is_reported() {
        let runtime = runtime(&sidecar_value());
        assert_eq!(
            runtime.resolve(
                Some("ghost"),
                None,
                "GET",
                "/users/{id}",
                &params("1"),
                None,
                &json!({}),
                false,
                true
            ),
            Resolution::Failed(StatefulFailure::UnknownScenario {
                requested: "ghost".to_owned(),
                available: vec!["browse".to_owned()],
            })
        );
    }

    #[test]
    fn nested_collections_do_not_leak_into_each_other() {
        let mut value = sidecar_value();
        value["operations"]["readOrder"] = json!({
            "match": { "method": "GET", "path": "/users/{id}/orders/{orderId}" },
            "store": { "resource": "/users/{id}/orders", "action": "read", "key": "{orderId}" }
        });
        value["scenarios"][0]["states"]["anonymous"]["readOrder"] = json!({});
        value["scenarios"][0]["seed"] =
            json!([{ "resource": "/users/5/orders", "items": [{ "id": "1" }] }]);
        let runtime = runtime(&value);

        let mut bound = params("5");
        bound.insert("orderId".to_owned(), "1".to_owned());
        assert_eq!(
            runtime.resolve(
                None,
                None,
                "GET",
                "/users/{id}/orders/{orderId}",
                &bound,
                None,
                &json!({}),
                false,
                true
            ),
            Resolution::Body(json!({"id": "1"}))
        );
        assert_eq!(
            read(&runtime, "1"),
            Resolution::Failed(StatefulFailure::ResourceNotFound { key: "/users/1".to_owned() }),
            "the order is not visible at the top-level users namespace"
        );
    }
}
