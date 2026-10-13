use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::control_session::{Failure, Operation};
use super::super::instances::Identity;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    schema_version: u16,
    pub id: String,
    pub operation: Operation,
}

impl Envelope {
    pub(super) fn parse(bytes: Vec<u8>) -> Result<Self, Failure> {
        let request: Self = serde_json::from_slice(&bytes)
            .map_err(|error| Failure::new("invalid-request", error.to_string()))?;
        if request.schema_version != 1 {
            return Err(Failure::new(
                "unsupported-version",
                "only schema_version 1 is supported",
            ));
        }
        if request.id.is_empty()
            || request.id.len() > 128
            || request.id.chars().any(char::is_control)
        {
            return Err(Failure::new(
                "invalid-request",
                "id must be 1-128 bytes without control characters",
            ));
        }
        Ok(request)
    }
}

#[derive(Serialize)]
pub(super) struct Reply {
    schema_version: u16,
    pub id: Option<String>,
    instance_id: String,
    run_id: String,
    /// Protocol success only. Callers must inspect operation-specific outcomes.
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Failure>,
}

impl Reply {
    pub(super) fn new(
        id: Option<String>,
        identity: &Identity,
        result: Result<Value, Failure>,
    ) -> Self {
        let (data, error) = match result {
            Ok(data) => (Some(data), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            schema_version: 1,
            id,
            instance_id: identity.instance_id.clone(),
            run_id: identity.run_id.clone(),
            ok: error.is_none(),
            data,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_agent_envelopes_reject_ambiguous_or_elevated_requests() {
        let valid = json!({"schema_version": 1, "id": "test", "operation": {"method": "status"}});
        assert!(Envelope::parse(serde_json::to_vec(&valid).unwrap()).is_ok());
        for operation in [
            json!({"method": "status", "allow": ["stop"]}),
            json!({"method": "stop"}),
            json!({"method": "stop", "target": {"instance_id": "i", "run_id": "r", "all": true}}),
            json!({"method": "start"}),
        ] {
            let request = json!({"schema_version": 1, "id": "test", "operation": operation});
            assert!(
                Envelope::parse(serde_json::to_vec(&request).unwrap()).is_err(),
                "{request}"
            );
        }
        for id in ["".to_string(), "x".repeat(129), "line\nbreak".to_string()] {
            let mut request = valid.clone();
            request["id"] = id.into();
            assert!(Envelope::parse(serde_json::to_vec(&request).unwrap()).is_err());
        }
        assert!(Envelope::parse(
            br#"{"schema_version":1,"id":"a","id":"b","operation":{"method":"status"}}"#.to_vec()
        )
        .is_err());
    }
}
