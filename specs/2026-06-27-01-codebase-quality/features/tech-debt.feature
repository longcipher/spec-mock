@tech-debt
Feature: Tech debt cleanup

  @finding-13 @priority-low
  Scenario: Hash function is shared across HTTP, gRPC, and WebSocket runtimes
    Given the deterministic hash for seed derivation
    When HTTP, gRPC, and WebSocket modules need path hashing
    Then they all call a single shared hash function
    And the constants are consistent

  @finding-14 @priority-low
  Scenario: JSON Pointer resolution has a single implementation
    Given the need to resolve RFC 6901 JSON Pointers
    When ref_resolver and openapi modules need pointer resolution
    Then they both use the same function from specmock-core

  @finding-15 @priority-low
  Scenario: Dead code enums are removed
    Given the Protocol and ValidationDirection enums in contract.rs
    When no code references them outside re-exports
    Then they are removed from the codebase
    And their re-exports from lib.rs are removed

  @finding-16 @priority-low
  Scenario: ResolvedDocument wrapper is eliminated
    Given the ResolvedDocument newtype in ref_resolver.rs
    When it has no methods or invariants
    Then resolve() returns Value directly
    And ResolvedDocument is deleted
