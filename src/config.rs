use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::error::{Code, WithCode, coded};
use serde_json::{Map, Value, json};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::protocol::{ApprovalsMode, HarnessSpec};

const BUILTIN_HARNESSES: &[&str] = &["claude", "codex", "cursor", "grok", "agy", "opencode"];

const DEFAULT_HARNESS_KEYS: &[&str] = &[
    "model",
    "effort",
    "approvals",
    "clean_env",
    "pass_env",
    "unset_env",
    "env",
    "harness_options",
];

#[derive(Debug, Clone)]
pub struct Config {
    profiles: BTreeMap<String, Profile>,
    approvals: Option<ApprovalsMode>,
    defaults: BTreeMap<String, PartialLaunch>,
    harnesses: BTreeMap<String, HarnessSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub harness: Option<String>,
    settings: PartialLaunch,
    public: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialLaunch {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub approvals: Option<ApprovalsMode>,
    pub clean_env: Option<bool>,
    pub env: BTreeMap<String, String>,
    pub pass_env: Vec<String>,
    pub unset_env: Vec<String>,
    pub harness_options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub model: Option<String>,
    pub effort: Option<String>,
    /// `None` for an ACP Harness without an approvals mapping: the agent's
    /// own configuration decides and dlgt requests nothing.
    pub approvals: Option<ApprovalsMode>,
    pub clean_env: bool,
    pub harness_options: Vec<String>,
    pub environment: BTreeMap<String, String>,
    /// Present for an ACP Harness.
    pub harness_spec: Option<HarnessSpec>,
}

impl Default for Config {
    fn default() -> Self {
        let defaults = BUILTIN_HARNESSES
            .iter()
            .map(|name| ((*name).to_owned(), PartialLaunch::default()))
            .collect();
        Self {
            profiles: BTreeMap::new(),
            approvals: None,
            defaults,
            harnesses: BTreeMap::new(),
        }
    }
}

pub fn validate_harness_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid_first = characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase());
    if !valid_first
        || name.len() > 32
        || !characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        bail!("Harness name must match [a-z][a-z0-9-]{{0,31}}");
    }
    Ok(())
}

fn is_builtin_harness(name: &str) -> bool {
    BUILTIN_HARNESSES.contains(&name)
}

pub fn is_acp_harness(name: &str) -> bool {
    !matches!(name, "claude" | "codex")
}

fn builtin_harness(name: &str) -> Option<HarnessSpec> {
    let argv = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let (command, login) = match name {
        "cursor" => (argv(&["cursor-agent", "acp"]), "cursor-agent login"),
        "grok" => (argv(&["grok", "agent", "stdio"]), "grok login"),
        "opencode" => (argv(&["opencode", "acp"]), "opencode auth login"),
        "agy" => (argv(&["agy", "acp"]), "agy acp --login"),
        _ => return None,
    };
    let mut spec = HarnessSpec {
        command,
        login: Some(login.to_owned()),
        ..HarnessSpec::default()
    };
    if name == "agy" {
        spec.approvals = BTreeMap::from([
            (ApprovalsMode::Auto, "auto".to_owned()),
            (ApprovalsMode::Ask, "default".to_owned()),
        ]);
        spec.resume = Some(argv(&["agy", "--conversation", "{id}"]));
    }
    Some(spec)
}

/// Credential-related environment names only; values remain in the child
/// process environment and are never copied into a Session snapshot.
pub fn auth_env_names<'a>(
    harness: &str,
    environment: impl Iterator<Item = (&'a String, &'a String)>,
) -> Vec<String> {
    const CLAUDE: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "AWS_BEARER_TOKEN_BEDROCK",
        "ANTHROPIC_FOUNDRY_API_KEY",
    ];
    const CODEX: &[&str] = &["OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL"];
    const CURSOR: &[&str] = &["CURSOR_API_KEY"];
    const GROK: &[&str] = &["XAI_API_KEY"];
    const AGY: &[&str] = &[
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "AGY_ADC_AUTH",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ];
    environment
        .filter(|(name, value)| {
            if value.is_empty() {
                return false;
            }
            match harness {
                "claude" => CLAUDE.contains(&name.as_str()),
                "codex" => CODEX.contains(&name.as_str()),
                "cursor" => CURSOR.contains(&name.as_str()),
                "grok" => GROK.contains(&name.as_str()),
                "agy" => AGY.contains(&name.as_str()),
                _ => api_key_environment_name(name),
            }
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn api_key_environment_name(name: &str) -> bool {
    name.ends_with("_API_KEY")
        && name.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
}

pub fn path() -> PathBuf {
    std::env::var_os("DLGT_CONFIG").map_or_else(
        || {
            std::env::var_os("HOME")
                .map_or_else(PathBuf::new, PathBuf::from)
                .join(".config/dlgt/config.toml")
        },
        PathBuf::from,
    )
}

pub fn load() -> Result<Config> {
    load_from_path(&path())
}

pub fn load_from_path(path: &std::path::Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", path.display()))
                .code(Code::InvalidArgument);
        }
    };
    parse(&text).with_context(|| format!("invalid dlgt config at {}", path.display()))
}

