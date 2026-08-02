use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    Idle,
    Busy,
    Quiescing,
    Stopping,
    Stopped,
    Failed,
    Restarting,
    Blocked,
}

impl SessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Quiescing => "quiescing",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Restarting => "restarting",
            Self::Blocked => "blocked",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }

    pub const fn public_name(self) -> &'static str {
        match self {
            Self::Running => "starting",
            Self::Quiescing => "canceling",
            _ => self.as_str(),
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Submitted,
    Running,
    Completed,
    Failed,
    Canceled,
    Interrupted,
}

impl TurnState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }

    pub const fn is_active(self) -> bool {
        matches!(self, Self::Submitted | Self::Running)
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Interrupted
        )
    }

    pub const fn is_provider_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

impl std::fmt::Display for TurnState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

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
    pub launch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
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
                launch_id: None,
                correlation_id: None,
                hint: None,
                session_state: None,
                action: None,
            }),
            info: None,
        }
    }

    pub fn session_error(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        session_id: Option<String>,
        launch_id: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(RpcError {
                code: code.into(),
                message: message.into(),
                session_id,
                launch_id,
                correlation_id: None,
                hint: None,
                session_state: None,
                action: None,
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
    pub state: SessionState,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub harness_options: Vec<String>,
    pub auto_approve: bool,
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
    pub state: TurnState,
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
    /// The only event payload retained: retry attempts are exposed by the
    /// public event stream for provider retry notifications.
    pub retry_attempt: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{Request, Response, SessionState, TurnState};
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
            None,
            Some("internal:ABC12345".to_owned()),
        ))
        .unwrap_or_else(|error| panic!("failed to encode response: {error}"));
        assert_eq!(
            encoded,
            json!({
                "id":"req_1",
                "error":{
                    "code":"LAUNCH_FAILED",
                    "message":"launch failed",
                    "launch_id":"internal:ABC12345"
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

    #[test]
    fn lifecycle_states_keep_wire_names_and_public_session_aliases() {
        assert_eq!(
            serde_json::to_value(SessionState::Restarting)
                .unwrap_or_else(|error| panic!("failed to serialize session state: {error}")),
            "restarting"
        );
        assert_eq!(
            serde_json::to_value(TurnState::Interrupted)
                .unwrap_or_else(|error| panic!("failed to serialize turn state: {error}")),
            "interrupted"
        );
        assert_eq!(SessionState::Running.public_name(), "starting");
        assert_eq!(SessionState::Quiescing.public_name(), "canceling");
    }
}
