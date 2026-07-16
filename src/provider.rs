use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};
use uuid::Uuid;

static CODEX_CONFIG_LOCK: Mutex<()> = Mutex::new(());

const CODEX_UPDATE_SUPPRESSION: &str = "check_for_update_on_startup=false";
const CLAUDE_AUTOUPDATER_ENV: &str = "DISABLE_AUTOUPDATER";

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Claude,
}

impl Agent {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => bail!("unsupported agent {value:?}; expected codex or claude"),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn semantic_input(self, prompt: &str) -> Result<Vec<u8>> {
        match self {
            Self::Claude => bracketed_paste_input(prompt),
            Self::Codex => bail!("Codex semantic input must use app-server turn/start"),
        }
    }

    pub const fn cancel_input(self) -> &'static [u8] {
        match self {
            Self::Codex => &[0x03],
            Self::Claude => &[0x1b],
        }
    }
}

fn bracketed_paste_input(prompt: &str) -> Result<Vec<u8>> {
    if prompt.contains(['\0', '\u{1b}']) {
        bail!("semantic prompts must not contain NUL or ESC control bytes");
    }
    let mut input = Vec::with_capacity(prompt.len() + 13);
    input.extend_from_slice(b"\x1b[200~");
    input.extend_from_slice(prompt.as_bytes());
    input.extend_from_slice(b"\x1b[201~\r");
    Ok(input)
}

#[derive(Debug, Clone)]
pub struct LaunchOptions<'a> {
    pub agent: Agent,
    pub session_id: &'a str,
    pub alias: &'a str,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub resume_provider_id: Option<&'a str>,
    pub environment: &'a HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub environment: HashMap<String, String>,
}

pub fn command_spec(options: &LaunchOptions<'_>) -> Result<CommandSpec> {
    match options.agent {
        Agent::Codex => bail!("Codex requires the app-server runtime"),
        Agent::Claude => claude_command(options),
    }
}

pub fn codex_program() -> PathBuf {
    std::env::var_os("DLGT_CODEX_BIN").map_or_else(|| PathBuf::from("codex"), PathBuf::from)
}

pub fn codex_remote_tui_command(options: &LaunchOptions<'_>, socket_path: &Path) -> CommandSpec {
    let program =
        std::env::var_os("DLGT_CODEX_BIN").map_or_else(|| PathBuf::from("codex"), PathBuf::from);
    let mut args = vec!["--config".to_owned(), CODEX_UPDATE_SUPPRESSION.to_owned()];
    if let Some(provider_id) = options.resume_provider_id {
        args.extend(["resume".to_owned(), provider_id.to_owned()]);
    }
    args.extend([
        "--remote".to_owned(),
        format!("unix://{}", socket_path.display()),
        "--no-alt-screen".to_owned(),
        "--dangerously-bypass-approvals-and-sandbox".to_owned(),
    ]);
    if let Some(model) = options.model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = options.effort {
        args.extend([
            "--config".to_owned(),
            format!("model_reasoning_effort={}", toml_string(effort)),
        ]);
    }
    CommandSpec {
        program,
        args,
        cwd: options.cwd.to_path_buf(),
        environment: options.environment.clone(),
    }
}

pub(crate) fn codex_app_server_args(endpoint: &str) -> Vec<String> {
    vec![
        "--config".to_owned(),
        CODEX_UPDATE_SUPPRESSION.to_owned(),
        "app-server".to_owned(),
        "--listen".to_owned(),
        endpoint.to_owned(),
    ]
}

pub fn prepare_workspace(agent: Agent, cwd: &Path) -> Result<()> {
    if agent == Agent::Codex {
        let home = std::env::var_os("CODEX_HOME").map_or_else(
            || {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".codex"))
                    .context("HOME is not set")
            },
            |home| Ok(PathBuf::from(home)),
        )?;
        trust_codex_workspace(&home, cwd)?;
    }
    Ok(())
}