pub fn parse(text: &str) -> Result<Config> {
    let document = toml_edit::Document::parse(text).map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start);
        let (line, column) = line_column(text, offset);
        coded(
            Code::InvalidArgument,
            format!("invalid dlgt config at {line}:{column}: invalid TOML syntax"),
        )
    })?;
    // The reason can quote a configured value, so only the location of the
    // innermost offending item is reported.
    parse_document(document.as_table()).map_err(|error| {
        let offset = error.downcast_ref::<ItemSpan>().map_or(0, |span| span.0);
        let (line, column) = line_column(text, offset);
        coded(
            Code::InvalidArgument,
            format!("invalid dlgt config at {line}:{column}: invalid configuration entry"),
        )
    })
}

/// Where in the source the item being parsed starts.
#[derive(Debug)]
struct ItemSpan(usize);

impl fmt::Display for ItemSpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at offset {}", self.0)
    }
}

impl Error for ItemSpan {}

/// Record the innermost item that failed; outer items keep its location.
fn at_item<T>(item: &Item, result: Result<T>) -> Result<T> {
    at_span(item.span(), result)
}

fn at_span<T>(span: Option<std::ops::Range<usize>>, result: Result<T>) -> Result<T> {
    match (result, span) {
        (Err(error), Some(span)) if error.downcast_ref::<ItemSpan>().is_none() => {
            Err(error.context(ItemSpan(span.start)))
        }
        (result, _) => result,
    }
}

pub fn parse_toml_document(text: &str, source: &str) -> Result<DocumentMut> {
    parse_toml_document_as(text, &format!("TOML in {source}"))
}

fn parse_toml_document_as(text: &str, subject: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>().map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start);
        let (line, column) = line_column(text, offset);
        coded(
            Code::InvalidArgument,
            format!("invalid {subject} at {line}:{column}: invalid TOML syntax"),
        )
    })
}

fn parse_document(document: &toml_edit::Table) -> Result<Config> {
    let mut config = Config::default();
    for (key, item) in document {
        if !matches!(key, "defaults" | "profiles" | "harnesses") {
            at_item(
                item,
                Err(anyhow!(
                    "unknown top-level key {key:?}; expected [defaults], [profiles], or [harnesses]"
                )),
            )?;
        }
    }

    if let Some(harnesses) = document.get("harnesses") {
        let table = at_item(
            harnesses,
            harnesses
                .as_table_like()
                .context("[harnesses] must be a table"),
        )?;
        for (name, item) in table.iter() {
            let spec = at_item(item, parse_harness(name, item))?;
            config.harnesses.insert(name.to_owned(), spec);
            config.defaults.entry(name.to_owned()).or_default();
        }
    }

    if let Some(defaults) = document.get("defaults") {
        let table = at_item(
            defaults,
            defaults
                .as_table_like()
                .context("[defaults] must be a table"),
        )?;
        for (key, item) in table.iter() {
            at_item(
                item,
                (|| {
                    match key {
                        "approvals" => {
                            config.approvals = Some(parse_approvals(item, "[defaults].approvals")?);
                        }
                        name => {
                            validate_harness_name(name)?;
                            let keys = config.launch_keys(name)?;
                            let table = item
                                .as_table_like()
                                .with_context(|| format!("[defaults.{name}] must be a table"))?;
                            config.defaults.insert(
                                name.to_owned(),
                                parse_settings(table, &format!("defaults.{name}"), &keys)?,
                            );
                        }
                    }
                    Ok(())
                })(),
            )?;
        }
    }

    if let Some(profiles) = document.get("profiles") {
        let table = at_item(
            profiles,
            profiles
                .as_table_like()
                .context("[profiles] must be a table"),
        )?;
        for (name, item) in table.iter() {
            let profile_table = at_item(
                item,
                item.as_table_like()
                    .with_context(|| format!("profile {name:?} must be a table")),
            )?;
            let profile = at_item(item, parse_profile(&config, name, profile_table))?;
            config.profiles.insert(name.to_owned(), profile);
        }
    }
    Ok(config)
}

