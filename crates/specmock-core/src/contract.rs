//! Shared contract-level enums and metadata used across protocol runtimes.

use serde::{Deserialize, Serialize};

/// Runtime mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MockMode {
    /// Return mocked responses.
    #[default]
    Mock,
    /// Forward to upstream and validate upstream responses.
    Proxy,
}

#[cfg(test)]
mod tests {
    use super::MockMode;

    #[test]
    fn default_mode_is_mock() {
        assert_eq!(MockMode::default(), MockMode::Mock, "the runtime must default to mock mode");
    }

    #[test]
    fn serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&MockMode::Mock).unwrap_or_default(), "\"mock\"");
        assert_eq!(serde_json::to_string(&MockMode::Proxy).unwrap_or_default(), "\"proxy\"");
    }

    #[test]
    fn deserializes_from_snake_case() {
        let parsed: MockMode = serde_json::from_str("\"proxy\"").expect("proxy should deserialize");
        assert_eq!(parsed, MockMode::Proxy);

        let parsed: MockMode = serde_json::from_str("\"mock\"").expect("mock should deserialize");
        assert_eq!(parsed, MockMode::Mock);
    }

    #[test]
    fn rejects_unknown_mode_name() {
        let result: Result<MockMode, _> = serde_json::from_str("\"record\"");
        assert!(result.is_err(), "an unknown mode name must not deserialize");
    }

    #[test]
    fn roundtrips_through_json() {
        for mode in [MockMode::Mock, MockMode::Proxy] {
            let encoded = serde_json::to_string(&mode).expect("serialize");
            let decoded: MockMode = serde_json::from_str(&encoded).expect("deserialize");
            assert_eq!(decoded, mode);
        }
    }

    #[test]
    fn modes_are_distinct() {
        assert_ne!(MockMode::Mock, MockMode::Proxy);
    }
}