fn trust_codex_workspace(codex_home: &Path, cwd: &Path) -> Result<()> {
    let _guard = CODEX_CONFIG_LOCK
        .lock()
        .map_err(|_| anyhow!("Codex config lock poisoned"))?;
    fs::create_dir_all(codex_home)
        .with_context(|| format!("failed to create {}", codex_home.display()))?;
    let config_path = codex_home.join("config.toml");
    let existing = match fs::read_to_string(&config_path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    let mut document = existing
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let cwd = cwd
        .to_str()
        .context("Codex workspace path is not valid UTF-8")?;
    if project_is_trusted(&document, cwd) {
        return Ok(());
    }
    set_project_trusted(&mut document, cwd)?;
    write_codex_config(&config_path, &document.to_string())
}

fn set_project_trusted(document: &mut DocumentMut, cwd: &str) -> Result<()> {
    if document.get("projects").is_none() {
        document["projects"] = Item::Table(Table::new());
    }
    let projects = document
        .get_mut("projects")
        .and_then(Item::as_table_like_mut)
        .context("Codex config `projects` must be a table")?;
    if projects.get(cwd).is_none() {
        projects.insert(cwd, Item::Table(Table::new()));
    }
    let project = projects
        .get_mut(cwd)
        .and_then(Item::as_table_like_mut)
        .with_context(|| format!("Codex config project {cwd:?} must be a table"))?;
    project.insert("trust_level", value("trusted"));
    Ok(())
}

fn project_is_trusted(document: &DocumentMut, cwd: &str) -> bool {
    document
        .get("projects")
        .and_then(Item::as_table_like)
        .and_then(|projects| projects.get(cwd))
        .and_then(Item::as_table_like)
        .and_then(|project| project.get("trust_level"))
        .and_then(Item::as_str)
        == Some("trusted")
}

fn write_codex_config(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().context("Codex config has no parent")?;
    let temporary = parent.join(format!(".config.toml.dlgt-{}.tmp", Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        } else {
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        OpenOptions::new()
            .read(true)
            .open(parent)
            .with_context(|| format!("failed to open {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("failed to sync {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn claude_command(options: &LaunchOptions<'_>) -> Result<CommandSpec> {
    let program =
        std::env::var_os("DLGT_CLAUDE_BIN").map_or_else(|| PathBuf::from("claude"), PathBuf::from);
    let hook_command = hook_command(options)?;
    let settings = claude_hook_settings(&hook_command);
    let mut args = vec![
        "--dangerously-skip-permissions".to_owned(),
        "--name".to_owned(),
        options.alias.trim_start_matches('@').to_owned(),
        "--settings".to_owned(),
        settings.to_string(),
    ];
    if let Some(model) = options.model {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) = options.effort {
        args.extend(["--effort".to_owned(), effort.to_owned()]);
    }
    if let Some(provider_id) = options.resume_provider_id {
        args.extend(["--resume".to_owned(), provider_id.to_owned()]);
    }
    Ok(CommandSpec {
        program,
        args,
        cwd: options.cwd.to_path_buf(),
        environment: claude_environment(options.environment),
    })
}

fn claude_environment(environment: &HashMap<String, String>) -> HashMap<String, String> {
    let mut environment = environment.clone();
    environment.insert(CLAUDE_AUTOUPDATER_ENV.to_owned(), "1".to_owned());
    environment
}

fn hook_command(options: &LaunchOptions<'_>) -> Result<String> {
    let executable = std::env::current_exe()?;
    Ok(format!(
        "{} hook emit {} {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(options.session_id),
        shell_quote(options.agent.as_str()),
    ))
}

fn claude_hook_settings(command: &str) -> serde_json::Value {
    let handler = || {
        serde_json::json!([{
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": 5,
            }]
        }])
    };
    serde_json::json!({
        "hooks": {
            "SessionStart": handler(),
            "UserPromptSubmit": handler(),
            "Stop": handler(),
            "StopFailure": handler(),
            "Notification": handler(),
            "SessionEnd": handler(),
        }
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::{
        Agent, LaunchOptions, codex_app_server_args, codex_remote_tui_command, command_spec,
        trust_codex_workspace,
    };

    #[test]
    fn claude_is_interactive_and_skips_permission_prompts() {
        let environment =
            std::collections::HashMap::from([("DISABLE_AUTOUPDATER".to_owned(), "0".to_owned())]);
        let spec = command_spec(&LaunchOptions {
            agent: Agent::Claude,
            session_id: "ses_1",
            alias: "@worker",
            cwd: Path::new("/tmp"),
            model: Some("claude-4-5-haiku-latest"),
            effort: Some("high"),
            resume_provider_id: None,
            environment: &environment,
        })
        .unwrap_or_else(|error| panic!("failed to build command: {error}"));
        assert!(
            spec.args
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
        assert!(!spec.args.iter().any(|arg| arg == "--print"));
        assert_eq!(
            spec.environment.get("DISABLE_AUTOUPDATER"),
            Some(&"1".to_owned())
        );
    }

    #[test]
    fn codex_remote_tui_keeps_the_primary_screen_visible_without_hooks() {
        let environment = std::collections::HashMap::from([(
            "CODEX_HOME".to_owned(),
            "/tmp/user-codex-home".to_owned(),
        )]);
        let spec = codex_remote_tui_command(
            &LaunchOptions {
                agent: Agent::Codex,
                session_id: "ses_1",
                alias: "@worker",
                cwd: Path::new("/tmp"),
                model: None,
                effort: Some("xhigh"),
                resume_provider_id: None,
                environment: &environment,
            },
            Path::new("/tmp/dlgt.sock"),
        );
        assert_eq!(
            &spec.args[..2],
            ["--config", "check_for_update_on_startup=false"]
        );
        assert!(
            spec.args
                .windows(2)
                .any(|args| args == ["--remote", "unix:///tmp/dlgt.sock"])
        );
        assert!(!spec.args.iter().any(|arg| arg == "resume"));
        assert!(spec.args.iter().any(|arg| arg == "--no-alt-screen"));
        assert!(
            spec.args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
        );
        assert!(
            spec.args
                .iter()
                .any(|arg| arg.contains("model_reasoning_effort"))
        );
        assert_eq!(spec.environment, environment);
    }

    #[test]
    fn codex_app_server_suppresses_startup_update_prompts() {
        assert_eq!(
            codex_app_server_args("unix:///tmp/dlgt.sock"),
            [
                "--config",
                "check_for_update_on_startup=false",
                "app-server",
                "--listen",
                "unix:///tmp/dlgt.sock",
            ]
        );
    }

    #[test]
    fn claude_registers_success_and_failure_hooks() {
        let environment = std::collections::HashMap::new();
        let spec = command_spec(&LaunchOptions {
            agent: Agent::Claude,
            session_id: "ses_1",
            alias: "@worker",
            cwd: Path::new("/tmp"),
            model: None,
            effort: None,
            resume_provider_id: None,
            environment: &environment,
        })
        .unwrap_or_else(|error| panic!("failed to build command: {error}"));
        let settings = spec
            .args
            .windows(2)
            .find(|args| args[0] == "--settings")
            .map_or_else(|| panic!("Claude settings missing"), |args| &args[1]);
        assert!(settings.contains("StopFailure"));
        assert!(settings.contains("Stop"));
    }

    #[test]
    fn semantic_input_uses_bracketed_paste_then_enter() {
        let input = Agent::Claude
            .semantic_input("first\nsecond")
            .unwrap_or_else(|error| panic!("failed to frame prompt: {error}"));
        assert_eq!(input, b"\x1b[200~first\nsecond\x1b[201~\r");
    }

    #[test]
    fn provider_commands_resume_the_stored_conversation() {
        let environment = std::collections::HashMap::new();
        let claude = command_spec(&LaunchOptions {
            agent: Agent::Claude,
            session_id: "ses_1",
            alias: "@worker",
            cwd: Path::new("/tmp"),
            model: None,
            effort: None,
            resume_provider_id: Some("claude-session"),
            environment: &environment,
        })
        .unwrap_or_else(|error| panic!("failed to build Claude command: {error}"));
        assert!(
            claude
                .args
                .windows(2)
                .any(|args| args == ["--resume", "claude-session"])
        );

        let codex = codex_remote_tui_command(
            &LaunchOptions {
                agent: Agent::Codex,
                session_id: "ses_1",
                alias: "@worker",
                cwd: Path::new("/tmp"),
                model: None,
                effort: None,
                resume_provider_id: Some("codex-thread"),
                environment: &environment,
            },
            Path::new("/tmp/dlgt.sock"),
        );
        assert!(
            codex
                .args
                .windows(2)
                .any(|args| args == ["resume", "codex-thread"])
        );
    }

    #[test]
    fn semantic_input_rejects_terminal_escape_injection() {
        assert!(Agent::Claude.semantic_input("unsafe\x1b[201~").is_err());
        assert!(Agent::Codex.semantic_input("safe").is_err());
    }

    #[test]
    fn codex_workspace_trust_is_atomic_and_idempotent() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let codex_home = directory.path().join("codex-home");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&codex_home)
            .unwrap_or_else(|error| panic!("failed to create Codex home: {error}"));
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("failed to create workspace: {error}"));
        let config = codex_home.join("config.toml");
        fs::write(&config, "model = \"gpt-test\"\n")
            .unwrap_or_else(|error| panic!("failed to seed config: {error}"));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|error| panic!("failed to secure config: {error}"));

        trust_codex_workspace(&codex_home, &workspace)
            .unwrap_or_else(|error| panic!("failed to trust workspace: {error}"));
        let first = fs::read_to_string(&config)
            .unwrap_or_else(|error| panic!("failed to read config: {error}"));
        assert!(first.contains("model = \"gpt-test\""));
        assert!(first.contains("trust_level = \"trusted\""));
        assert!(first.contains(&workspace.to_string_lossy().to_string()));
        assert_eq!(
            fs::metadata(&config)
                .unwrap_or_else(|error| panic!("failed to stat config: {error}"))
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        trust_codex_workspace(&codex_home, &workspace)
            .unwrap_or_else(|error| panic!("failed to re-trust workspace: {error}"));
        let second = fs::read_to_string(&config)
            .unwrap_or_else(|error| panic!("failed to reread config: {error}"));
        assert_eq!(first, second);
    }

    #[test]
    fn malformed_codex_project_table_returns_an_error() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("failed to create temporary directory: {error}"));
        let codex_home = directory.path().join("codex-home");
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&codex_home)
            .unwrap_or_else(|error| panic!("failed to create Codex home: {error}"));
        fs::create_dir_all(&workspace)
            .unwrap_or_else(|error| panic!("failed to create workspace: {error}"));
        fs::write(codex_home.join("config.toml"), "projects = \"invalid\"\n")
            .unwrap_or_else(|error| panic!("failed to seed malformed config: {error}"));

        let error = trust_codex_workspace(&codex_home, &workspace)
            .err()
            .unwrap_or_else(|| panic!("malformed projects table should fail"));
        assert!(error.to_string().contains("must be a table"));
    }
}