/// One `[harnesses.<name>]` table. A built-in name keeps its built-in value
/// for every key the table does not set.
fn parse_harness(name: &str, item: &Item) -> Result<HarnessSpec> {
    validate_harness_name(name)?;
    if !is_acp_harness(name) {
        bail!("[harnesses.{name}] cannot replace a native Harness");
    }
    let entry = item
        .as_table_like()
        .with_context(|| format!("[harnesses.{name}] must be a table"))?;
    let mut spec = builtin_harness(name).unwrap_or_default();
    for (key, value) in entry.iter() {
        let key_name = format!("harnesses.{name}.{key}");
        at_item(
            value,
            (|| {
                match key {
                    "command" => {
                        spec.command = string_array(value, &key_name)?;
                        if spec.command.first().is_none_or(String::is_empty) {
                            bail!("{key_name} must name a program");
                        }
                    }
                    "login" => spec.login = Some(required_string(value, &key_name)?),
                    "approvals" => spec.approvals = parse_approvals_mapping(value, &key_name)?,
                    "resume" => {
                        let argv = string_array(value, &key_name)?;
                        if argv.is_empty() {
                            bail!("{key_name} must not be empty");
                        }
                        spec.resume = Some(argv);
                    }
                    _ => bail!("unknown key {key:?} in [harnesses.{name}]"),
                }
                Ok(())
            })(),
        )?;
    }
    if spec.command.is_empty() {
        bail!("[harnesses.{name}] requires a command array");
    }
    Ok(spec)
}

