use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const SNAPSHOT_URL: &str =
    "https://raw.githubusercontent.com/combinatrix-ai/claude-models-list/main/models.json";

const MAX_BODY_BYTES: u64 = 1024 * 1024;
const MAX_SNAPSHOT_MODELS: usize = 256;
const CACHE_TTL: Duration = Duration::from_hours(1);
const ALIASES: [&str; 5] = ["default", "best", "sonnet", "opus", "haiku"];
const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Clone)]
struct Snapshot {
    models: Vec<Value>,
    retrieved_at: Option<String>,
}

static SNAPSHOT_CACHE: OnceLock<Mutex<Option<(Instant, Snapshot)>>> = OnceLock::new();

pub fn list_models() -> Value {
    match load_snapshot() {
        Ok(snapshot) => json!({
            "harness": "claude",
            "source": SNAPSHOT_URL,
            "discovery": "snapshot",
            "retrieved_at": snapshot.retrieved_at,
            "models": with_aliases(snapshot.models),
        }),
        Err(error) => json!({
            "harness": "claude",
            "source": "claude-code-aliases",
            "discovery": "partial",
            "warning": format!("public model snapshot unavailable: {error:#}"),
            "models": aliases(),
        }),
    }
}

pub fn validate_model_effort(model: Option<&str>, effort: Option<&str>) -> Result<()> {
    let (Some(model), Some(effort)) = (model, effort) else {
        return Ok(());
    };
    if ALIASES.contains(&model) {
        return Ok(());
    }
    let Ok(snapshot) = load_snapshot() else {
        return Ok(());
    };
    validate_against_snapshot(&snapshot, model, effort)
}

fn load_snapshot() -> Result<Snapshot> {
    let cache = SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some((fetched_at, snapshot)) = guard.as_ref()
        && fetched_at.elapsed() < CACHE_TTL
    {
        return Ok(snapshot.clone());
    }
    let snapshot = fetch_snapshot()?;
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), snapshot.clone()));
    }
    Ok(snapshot)
}

fn fetch_snapshot() -> Result<Snapshot> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build();
    let agent: ureq::Agent = config.into();
    let body = agent
        .get(SNAPSHOT_URL)
        .call()
        .context("failed to fetch public model snapshot")?
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_string()
        .context("failed to read public model snapshot")?;
    let snapshot: Value =
        serde_json::from_str(&body).context("public model snapshot is not valid JSON")?;
    parse_snapshot(&snapshot)
}

fn parse_snapshot(snapshot: &Value) -> Result<Snapshot> {
    let data = snapshot
        .get("data")
        .and_then(Value::as_array)
        .context("public model snapshot has no data array")?;
    if data.len() > MAX_SNAPSHOT_MODELS {
        bail!("public model snapshot contains too many models");
    }

    let mut seen = HashSet::new();
    let models = data
        .iter()
        .filter_map(|model| {
            let id = model.get("id").and_then(Value::as_str)?;
            if !valid_model_id(id) {
                return None;
            }
            let id = undated_model_id(id);
            if !seen.insert(id.clone()) {
                return None;
            }
            let mut model = model.clone();
            model.as_object_mut()?.insert("id".to_owned(), json!(id));
            Some(model)
        })
        .collect();
    let retrieved_at = snapshot
        .get("retrieved_at")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Snapshot {
        models,
        retrieved_at,
    })
}

