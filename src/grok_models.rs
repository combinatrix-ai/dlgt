//! Discover Grok models by parsing `grok models` CLI output.
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const CLI_TIMEOUT: Duration = Duration::from_secs(10);

pub fn list_models() -> Value {
    match discover() {
        Ok(models) => json!({
            "harness": "grok",
            "source": "cli",
            "discovery": "cli",
            "models": models,
        }),
        Err(error) => json!({
            "harness": "grok",
            "source": "cli",
            "discovery": "unavailable",
            "models": [],
            "error": format!("{error:#}"),
            "hint": "Use grok models or pass --model with a Grok model ID",
        }),
    }
}

fn discover() -> Result<Vec<Value>> {
    let program = crate::grok_agent::program();
    let output = run_models_cli(&program)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let detail = first_nonempty(&stderr).or_else(|| first_nonempty(&stdout));
        bail!(
            "{} models exited {}: {}",
            program.display(),
            output.status,
            detail.unwrap_or_default()
        );
    }
    let models = parse_models_output(&stdout);
    if models.is_empty() {
        bail!("grok models produced no recognizable model IDs");
    }
    Ok(models)
}

fn run_models_cli(program: &std::path::Path) -> Result<std::process::Output> {
    let program = program.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = Command::new(&program).arg("models").output();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(CLI_TIMEOUT) {
        Ok(result) => result.with_context(|| "failed to run grok models"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("grok models timed out after {}s", CLI_TIMEOUT.as_secs())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("grok models worker disconnected")
        }
    }
}

/// Parse `grok models` human-readable output into model entries.
///
/// Recognizes:
/// - `Default model: <id>`
/// - Available lines: `  * <id> (default)` or `  - <id>`
pub fn parse_models_output(text: &str) -> Vec<Value> {
    let mut default_id: Option<String> = None;
    let mut models: Vec<Value> = Vec::new();
    let mut in_available = false;

    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("Default model:") {
            let id = rest.trim();
            if valid_model_id(id) {
                default_id = Some(id.to_owned());
            }
            continue;
        }

        if trimmed.eq_ignore_ascii_case("Available models:")
            || trimmed.eq_ignore_ascii_case("Available models")
        {
            in_available = true;
            continue;
        }

        if !in_available {
            continue;
        }

        let Some(id) = parse_available_line(trimmed) else {
            if !trimmed.starts_with('*') && !trimmed.starts_with('-') {
                in_available = false;
            }
            continue;
        };
        if !valid_model_id(&id) {
            continue;
        }
        if models
            .iter()
            .any(|model| model.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            continue;
        }
        let is_default =
            default_id.as_deref() == Some(id.as_str()) || trimmed.contains("(default)");
        if is_default {
            default_id = Some(id.clone());
            models.push(json!({"id": id, "default": true}));
        } else {
            models.push(json!({"id": id}));
        }
    }

    if let Some(default) = default_id.as_deref() {
        let has_default = models.iter().any(|model| {
            model.get("default").and_then(Value::as_bool) == Some(true)
                && model.get("id").and_then(Value::as_str) == Some(default)
        });
        if !has_default {
            if let Some(model) = models
                .iter_mut()
                .find(|model| model.get("id").and_then(Value::as_str) == Some(default))
            {
                if let Some(object) = model.as_object_mut() {
                    object.insert("default".to_owned(), json!(true));
                }
            } else if valid_model_id(default) {
                models.insert(0, json!({"id": default, "default": true}));
            }
        }
    }

    models
}

fn parse_available_line(trimmed: &str) -> Option<String> {
    let rest = trimmed
        .strip_prefix('*')
        .or_else(|| trimmed.strip_prefix('-'))?
        .trim();
    // Model IDs are a single token; ignore trailing annotations like "(default)".
    let id = rest
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| c == '(' || c == ')' || c == ',' || c == '[' || c == ']');
    if id.is_empty() {
        None
    } else {
        Some(id.to_owned())
    }
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
        })
}

fn first_nonempty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_star_and_dash_available_list_with_default() {
        let text = r"You are logged in with grok.com.

Default model: grok-4.7

Available models:
  * grok-4.7 (default)
  - grok-4.7-build-fast
  - grok-4.6
  - grok-4.5
";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 4);
        assert_eq!(models[0]["id"], "grok-4.7");
        assert_eq!(models[0]["default"], true);
        assert_eq!(models[1]["id"], "grok-4.7-build-fast");
        assert!(models[1].get("default").is_none());
        assert_eq!(models[2]["id"], "grok-4.6");
        assert_eq!(models[3]["id"], "grok-4.5");
    }

    #[test]
    fn default_header_alone_marks_matching_available_entry() {
        let text = r"Default model: grok-4.6

Available models:
  - grok-4.7
  - grok-4.6
  - grok-4.5
";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 3);
        assert_eq!(models[1]["id"], "grok-4.6");
        assert_eq!(models[1]["default"], true);
        assert!(models[0].get("default").is_none());
    }

    #[test]
    fn default_only_header_synthesizes_entry() {
        let text = "Default model: grok-4.7\n";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "grok-4.7");
        assert_eq!(models[0]["default"], true);
    }

    #[test]
    fn skips_empty_marker_lines() {
        let text = r"Available models:
  -
  - ok-model
";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "ok-model");
    }

    #[test]
    fn deduplicates_ids() {
        let text = r"Available models:
  * grok-4.7 (default)
  - grok-4.7
";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["default"], true);
    }
}
