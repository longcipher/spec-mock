//! Named stateful flows.
//!
//! A scenario is a finite state machine: exactly one state is current, and a
//! matched operation may move to another. The structure is deliberately
//! *state-centric* rather than a flat list of per-operation rules, because an
//! operation that is illegal in the current state should be unrepresentable
//! rather than merely absent. A rule list permits two rules to compete for one
//! state and lets load order decide the winner.
//!
//! This module holds no protocol types. HTTP wires it up; WebSocket and gRPC
//! could adopt the same types unchanged.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// A named position within a scenario.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct State(String);

impl State {
    /// Name a state.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The state's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The move an operation makes from one state to another.
///
/// An absent `target` means the operation is *legal here without moving* -- a
/// resting state, not a self-loop and not an error. Requiring an explicit
/// self-loop to fire twice keeps "this happens repeatedly" a stated fact rather
/// than an accident of where the arrow happens to point.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Transition {
    /// State to move to, or `None` to stay put.
    pub target: Option<State>,
}

impl Transition {
    /// Build a transition that moves to `target`.
    #[must_use]
    pub fn to(target: State) -> Self {
        Self { target: Some(target) }
    }

    /// Build a transition that stays in the current state.
    #[must_use]
    pub fn stay() -> Self {
        Self { target: None }
    }
}

/// A named flow: a starting state, the states it can occupy, and the operations
/// legal in each together with where each leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    name: String,
    initial: State,
    /// State to the operations legal there, mapped to the transition each makes.
    states: BTreeMap<State, BTreeMap<String, Transition>>,
}

impl Scenario {
    /// Build a scenario from its initial state and per-state transitions.
    ///
    /// Operations are referenced by name from the sidecar's operation
    /// catalogue, keyed on method plus path template rather than `operationId`
    /// because `operationId` is optional in OpenAPI.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        initial: State,
        states: BTreeMap<State, BTreeMap<String, Transition>>,
    ) -> Self {
        Self { name: name.into(), initial, states }
    }

    /// The scenario's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The state a fresh instance starts in.
    #[must_use]
    pub fn initial_state(&self) -> &State {
        &self.initial
    }

    /// Whether `operation` may fire while in `state`.
    #[must_use]
    pub fn allows(&self, state: &State, operation: &str) -> bool {
        self.states.get(state).is_some_and(|operations| operations.contains_key(operation))
    }

    /// The transition `operation` makes from `state`, if it is legal there.
    #[must_use]
    pub fn transition(&self, state: &State, operation: &str) -> Option<&Transition> {
        self.states.get(state).and_then(|operations| operations.get(operation))
    }

    /// The state `operation` leads to from `state`, if it moves at all.
    #[must_use]
    pub fn next_state(&self, state: &State, operation: &str) -> Option<State> {
        self.transition(state, operation)?.target.clone()
    }

    /// Every declared state name.
    #[must_use]
    pub fn state_names(&self) -> Vec<&str> {
        self.states.keys().map(State::name).collect()
    }

    /// Every operation declared in any state, in sorted order.
    #[must_use]
    pub fn operations(&self) -> Vec<&str> {
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for operations in self.states.values() {
            for name in operations.keys() {
                names.insert(name.as_str());
            }
        }
        names.into_iter().collect()
    }

    /// States no transition can reach from the initial state.
    ///
    /// These are dead configuration rather than a failure, so callers report
    /// them as warnings. Every declared operation is followed; those that stay
    /// put simply lead nowhere new.
    #[must_use]
    pub fn unreachable_states(&self) -> Vec<State> {
        let mut reachable: BTreeSet<State> = BTreeSet::new();
        let mut frontier = vec![self.initial.clone()];

        while let Some(state) = frontier.pop() {
            if !reachable.insert(state.clone()) {
                continue;
            }
            let Some(operations) = self.states.get(&state) else {
                continue;
            };
            for transition in operations.values() {
                if let Some(next) = &transition.target {
                    frontier.push(next.clone());
                }
            }
        }

        self.states.keys().filter(|state| !reachable.contains(state)).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(state, [(operation, target)])` where a `None` target means "stay put".
    type Spec = (&'static str, &'static [(&'static str, Option<&'static str>)]);

    fn states(pairs: &[Spec]) -> BTreeMap<State, BTreeMap<String, Transition>> {
        pairs
            .iter()
            .map(|(state, operations)| {
                let transitions = operations
                    .iter()
                    .map(|(operation, target)| {
                        (
                            (*operation).to_owned(),
                            match target {
                                Some(next) => Transition::to(State::new(*next)),
                                None => Transition::stay(),
                            },
                        )
                    })
                    .collect();
                (State::new(*state), transitions)
            })
            .collect()
    }

    fn checkout() -> Scenario {
        Scenario::new(
            "checkout",
            State::new("cart"),
            states(&[
                ("cart", &[("addItem", Some("cart")), ("pay", Some("ordered"))]),
                ("ordered", &[("pay", None), ("cancel", Some("cart"))]),
            ]),
        )
    }

    #[test]
    fn an_operation_is_legal_only_where_declared() {
        let scenario = checkout();
        assert!(scenario.allows(&State::new("cart"), "addItem"));
        assert!(scenario.allows(&State::new("cart"), "pay"));
        assert!(!scenario.allows(&State::new("ordered"), "addItem"), "addItem belongs to `cart`");
    }

    #[test]
    fn an_unknown_state_allows_nothing() {
        assert!(!checkout().allows(&State::new("nowhere"), "addItem"));
    }

    #[test]
    fn a_declared_target_is_the_next_state() {
        let scenario = checkout();
        assert_eq!(scenario.next_state(&State::new("cart"), "pay"), Some(State::new("ordered")));
    }

    #[test]
    fn an_omitted_target_means_stay_without_moving() {
        let scenario = checkout();
        assert_eq!(scenario.next_state(&State::new("ordered"), "pay"), None);
        assert!(scenario.transition(&State::new("ordered"), "pay").is_some());
    }

    #[test]
    fn an_illegal_operation_has_no_transition() {
        assert!(checkout().transition(&State::new("ordered"), "addItem").is_none());
    }

    #[test]
    fn state_names_and_operations_are_reported_in_sorted_order() {
        let scenario = checkout();
        assert_eq!(scenario.state_names(), ["cart", "ordered"]);
        assert_eq!(scenario.operations(), ["addItem", "cancel", "pay"]);
    }

    #[test]
    fn unreachable_states_are_reported_but_not_fatal() {
        let scenario = Scenario::new(
            "s",
            State::new("a"),
            states(&[("a", &[("go", Some("b"))]), ("b", &[("back", Some("a"))]), ("orphan", &[])]),
        );
        assert_eq!(
            scenario.unreachable_states(),
            [State::new("orphan")],
            "b is reachable via `go`, orphan is not"
        );
    }

    #[test]
    fn a_scenario_whose_states_are_all_reachable_reports_none() {
        assert!(checkout().unreachable_states().is_empty());
    }

    #[test]
    fn a_cyclic_scenario_terminates_the_reachability_walk() {
        let scenario = Scenario::new(
            "s",
            State::new("a"),
            states(&[("a", &[("to_b", Some("b"))]), ("b", &[("to_a", Some("a"))])]),
        );
        assert!(scenario.unreachable_states().is_empty());
    }

    #[test]
    fn an_initial_state_missing_from_the_map_leaves_everything_unreachable() {
        let scenario = Scenario::new("s", State::new("absent"), states(&[("present", &[])]));
        assert_eq!(scenario.unreachable_states(), [State::new("present")]);
    }
}