fn validate_against_snapshot(snapshot: &Snapshot, model_id: &str, effort: &str) -> Result<()> {
    let normalized = undated_model_id(model_id);
    let Some(model) = snapshot
        .models
        .iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(normalized.as_str()))
    else {
        return Ok(());
    };
    let supported = EFFORT_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            model
                .pointer(&format!("/capabilities/effort/{level}/supported"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if supported.contains(&effort) {
        return Ok(());
    }
    let supported = if supported.is_empty() {
        "none".to_owned()
    } else {
        supported.join(", ")
    };
    bail!(
        "invalid model/effort combination: effort {effort:?} is not supported by Claude model {model_id:?}; supported efforts: {supported}"
    )
}

fn with_aliases(models: Vec<Value>) -> Vec<Value> {
    let mut combined = aliases();
    combined.extend(models);
    combined
}

fn aliases() -> Vec<Value> {
    vec![
        json!({"id":"default", "kind":"alias", "recommended":true}),
        json!({"id":"best", "kind":"alias"}),
        json!({"id":"sonnet", "kind":"alias"}),
        json!({"id":"opus", "kind":"alias"}),
        json!({"id":"haiku", "kind":"alias"}),
    ]
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.starts_with("claude-")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Claude Code accepts the undated alias for every date-pinned ID the public
/// snapshot still returns, so the alias is the form worth presenting.
fn undated_model_id(id: &str) -> String {
    match id.rsplit_once('-') {
        Some((head, suffix))
            if suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            head.to_owned()
        }
        _ => id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_only_strips_a_terminal_yyyymmdd() {
        assert_eq!(
            undated_model_id("claude-opus-4-5-20251101"),
            "claude-opus-4-5"
        );
        assert_eq!(undated_model_id("claude-opus-5"), "claude-opus-5");
        assert_eq!(
            undated_model_id("claude-20251101-preview"),
            "claude-20251101-preview"
        );
        assert_eq!(
            undated_model_id("claude-opus-2025110"),
            "claude-opus-2025110"
        );
    }

    #[test]
    fn snapshot_normalizes_dated_ids_and_drops_invalid_and_duplicates() {
        let snapshot = json!({
            "retrieved_at": "2026-08-01T00:00:00Z",
            "data": [
                {"id":"claude-opus-5", "display_name":"Claude Opus 5"},
                {"id":"claude-opus-4-5-20251101", "display_name":"Claude Opus 4.5"},
                {"id":"not-claude"},
                {"id":"claude-opus-5", "display_name":"duplicate"},
                {"id":"claude-sonnet-5"}
            ]
        });
        let snapshot =
            parse_snapshot(&snapshot).unwrap_or_else(|error| panic!("parse failed: {error:#}"));
        assert_eq!(
            snapshot.retrieved_at.as_deref(),
            Some("2026-08-01T00:00:00Z")
        );
        assert_eq!(snapshot.models.len(), 3);
        assert_eq!(snapshot.models[0]["id"], "claude-opus-5");
        assert_eq!(snapshot.models[1]["id"], "claude-opus-4-5");
        assert_eq!(snapshot.models[1]["display_name"], "Claude Opus 4.5");
        assert_eq!(snapshot.models[2]["id"], "claude-sonnet-5");
    }

    #[test]
    fn undated_and_dated_ids_collapse_to_one_entry() {
        let snapshot = json!({
            "data": [
                {"id":"claude-haiku-4-5", "display_name":"alias"},
                {"id":"claude-haiku-4-5-20251001", "display_name":"dated"}
            ]
        });
        let snapshot =
            parse_snapshot(&snapshot).unwrap_or_else(|error| panic!("parse failed: {error:#}"));
        assert_eq!(snapshot.models.len(), 1);
        assert_eq!(snapshot.models[0]["display_name"], "alias");
    }

    #[test]
    fn aliases_precede_snapshot_models() {
        let models = with_aliases(vec![json!({"id":"claude-fable-5"})]);
        assert_eq!(models[0]["id"], "default");
        assert_eq!(models[0]["recommended"], true);
        assert_eq!(models[5]["id"], "claude-fable-5");
    }

    #[test]
    fn exact_models_validate_effort_capabilities() {
        let snapshot = Snapshot {
            retrieved_at: None,
            models: vec![json!({
                "id":"claude-sonnet-4-6",
                "capabilities":{"effort":{
                    "supported":true,
                    "low":{"supported":true},
                    "medium":{"supported":true},
                    "high":{"supported":true},
                    "xhigh":{"supported":false},
                    "max":{"supported":true}
                }}
            })],
        };
        assert!(validate_against_snapshot(&snapshot, "claude-sonnet-4-6", "max").is_ok());
        let error = match validate_against_snapshot(&snapshot, "claude-sonnet-4-6", "xhigh") {
            Ok(()) => panic!("unsupported effort unexpectedly passed validation"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("supported efforts: low, medium, high, max"));
    }

    #[test]
    fn dated_model_input_validates_against_its_alias() {
        let snapshot = Snapshot {
            retrieved_at: None,
            models: vec![json!({
                "id":"claude-opus-4-5",
                "capabilities":{"effort":{
                    "supported":true,
                    "low":{"supported":true},
                    "medium":{"supported":true},
                    "high":{"supported":true},
                    "xhigh":{"supported":false},
                    "max":{"supported":false}
                }}
            })],
        };
        assert!(validate_against_snapshot(&snapshot, "claude-opus-4-5-20251101", "high").is_ok());
        let error = match validate_against_snapshot(&snapshot, "claude-opus-4-5-20251101", "max") {
            Ok(()) => panic!("unsupported effort unexpectedly passed validation"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("claude-opus-4-5-20251101"));
        assert!(error.contains("supported efforts: low, medium, high"));
    }

    #[test]
    fn aliases_and_unknown_models_are_left_to_claude_code() {
        let snapshot = Snapshot {
            retrieved_at: None,
            models: Vec::new(),
        };
        assert!(validate_against_snapshot(&snapshot, "opus", "future-effort").is_ok());
        assert!(validate_against_snapshot(&snapshot, "claude-future-9", "future-effort").is_ok());
    }
}
