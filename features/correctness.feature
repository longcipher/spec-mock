@correctness
Feature: Correctness and bug fixes

  Background:
    Given a running spec-mock server

  @finding-1 @priority-high
  Scenario: Recursive protobuf message does not crash the server
    Given a protobuf spec with a recursive message type (e.g., Tree with left/right children)
    When a gRPC request targets the recursive message
    Then the server returns a mock response without panicking
    And the response depth is bounded

  @finding-4 @priority-medium
  Scenario: Faker selects varied enum values based on seed
    Given an OpenAPI schema with "enum": ["A", "B", "C"]
    When the faker generates a value with seed 1
    And the faker generates a value with seed 2
    Then the generated values may differ based on seed
    And each generated value is one of the enum members

  @finding-5 @priority-medium
  Scenario: Faker respects minItems and minLength constraints
    Given an OpenAPI schema with "minItems": 10 for an array
    When the faker generates a value
    Then the generated array has at least 10 items
    Given an OpenAPI schema with "minLength": 200
    When the faker generates a string
    Then the generated string has at least 200 characters

  @finding-6 @priority-low
  Scenario: Faker handles integer minimum near i64::MAX without overflow
    Given an OpenAPI schema with "minimum" near i64::MAX
    When the faker generates an integer value
    Then the generation succeeds without panicking
    And the value is within the valid range

  @finding-7 @priority-medium
  Scenario: OpenAPI parser rejects non-JSON content types gracefully
    Given an OpenAPI operation that declares only "application/xml" content
    When the runtime processes a request to that operation
    Then the runtime returns 415 Unsupported Media Type or uses no schema
    And does not apply the XML schema as if it were JSON

  @finding-8 @priority-low
  Scenario: Prefer header with non-existent status code returns error
    Given an OpenAPI spec with responses for 200 and 500 only
    When a request includes "Prefer: code=404"
    Then the server returns 404 with a problem+json error
    And does not silently fall back to 200

  @finding-11 @priority-medium
  Scenario: Callback URL resolver handles non-body runtime expressions
    Given an OpenAPI callback with expression "{$request.body#/url}/{$method}"
    When the callback is triggered
    Then the URL is resolved or skipped gracefully
    And does not silently fail for the entire callback

  @finding-12 @priority-low
  Scenario: Named examples selection is deterministic
    Given an OpenAPI response with multiple named examples
    When the server is started twice with the same seed
    Then the same example is selected both times

  @finding-17 @priority-medium
  Scenario: SDK ws_url reflects configured WebSocket path
    Given a mock server configured with --ws-path /socket
    When the SDK returns the WebSocket URL
    Then the URL ends with /socket not /ws
