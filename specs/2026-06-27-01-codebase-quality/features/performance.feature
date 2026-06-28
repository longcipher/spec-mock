@performance
Feature: Performance improvements

  @finding-18 @priority-low
  Scenario: Schema normalization does not clone entire tree
    Given a large OpenAPI spec with deeply nested schemas
    When the spec is loaded at startup
    Then normalization mutates in-place without cloning subtrees
    And peak memory usage during load is reduced

  @finding-21 @priority-medium
  Scenario: Validator cache uses hashed keys instead of full JSON serialization
    Given a schema used for request validation
    When multiple requests arrive with the same schema
    Then the cache key is a hash, not a full JSON string
    And validation latency is not dominated by key serialization