impl Config {
    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles
            .get(name)
            .with_context(|| format!("profile not found: {name}"))
            .code(Code::NotFound)
    }

    pub fn profiles_value(&self) -> Value {
        Value::Object(
            self.profiles
                .iter()
                .map(|(name, profile)| (name.clone(), profile.public.clone()))
                .collect(),
        )
    }

    pub fn profile_value(&self, name: &str) -> Result<Value> {
        Ok(self.profile(name)?.public.clone())
    }

    pub fn resolved_defaults_value(&self) -> Value {
        Value::Object(
            self.defaults
                .iter()
                .map(|(name, settings)| {
                    (
                        name.clone(),
                        defaults_value(
                            self.takes_approvals(name)
                                .then(|| self.approvals.unwrap_or(ApprovalsMode::Auto)),
                            settings,
                            is_acp_harness(name),
                        ),
                    )
                })
                .collect(),
        )
    }

    pub fn harness_spec(&self, name: &str) -> Result<HarnessSpec> {
        self.harnesses
            .get(name)
            .cloned()
            .or_else(|| builtin_harness(name))
            .with_context(|| format!("unknown Harness {name:?}; define it in [harnesses.{name}]"))
            .code(Code::InvalidArgument)
    }

    /// Every built-in and configured ACP Harness name.
    pub fn acp_harness_names(&self) -> Vec<String> {
        let mut names = BUILTIN_HARNESSES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        names.extend(self.harnesses.keys().cloned());
        names
            .into_iter()
            .filter(|name| is_acp_harness(name))
            .collect()
    }

    /// Whether `--approvals` and its defaults apply to this Harness.
    fn takes_approvals(&self, harness: &str) -> bool {
        !is_acp_harness(harness)
            || self
                .harness_spec(harness)
                .is_ok_and(|spec| !spec.approvals.is_empty())
    }

    /// The launch keys `[defaults.<harness>]` and a Profile accept.
    fn launch_keys(&self, harness: &str) -> Result<Vec<&'static str>> {
        if !is_builtin_harness(harness) && !self.harnesses.contains_key(harness) {
            bail!("Harness {harness:?} has no [harnesses.{harness}] command");
        }
        Ok(DEFAULT_HARNESS_KEYS
            .iter()
            .copied()
            .filter(|key| match *key {
                "approvals" => self.takes_approvals(harness),
                "harness_options" => !is_acp_harness(harness),
                _ => true,
            })
            .collect())
    }

    pub fn resolve(
        &self,
        harness: &str,
        profile: Option<&Profile>,
        explicit: &PartialLaunch,
        client_environment: &BTreeMap<String, String>,
    ) -> Result<ResolvedLaunch> {
        let acp = is_acp_harness(harness);
        let defaults = self.defaults_for(harness)?;
        let takes_approvals = self.takes_approvals(harness);
        let layers = [
            Some(defaults),
            profile.map(|profile| &profile.settings),
            Some(explicit),
        ];
        let layers = || layers.iter().copied().flatten();
        if acp && layers().any(|layer| !layer.harness_options.is_empty()) {
            return Err(coded(
                Code::Unsupported,
                "ACP Harnesses do not support harness_options",
            ));
        }
        if !takes_approvals && layers().any(|layer| layer.approvals.is_some()) {
            return Err(coded(
                Code::Unsupported,
                format!(
                    "Harness {harness:?} has no approvals mapping and does not support --approvals"
                ),
            ));
        }
        let mut resolved = ResolvedLaunch {
            approvals: takes_approvals.then(|| self.approvals.unwrap_or(ApprovalsMode::Auto)),
            harness_spec: acp.then(|| self.harness_spec(harness)).transpose()?,
            ..ResolvedLaunch::default()
        };
        for layer in layers() {
            overlay(&mut resolved, layer);
        }
        if let (Some(spec), Some(approvals)) = (&resolved.harness_spec, resolved.approvals)
            && !spec.approvals.contains_key(&approvals)
        {
            return Err(coded(
                Code::Unsupported,
                format!(
                    "Harness {harness:?} has no approvals mapping for {:?}",
                    approvals.as_str()
                ),
            ));
        }

        let clean_env = resolved.clean_env;
        let pass_env = collect_pass_env(defaults, profile, explicit);
        let unset_env = collect_unset_env(defaults, profile, explicit);
        let mut environment = if clean_env {
            BTreeMap::new()
        } else {
            client_environment.clone()
        };
        if clean_env {
            for key in pass_env {
                if let Some(value) = client_environment.get(&key) {
                    environment.insert(key, value.clone());
                }
            }
        }
        let mut assignments = defaults.env.clone();
        if let Some(profile) = profile {
            assignments.extend(
                profile
                    .settings
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        assignments.extend(
            explicit
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        environment.extend(assignments);
        for key in unset_env {
            environment.remove(&key);
        }
        resolved.environment = environment;
        Ok(resolved)
    }

    fn defaults_for(&self, harness: &str) -> Result<&PartialLaunch> {
        if !is_builtin_harness(harness) && !self.harnesses.contains_key(harness) {
            return Err(coded(
                Code::InvalidArgument,
                format!("unknown Harness {harness:?}; define it in [harnesses.{harness}]"),
            ));
        }
        self.defaults
            .get(harness)
            .map_or_else(|| self.defaults.get("codex"), Some)
            .context("built-in launch defaults are missing")
    }
}

pub(crate) fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let prefix = &text[..offset.min(text.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit('\n')
        .next()
        .map_or(1, |last| last.chars().count() + 1);
    (line, column)
}

fn collect_pass_env(
    defaults: &PartialLaunch,
    profile: Option<&Profile>,
    explicit: &PartialLaunch,
) -> Vec<String> {
    let mut values = defaults.pass_env.clone();
    if let Some(profile) = profile {
        values.extend(profile.settings.pass_env.iter().cloned());
    }
    values.extend(explicit.pass_env.iter().cloned());
    values
}

fn collect_unset_env(
    defaults: &PartialLaunch,
    profile: Option<&Profile>,
    explicit: &PartialLaunch,
) -> Vec<String> {
    let mut values = defaults.unset_env.clone();
    if let Some(profile) = profile {
        values.extend(profile.settings.unset_env.iter().cloned());
    }
    values.extend(explicit.unset_env.iter().cloned());
    values
}

fn overlay(resolved: &mut ResolvedLaunch, layer: &PartialLaunch) {
    if let Some(value) = &layer.model {
        resolved.model.clone_from(&Some(value.clone()));
    }
    if let Some(value) = &layer.effort {
        resolved.effort.clone_from(&Some(value.clone()));
    }
    if let Some(value) = layer.approvals {
        resolved.approvals = Some(value);
    }
    if let Some(value) = layer.clean_env {
        resolved.clean_env = value;
    }
    resolved
        .harness_options
        .extend(layer.harness_options.iter().cloned());
}

/// `default_approvals` is `None` for a Harness that takes no approvals mode.
fn defaults_value(
    default_approvals: Option<ApprovalsMode>,
    settings: &PartialLaunch,
    acp: bool,
) -> Value {
    let mut value = Map::new();
    if let Some(approvals) = default_approvals {
        value.insert(
            "approvals".to_owned(),
            json!(settings.approvals.unwrap_or(approvals)),
        );
    }
    if let Some(model) = &settings.model {
        value.insert("model".to_owned(), json!(model));
    }
    if let Some(effort) = &settings.effort {
        value.insert("effort".to_owned(), json!(effort));
    }
    value.insert(
        "clean_env".to_owned(),
        json!(settings.clean_env.unwrap_or(false)),
    );
    value.insert("pass_env".to_owned(), json!(settings.pass_env));
    value.insert("unset_env".to_owned(), json!(settings.unset_env));
    if !acp {
        value.insert(
            "harness_options".to_owned(),
            json!(settings.harness_options),
        );
    }
    value.insert("env".to_owned(), redact_env(&settings.env));
    Value::Object(value)
}

fn parse_profile(config: &Config, name: &str, table: &dyn TableLike) -> Result<Profile> {
    let profile_name = format!("profiles.{name}");
    let harness = table
        .get("harness")
        .map(|item| {
            at_item(
                item,
                required_string(item, &format!("{profile_name}.harness")).and_then(|harness| {
                    validate_harness_name(&harness)?;
                    Ok(harness)
                }),
            )
        })
        .transpose()?;
    let mut keys = match harness.as_deref() {
        Some(harness) => at_item(
            table.get("harness").unwrap_or(&Item::None),
            config.launch_keys(harness),
        )?,
        None => DEFAULT_HARNESS_KEYS.to_vec(),
    };
    keys.push("harness");
    let settings = parse_settings(table, &profile_name, &keys)?;
    let mut public = Map::new();
    for (key, item) in table.iter() {
        let value = if key == "env" {
            redact_env(&parse_env(item, &format!("{profile_name}.env"))?)
        } else {
            item_to_json(item)
                .with_context(|| format!("unsupported value for {profile_name}.{key}"))?
        };
        public.insert(key.to_owned(), value);
    }
    Ok(Profile {
        harness,
        settings,
        public: Value::Object(public),
    })
}

fn parse_settings(
    table: &dyn TableLike,
    name: &str,
    allowed_keys: &[&str],
) -> Result<PartialLaunch> {
    let mut settings = PartialLaunch::default();
    for (key, item) in table.iter() {
        at_item(
            item,
            (|| {
                if !allowed_keys.contains(&key) {
                    bail!("unknown key {key:?} in [{name}]");
                }
                let key_name = format!("{name}.{key}");
                match key {
                    "model" => settings.model = Some(required_string(item, &key_name)?),
                    "effort" => settings.effort = Some(required_string(item, &key_name)?),
                    "approvals" => settings.approvals = Some(parse_approvals(item, &key_name)?),
                    "clean_env" => {
                        settings.clean_env = Some(
                            item.as_bool()
                                .with_context(|| format!("{key_name} must be a boolean"))?,
                        );
                    }
                    "env" => settings.env = parse_env(item, &key_name)?,
                    "pass_env" => settings.pass_env = string_array(item, &key_name)?,
                    "unset_env" => settings.unset_env = string_array(item, &key_name)?,
                    "harness_options" => settings.harness_options = string_array(item, &key_name)?,
                    "harness" => {}
                    _ => bail!("unknown key {key:?} in [{name}]"),
                }
                Ok(())
            })(),
        )?;
    }
    Ok(settings)
}

fn parse_approvals_mapping(item: &Item, name: &str) -> Result<BTreeMap<ApprovalsMode, String>> {
    let table = item
        .as_table_like()
        .with_context(|| format!("{name} must be a table of approvals mode to agent mode"))?;
    table
        .iter()
        .map(|(mode, value)| {
            at_item(
                value,
                (|| {
                    let mode = ApprovalsMode::parse(mode)
                        .with_context(|| format!("{name} keys must be auto, ask, or bypass"))?;
                    Ok((
                        mode,
                        required_string(value, &format!("{name}.{}", mode.as_str()))?,
                    ))
                })(),
            )
        })
        .collect()
}

fn parse_approvals(item: &Item, name: &str) -> Result<ApprovalsMode> {
    let value = required_string(item, name)?;
    ApprovalsMode::parse(&value).with_context(|| format!("{name} must be auto, ask, or bypass"))
}

fn required_string(item: &Item, name: &str) -> Result<String> {
    item.as_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} must be a string"))
}

fn string_array(item: &Item, name: &str) -> Result<Vec<String>> {
    item.as_array()
        .with_context(|| format!("{name} must be an array of strings"))?
        .iter()
        .map(|value| {
            at_span(
                value.span(),
                value
                    .as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("{name} entries must be strings")),
            )
        })
        .collect()
}

fn parse_env(item: &Item, name: &str) -> Result<BTreeMap<String, String>> {
    if let Some(table) = item.as_table_like() {
        return table
            .iter()
            .map(|(key, value)| {
                let text = at_item(
                    value,
                    value
                        .as_str()
                        .with_context(|| format!("{name}.{key} must be a string")),
                )?;
                Ok((key.to_owned(), text.to_owned()))
            })
            .collect();
    }
    if let Some(array) = item.as_array() {
        return array
            .iter()
            .map(|entry| {
                at_span(
                    entry.span(),
                    (|| {
                        let assignment = entry
                            .as_str()
                            .with_context(|| format!("{name} entries must be KEY=VALUE strings"))?;
                        let (key, value) = assignment
                            .split_once('=')
                            .with_context(|| format!("{name} entries must use KEY=VALUE"))?;
                        if key.is_empty() {
                            bail!("{name} keys must not be empty");
                        }
                        Ok((key.to_owned(), value.to_owned()))
                    })(),
                )
            })
            .collect();
    }
    Err(anyhow!(
        "{name} must be a table or an array of KEY=VALUE strings"
    ))
}

fn redact_env(values: &BTreeMap<String, String>) -> Value {
    Value::Object(
        values
            .keys()
            .map(|key| (key.clone(), json!("<redacted>")))
            .collect(),
    )
}

fn item_to_json(item: &Item) -> Result<Value> {
    if let Some(value) = item.as_str() {
        return Ok(json!(value));
    }
    if let Some(value) = item.as_bool() {
        return Ok(json!(value));
    }
    if let Some(array) = item.as_array() {
        return array
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(|value| json!(value))
                    .context("array entries must be strings")
            })
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    bail!("unsupported TOML value")
}

