@dx
Feature: Developer experience improvements

  @finding-20 @priority-medium
  Scenario: AGENTS.md matches actual project state
    Given the AGENTS.md file in the repository root
    When an agent reads it for development guidance
    Then the referenced just commands exist in Justfile
    And the preferred dependency versions match Cargo.toml
    And only actually-used dependencies are mentioned

  @finding-22 @priority-medium
  Scenario: BDD feature files exist for core behaviors
    Given the features/ directory
    When an agent looks for acceptance criteria
    Then Gherkin feature files cover core behaviors
    And the BDD test runner can execute them
