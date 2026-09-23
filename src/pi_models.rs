//! Discover Pi models by parsing `pi --list-models`.
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
            "harness": "pi",
            "source": "cli",
            "discovery": "cli",
            "models": models,
        }),
        Err(error) => json!({
            "harness": "pi",
            "source": "cli",
            "discovery": "unavailable",
            "models": [],
            "error": format!("{error:#}"),
            "hint": "Use pi --list-models or pass --model with --harness-option provider=<name>",
        }),
    }
}

fn discover() -> Result<Vec<Value>> {
    let program = crate::pi_agent::program();
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
        bail!("pi --list-models produced no recognizable model IDs");
    }
    Ok(models)
}

fn run_list_models_cli(program: &std::path::Path) -> Result<std::process::Output> {
    let program = program.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = Command::new(&program).arg("--list-models").output();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(CLI_TIMEOUT) {
        Ok(result) => result.with_context(|| "failed to run pi --list-models"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!(
                "pi --list-models timed out after {}s",
                CLI_TIMEOUT.as_secs()
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("pi --list-models worker disconnected")
        }
    }
}

/// Parse `pi --list-models`.
///
/// Current builds print a whitespace table whose first columns are `provider`
/// and `model`. IDs are returned as `provider/model`, which Pi also accepts
/// as `--model`. A `provider/model` line is accepted when the table header is
/// absent.
pub fn parse_list_models_output(text: &str) -> Vec<Value> {
    let text = strip_ansi(text);
    if let Some(models) = parse_table(&text)
        && !models.is_empty()
    {
        return models;
    }
    parse_slash_lines(&text)
}

fn parse_table(text: &str) -> Option<Vec<Value>> {
    let mut provider_index = None;
    let mut model_index = None;
    let mut models = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed
                .chars()
                .all(|character| character == '-' || character == ' ')
        {
            continue;
        }
        let columns = columns(trimmed);
        if provider_index.is_none() {
            let provider = columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case("provider"));
            let model = columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case("model"));
            if provider.is_some() && model.is_some() {
                provider_index = provider;
                model_index = model;
            }
            continue;
        }
        let (Some(provider_index), Some(model_index)) = (provider_index, model_index) else {
            continue;
        };
        let Some(provider) = columns.get(provider_index).copied() else {
            continue;
        };
        let Some(model) = columns.get(model_index).copied() else {
            continue;
        };
        push_model(&mut models, provider, model, trimmed.contains("(default)"));
    }
    provider_index.map(|_| models)
}

fn parse_slash_lines(text: &str) -> Vec<Value> {
    let mut models = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        let Some(token) = trimmed.split_whitespace().next() else {
            continue;
        };
        let Some((provider, model)) = token.split_once('/') else {
            continue;
        };
        push_model(&mut models, provider, model, trimmed.contains("(default)"));
    }
    models
}

fn push_model(models: &mut Vec<Value>, provider: &str, model: &str, is_default: bool) {
    if !valid_token(provider) || !valid_token(model) {
        return;
    }
    let id = format!("{provider}/{model}");
    if models
        .iter()
        .any(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
    {
        return;
    }
    if is_default {
        models.push(json!({"id": id, "default": true}));
    } else {
        models.push(json!({"id": id}));
    }
}

fn columns(line: &str) -> Vec<&str> {
    line.split("  ")
        .map(str::trim)
        .filter(|column| !column.is_empty())
        .collect()
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
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
    fn parse_provider_model_table() {
        let text = "\
provider  model     context  max-out  thinking  images
xai       grok-4.7  128K     8K       yes       no
xai       grok-4    128K     8K       yes       no
";
        let models = parse_list_models_output(text);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "xai/grok-4.7");
        assert_eq!(models[1]["id"], "xai/grok-4");
        assert!(models[0].get("default").is_none());
    }

    #[test]
    fn parse_slash_lines_when_there_is_no_table() {
        let text = "xai/grok-4.7 (default)\nopenai/gpt-4o\n";
        let models = parse_list_models_output(text);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "xai/grok-4.7");
        assert_eq!(models[0]["default"], true);
        assert_eq!(models[1]["id"], "openai/gpt-4o");
    }

    #[test]
    fn ignores_prose() {
        let models = parse_list_models_output("No models available\n");
        assert!(models.is_empty());
    }
}
