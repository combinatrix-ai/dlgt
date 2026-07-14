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
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl Response {
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            id: id.into(),
            result: Some(result),
            error: None,
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
            }),
        }
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
    fn request_defaults_params() {
        let request: Request = serde_json::from_value(json!({"id":"1","method":"server.ping"}))
            .unwrap_or_else(|error| panic!("failed to decode request: {error}"));
        assert_eq!(request.params, json!(null));
    }
}
