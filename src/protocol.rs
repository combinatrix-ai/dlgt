use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
}

impl Response {
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            id: id.into(),
            result: Some(result),
            error: None,
            info: None,
        }
    }

    pub fn error(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(RpcError {
                code: code.into(),
                message: message.into(),
                session_id: None,
                provider_session_id: None,
            }),
            info: None,
        }
    }

    pub fn session_error(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        session_id: impl Into<String>,
        provider_session_id: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(RpcError {
                code: code.into(),
                message: message.into(),
                session_id: Some(session_id.into()),
                provider_session_id,
            }),
            info: None,
        }
    }

    pub fn with_info(mut self, info: Option<Value>) -> Self {
        if self.error.is_none() {
            self.info = info;
        }
        self
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionRecord {
    pub id: String,
    pub alias: String,
    pub title: String,
    pub agent: String,
    pub cwd: String,
    pub state: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub harness_options: Vec<String>,
    pub auto_approve: bool,
    pub provider_session_id: Option<String>,
    pub active_turn_id: Option<String>,
    pub pid: Option<u32>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnRecord {
    pub id: String,
    pub session_id: String,
    pub execution_seq: i64,
    pub prompt: String,
    pub state: String,
    pub provider_turn_id: Option<String>,
    pub final_message: Option<String>,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventRecord {
    pub seq: i64,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub kind: String,
    pub payload: Value,
    pub created_at_ms: i64,
}

#[cfg(test)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputRecord {
    pub seq: i64,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub source: String,
    pub data_base64: String,
    pub display: String,
    pub byte_len: usize,
    pub created_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::{Request, Response};
    use serde_json::json;

    #[test]
    fn response_omits_empty_error() {
        let encoded = serde_json::to_value(Response::ok("req_1", json!({"ok": true})))
            .unwrap_or_else(|error| panic!("failed to encode response: {error}"));
        assert_eq!(encoded, json!({"id":"req_1","result":{"ok":true}}));
    }

    #[test]
    fn successful_response_can_include_info() {
        let response = Response::ok("req_1", json!([])).with_info(Some(json!({
            "code": "UPDATE_AVAILABLE",
            "latest_version": "0.2.0"
        })));
        let value = serde_json::to_value(response)
            .unwrap_or_else(|error| panic!("failed to serialize response: {error}"));
        assert_eq!(value["info"]["code"], "UPDATE_AVAILABLE");
        assert!(value.get("error").is_none());
    }

    #[test]
    fn session_error_includes_correlation_ids() {
        let encoded = serde_json::to_value(Response::session_error(
            "req_1",
            "LAUNCH_FAILED",
            "launch failed",
            "ses_1",
            Some("provider_1".to_owned()),
        ))
        .unwrap_or_else(|error| panic!("failed to encode response: {error}"));
        assert_eq!(
            encoded,
            json!({
                "id":"req_1",
                "error":{
                    "code":"LAUNCH_FAILED",
                    "message":"launch failed",
                    "session_id":"ses_1",
                    "provider_session_id":"provider_1"
                }
            })
        );
    }

    #[test]
    fn request_defaults_params() {
        let request: Request = serde_json::from_value(json!({"id":"1","method":"server.ping"}))
            .unwrap_or_else(|error| panic!("failed to decode request: {error}"));
        assert_eq!(request.params, json!(null));
    }
}
