//! Discover model ids by parsing `opencode models`.
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const CLI_TIMEOUT: Duration = Duration::from_secs(15);

pub fn list_models() -> Value {
    match discover() {
        Ok(models) => json!({
            "harness": "opencode",
            "source": "cli",
            "discovery": "cli",
            "models": models,
        }),
        Err(error) => json!({
            "harness": "opencode",
            "source": "cli",
            "discovery": "unavailable",
            "models": [],
            "error": format!("{error:#}"),
            "hint": "Use opencode models or pass --model provider/model",
        }),
    }
}

fn discover() -> Result<Vec<Value>> {
    let program = crate::opencode_agent::program();
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
        bail!("opencode models produced no recognizable provider/model IDs");
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
        Ok(result) => result.with_context(|| "failed to run opencode models"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("opencode models timed out after {}s", CLI_TIMEOUT.as_secs())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("opencode models worker disconnected")
        }
    }
}

/// Parse `opencode models` lines. The documented form is one `provider/model`
/// token per line. A trailing `(default)` marks the default.
pub fn parse_models_output(text: &str) -> Vec<Value> {
    let text = strip_ansi(text);
    let mut models: Vec<Value> = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(token) = parts.next() else {
            continue;
        };
        let is_default = trimmed.contains("(default)");
        if !valid_model_id(token) {
            continue;
        }
        if models
            .iter()
            .any(|model| model.get("id").and_then(Value::as_str) == Some(token))
        {
            continue;
        }
        if is_default {
            models.push(json!({"id": token, "default": true}));
        } else {
            models.push(json!({"id": token}));
        }
    }
    models
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id.contains('/')
        && !id.starts_with('/')
        && !id.ends_with('/')
        && !id.contains("//")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(character);
    }
    out
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
    fn parse_provider_model_lines_and_default() {
        let text = "\u{1b}[32mxai/grok-4.7\u{1b}[0m\nxai/grok-4.6 (default)\nnot a model\n";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "xai/grok-4.7");
        assert!(models[0].get("default").is_none());
        assert_eq!(models[1]["id"], "xai/grok-4.6");
        assert_eq!(models[1]["default"], true);
    }

    #[test]
    fn deduplicates_ids() {
        let text = "xai/grok-4.7\nxai/grok-4.7 (default)\n";
        let models = parse_models_output(text);
        assert_eq!(models.len(), 1);
        assert!(models[0].get("default").is_none());
    }
}
