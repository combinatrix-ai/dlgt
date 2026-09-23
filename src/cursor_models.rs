//! Discover Cursor models by parsing `cursor-agent --list-models`.
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
            "harness": "cursor",
            "source": "cli",
            "discovery": "cli",
            "models": models,
        }),
        Err(error) => json!({
            "harness": "cursor",
            "source": "cli",
            "discovery": "unavailable",
            "models": [],
            "error": format!("{error:#}"),
            "hint": "Use cursor-agent --list-models; pass the chosen ID with --model",
        }),
    }
}

fn discover() -> Result<Vec<Value>> {
    let program = crate::cursor_agent::program();
    let output = run_list_models_cli(&program)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let detail = first_nonempty(&stderr).or_else(|| first_nonempty(&stdout));
        bail!(
            "{} --list-models exited {}: {}",
            program.display(),
            output.status,
            detail.unwrap_or_default()
        );
    }
    let models = parse_list_models_output(&stdout);
    if models.is_empty() {
        bail!("cursor-agent --list-models produced no recognizable model IDs");
    }
    Ok(models)
}

fn run_list_models_cli(program: &std::path::Path) -> Result<std::process::Output> {
    let program = program.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // Auth tokens in the environment can make the CLI take a different path;
        // discovery should use the local logged-in agent like an interactive shell.
        let result = Command::new(&program)
            .arg("--list-models")
            .env_remove("CURSOR_AUTH_TOKEN")
            .env_remove("CURSOR_API_KEY")
            .output();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(CLI_TIMEOUT) {
        Ok(result) => result.with_context(|| "failed to run cursor-agent --list-models"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!(
                "cursor-agent --list-models timed out after {}s",
                CLI_TIMEOUT.as_secs()
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("cursor-agent --list-models worker disconnected")
        }
    }
}

/// Parse `cursor-agent --list-models` lines shaped like `id - Display Name`.
pub fn parse_list_models_output(text: &str) -> Vec<Value> {
    let mut models: Vec<Value> = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Available models") {
            continue;
        }
        let Some((id, rest)) = split_id_and_label(trimmed) else {
            continue;
        };
        if !valid_model_id(id) {
            continue;
        }
        if models
            .iter()
            .any(|model| model.get("id").and_then(Value::as_str) == Some(id))
        {
            continue;
        }
        let is_default = rest.contains("(default)") || rest.contains("(current, default)");
        if is_default {
            models.push(json!({"id": id, "default": true}));
        } else {
            models.push(json!({"id": id}));
        }
    }
    models
}

fn split_id_and_label(line: &str) -> Option<(&str, &str)> {
    // Require " - " so IDs that contain hyphens (gpt-5.3-codex) stay intact
    // and prose lines without that separator are ignored.
    let (id, rest) = line.split_once(" - ")?;
    let id = id.trim();
    if id.is_empty() || id.contains(char::is_whitespace) {
        None
    } else {
        Some((id, rest.trim()))
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
    fn parse_id_dash_label_and_default() {
        let text = r"Available models

auto - Auto (current, default)
gpt-5.3-codex-low - Codex 5.3 Low
composer-2.5 - Composer 2.5
grok-4.7-high - Grok 4.7  High
";
        let models = parse_list_models_output(text);
        assert_eq!(models.len(), 4);
        assert_eq!(models[0]["id"], "auto");
        assert_eq!(models[0]["default"], true);
        assert_eq!(models[1]["id"], "gpt-5.3-codex-low");
        assert!(models[1].get("default").is_none());
        assert_eq!(models[2]["id"], "composer-2.5");
        assert_eq!(models[3]["id"], "grok-4.7-high");
    }

    #[test]
    fn ignores_non_model_prose() {
        let text = "Please log in first\nAvailable models\n";
        let models = parse_list_models_output(text);
        assert!(models.is_empty());
    }

    #[test]
    fn deduplicates_ids() {
        let text = "auto - Auto (default)\nauto - Auto again\n";
        let models = parse_list_models_output(text);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["default"], true);
    }
}