#[cfg(test)]
mod tests {
    use super::{PartialLaunch, auth_env_names};
    use crate::protocol::ApprovalsMode;
    use std::collections::BTreeMap;

    fn client_environment(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn launch_precedence_merges_environment_and_harness_options() -> anyhow::Result<()> {
        let config = super::parse(
            r#"
[defaults]
approvals = "ask"

[defaults.claude]
model = "defaults-model"
effort = "defaults-effort"
approvals = "bypass"
harness_options = ["defaults-option"]
pass_env = ["DEFAULT_PASS"]
unset_env = ["DEFAULT_UNSET"]
env = { LAYER = "defaults", DEFAULT_ONLY = "yes" }

[profiles.review]
harness = "claude"
model = "profile-model"
effort = "profile-effort"
approvals = "ask"
harness_options = ["profile-option"]
pass_env = ["PROFILE_PASS"]
unset_env = ["PROFILE_UNSET"]
env = ["LAYER=profile", "PROFILE_ONLY=yes"]
"#,
        )?;
        let profile = config.profile("review")?;
        let mut explicit = PartialLaunch {
            model: Some("cli-model".to_owned()),
            effort: Some("cli-effort".to_owned()),
            approvals: Some(ApprovalsMode::Auto),
            clean_env: Some(true),
            pass_env: vec!["CLI_PASS".to_owned()],
            unset_env: vec!["CLI_UNSET".to_owned()],
            harness_options: vec!["cli-option".to_owned()],
            ..PartialLaunch::default()
        };
        explicit.env.insert("LAYER".to_owned(), "cli".to_owned());
        let client_env = client_environment(&[
            ("DEFAULT_PASS", "default-pass"),
            ("PROFILE_PASS", "profile-pass"),
            ("CLI_PASS", "cli-pass"),
            ("DEFAULT_UNSET", "remove"),
            ("PROFILE_UNSET", "remove"),
            ("CLI_UNSET", "remove"),
        ]);
        let resolved = config.resolve("claude", Some(profile), &explicit, &client_env)?;
        assert_eq!(resolved.model.as_deref(), Some("cli-model"));
        assert_eq!(resolved.effort.as_deref(), Some("cli-effort"));
        assert_eq!(resolved.approvals, Some(ApprovalsMode::Auto));
        assert_eq!(
            resolved.harness_options,
            ["defaults-option", "profile-option", "cli-option"]
        );
        assert_eq!(
            resolved.environment.get("LAYER").map(String::as_str),
            Some("cli")
        );
        for (key, expected) in [
            ("DEFAULT_ONLY", "yes"),
            ("PROFILE_ONLY", "yes"),
            ("DEFAULT_PASS", "default-pass"),
            ("PROFILE_PASS", "profile-pass"),
            ("CLI_PASS", "cli-pass"),
        ] {
            assert_eq!(
                resolved.environment.get(key).map(String::as_str),
                Some(expected)
            );
        }
        for key in ["DEFAULT_UNSET", "PROFILE_UNSET", "CLI_UNSET"] {
            assert!(!resolved.environment.contains_key(key));
        }
        Ok(())
    }

    #[test]
    fn acp_harnesses_parse_defaults_and_resolve_commands() -> anyhow::Result<()> {
        let config = super::parse(
            r#"
[harnesses.opencode]
command = ["opencode", "acp"]
login = "opencode auth login"

[defaults.opencode]
model = "opencode/big-pickle"
effort = "high"
clean_env = true
env = { API_KEY = "hidden" }
"#,
        )?;
        let resolved = config.resolve(
            "opencode",
            None,
            &PartialLaunch::default(),
            &client_environment(&[("PATH", "/bin")]),
        )?;
        assert_eq!(resolved.model.as_deref(), Some("opencode/big-pickle"));
        let spec = resolved.harness_spec.unwrap_or_default();
        assert_eq!(spec.command, ["opencode", "acp"]);
        assert_eq!(spec.login.as_deref(), Some("opencode auth login"));
        assert_eq!(
            resolved.environment.get("API_KEY").map(String::as_str),
            Some("hidden")
        );
        Ok(())
    }

    fn error_code(result: anyhow::Result<super::ResolvedLaunch>) -> Option<crate::error::Code> {
        result.err().and_then(|error| crate::error::code_of(&error))
    }

    #[test]
    fn a_harness_without_an_approvals_mapping_rejects_approvals_and_harness_options()
    -> anyhow::Result<()> {
        for text in [
            "[defaults.opencode]\napprovals = \"ask\"",
            "[profiles.p]\nharness = \"opencode\"\napprovals = \"ask\"",
            "[profiles.p]\nharness = \"opencode\"\nharness_options = [\"x\"]",
            "[profiles.p]\nharness = \"agy\"\nharness_options = [\"x\"]",
        ] {
            assert!(super::parse(text).is_err(), "accepted {text}");
        }
        let config = super::parse("[defaults]\napprovals = \"ask\"")?;
        assert_eq!(
            config
                .resolve(
                    "opencode",
                    None,
                    &PartialLaunch::default(),
                    &BTreeMap::new()
                )?
                .approvals,
            None
        );
        let explicit = PartialLaunch {
            approvals: Some(ApprovalsMode::Ask),
            ..PartialLaunch::default()
        };
        assert_eq!(
            error_code(config.resolve("opencode", None, &explicit, &BTreeMap::new())),
            Some(crate::error::Code::Unsupported)
        );
        Ok(())
    }

    #[test]
    fn agy_replaces_gemini_as_a_built_in_harness() -> anyhow::Result<()> {
        let config = super::Config::default();
        let launch = config.resolve("agy", None, &PartialLaunch::default(), &BTreeMap::new())?;
        let spec = launch.harness_spec.unwrap_or_default();
        assert_eq!(spec.command, ["agy", "acp"]);
        assert_eq!(spec.login.as_deref(), Some("agy acp --login"));
        assert_eq!(
            spec.resume_command("cfa001bc-3ba3-4c63-b172-5f7f3a85bfd8")
                .as_deref(),
            Some("agy --conversation cfa001bc-3ba3-4c63-b172-5f7f3a85bfd8")
        );
        assert!(config.acp_harness_names().contains(&"agy".to_owned()));
        assert!(!config.acp_harness_names().contains(&"gemini".to_owned()));
        assert!(
            config
                .resolve("gemini", None, &PartialLaunch::default(), &BTreeMap::new())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn an_approvals_mapping_makes_approvals_apply_as_for_native_harnesses() -> anyhow::Result<()> {
        let resolve = |config: &super::Config, profile: Option<&str>, approvals| {
            let explicit = PartialLaunch {
                approvals,
                ..PartialLaunch::default()
            };
            let profile = profile.map(|name| config.profile(name)).transpose()?;
            config.resolve("agy", profile, &explicit, &BTreeMap::new())
        };
        let config = super::Config::default();
        let launch = resolve(&config, None, None)?;
        assert_eq!(launch.approvals, Some(ApprovalsMode::Auto));
        assert_eq!(
            launch.harness_spec.unwrap_or_default().approvals,
            BTreeMap::from([
                (ApprovalsMode::Auto, "auto".to_owned()),
                (ApprovalsMode::Ask, "default".to_owned()),
            ])
        );
        assert_eq!(
            resolve(&config, None, Some(ApprovalsMode::Ask))?.approvals,
            Some(ApprovalsMode::Ask)
        );
        assert_eq!(
            error_code(resolve(&config, None, Some(ApprovalsMode::Bypass))),
            Some(crate::error::Code::Unsupported)
        );

        let config = super::parse(
            "[defaults]\napprovals = \"ask\"\n[profiles.careful]\nharness = \"agy\"\napprovals = \"auto\"",
        )?;
        assert_eq!(
            resolve(&config, None, None)?.approvals,
            Some(ApprovalsMode::Ask)
        );
        assert_eq!(
            resolve(&config, Some("careful"), None)?.approvals,
            Some(ApprovalsMode::Auto)
        );
        assert_eq!(config.resolved_defaults_value()["agy"]["approvals"], "ask");
        Ok(())
    }

    #[test]
    fn a_configured_harness_carries_its_approvals_mapping_and_resume_command() -> anyhow::Result<()>
    {
        let config = super::parse(
            r#"
[harnesses.x]
command = ["x", "acp"]
approvals = { auto = "auto", ask = "default", bypass = "yolo" }
resume = ["x", "resume", "--session={id}"]

[defaults.x]
approvals = "bypass"

[harnesses.agy]
command = ["/opt/agy", "acp"]
"#,
        )?;
        let launch = config.resolve("x", None, &PartialLaunch::default(), &BTreeMap::new())?;
        assert_eq!(launch.approvals, Some(ApprovalsMode::Bypass));
        let spec = launch.harness_spec.unwrap_or_default();
        assert_eq!(
            spec.approvals
                .get(&ApprovalsMode::Bypass)
                .map(String::as_str),
            Some("yolo")
        );
        assert_eq!(
            spec.resume_command("ses_1").as_deref(),
            Some("x resume --session=ses_1")
        );
        assert_eq!(spec.login, None);

        let agy = config
            .resolve("agy", None, &PartialLaunch::default(), &BTreeMap::new())?
            .harness_spec
            .unwrap_or_default();
        assert_eq!(agy.command, ["/opt/agy", "acp"]);
        assert_eq!(agy.login.as_deref(), Some("agy acp --login"));
        assert_eq!(agy.approvals.len(), 2);
        Ok(())
    }

    #[test]
    fn a_harness_spec_survives_the_client_to_daemon_round_trip() -> anyhow::Result<()> {
        let spec = super::Config::default().harness_spec("agy")?;
        let wire = serde_json::to_value(&spec)?;
        assert_eq!(
            wire,
            serde_json::json!({
                "command": ["agy", "acp"],
                "login": "agy acp --login",
                "approvals": {"auto": "auto", "ask": "default"},
                "resume": ["agy", "--conversation", "{id}"],
            })
        );
        assert_eq!(
            serde_json::from_value::<crate::protocol::HarnessSpec>(wire)?,
            spec
        );
        Ok(())
    }

    #[test]
    fn invalid_entries_are_rejected_without_echoing_the_config_source() {
        for (text, location) in [
            ("[defaults.claude]\nunknown = true", "2:"),
            ("[defaults.mystery]\nmodel = \"x\"", "1:"),
            ("[profiles.p]\nharness = \"codex\"\nauth = \"any\"", "3:"),
            ("[harnesses.bad_name]\ncommand = [\"x\"]", "1:"),
            ("[harnesses.pi]\ncommand = []", "2:"),
            ("[harnesses.pi]\ncommand = [\"x\"]\nunknown = 1", "3:"),
            (
                "[harnesses.pi]\ncommand = [\"x\"]\napprovals = { yolo = \"y\" }",
                "3:",
            ),
            ("[harnesses.pi]\ncommand = [\"x\"]\nresume = []", "3:"),
            (
                "[profiles.p]\nharness = \"opencode\"\napprovals = \"ask\"",
                "3:",
            ),
            ("[profiles.p]\nharness = \"mystery\"", "2:"),
        ] {
            let Err(error) = super::parse(text) else {
                panic!("accepted invalid config: {text}")
            };
            let message = error.to_string();
            assert!(message.contains(&format!("at {location}")), "{message}");
            assert!(!message.contains("bad_name"), "source leaked: {message}");
        }
    }

    #[test]
    fn profile_and_defaults_output_redact_environment_values() -> anyhow::Result<()> {
        let config = super::parse(
            "[defaults]\napprovals = \"ask\"\n[defaults.claude]\nenv = { API_KEY = \"secret-value\" }\n[profiles.safe]\nharness = \"claude\"\nenv = { KEY = \"profile-secret\" }",
        )?;
        let rendered = format!(
            "{}{}",
            config.profiles_value(),
            config.resolved_defaults_value()
        );
        assert!(!rendered.contains("secret-value"));
        assert!(!rendered.contains("profile-secret"));
        assert!(rendered.contains("<redacted>"));
        assert_eq!(
            config.resolved_defaults_value()["claude"]["approvals"],
            "ask"
        );
        assert!(
            config.resolved_defaults_value()["opencode"]
                .get("approvals")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn auth_env_reports_only_whitelisted_nonempty_names_after_options() -> anyhow::Result<()> {
        let config = super::parse(
            "[defaults.opencode]\nclean_env = true\npass_env = [\"GOOD_API_KEY\"]\n[profiles.p]\nharness = \"opencode\"\nenv = { ALSO_API_KEY = \"profile-secret\", BAD = \"bad-secret\" }",
        )?;
        let profile = config.profile("p")?;
        let mut explicit = PartialLaunch::default();
        explicit
            .env
            .insert("GOOD_API_KEY".into(), "cli-secret".into());
        explicit.env.insert("EMPTY_API_KEY".into(), String::new());
        let client = client_environment(&[
            ("GOOD_API_KEY", "host-secret"),
            ("IGNORED_API_KEY", "ignored-secret"),
        ]);
        let launch = config.resolve("opencode", Some(profile), &explicit, &client)?;
        let names = auth_env_names("opencode", launch.environment.iter());
        assert_eq!(names, ["ALSO_API_KEY", "GOOD_API_KEY"]);
        let public = serde_json::to_string(&names)?;
        assert!(!public.contains("secret"));
        assert!(!public.contains("bad-secret"));
        assert!(!public.contains("ignored-secret"));
        Ok(())
    }

    #[test]
    fn auth_env_uses_builtin_whitelists() {
        let environment = client_environment(&[
            ("ANTHROPIC_API_KEY", "a"),
            ("AWS_BEARER_TOKEN_BEDROCK", "b"),
            ("OPENAI_API_KEY", "c"),
            ("CURSOR_API_KEY", "d"),
            ("XAI_API_KEY", "e"),
            ("GOOGLE_API_KEY", "f"),
            ("GOOGLE_GENAI_USE_VERTEXAI", "true"),
            ("RANDOM", "secret"),
            ("EMPTY_API_KEY", ""),
        ]);
        assert_eq!(
            auth_env_names("claude", environment.iter()),
            ["ANTHROPIC_API_KEY", "AWS_BEARER_TOKEN_BEDROCK"]
        );
        assert_eq!(
            auth_env_names("codex", environment.iter()),
            ["OPENAI_API_KEY"]
        );
        assert_eq!(
            auth_env_names("cursor", environment.iter()),
            ["CURSOR_API_KEY"]
        );
        assert_eq!(auth_env_names("grok", environment.iter()), ["XAI_API_KEY"]);
        assert_eq!(
            auth_env_names("agy", environment.iter()),
            ["GOOGLE_API_KEY"]
        );
    }
}
