@security
Feature: Security hardening

  Background:
    Given a running spec-mock server in proxy mode

  @finding-2 @priority-high
  Scenario: Proxy mode rejects non-HTTP upstream URLs
    Given the server is started with --upstream file:///etc/passwd
    When the server validates configuration
    Then the server returns an error rejecting the upstream URL scheme

  @finding-2 @priority-high
  Scenario: Proxy mode rejects localhost/private upstream by default
    Given the server is started with --upstream http://169.254.169.254/
    When a request is proxied
    Then the server returns 502 or rejects the upstream at config time

  @finding-3 @priority-high
  Scenario: Callback URL is validated before firing
    Given an OpenAPI spec with a callback whose URL comes from request body
    When a request contains a callbackUrl pointing to an internal IP
    Then the callback is not fired
    And a warning is logged

  @finding-9 @priority-medium
  Scenario: Proxy strips sensitive headers from forwarded request
    Given the server is in proxy mode
    When a request includes Authorization and Cookie headers
    Then those headers are not forwarded to the upstream
    Or forwarding requires an explicit --forward-headers flag

  @finding-10 @priority-low
  Scenario: Error responses do not leak internal file paths
    Given a spec file that fails to load
    When the server returns an error response
    Then the response body does not contain absolute filesystem paths
    And the error message is sanitized
