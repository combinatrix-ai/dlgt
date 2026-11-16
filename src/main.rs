mod claude;
mod claude_models;
mod client;
mod codex;
mod codex_events;
mod config;
mod cursor;
mod daemon;
mod doctor;
mod error;
mod messages;
mod paths;
mod protocol;
mod provider;
mod reaper;
mod requests;
mod skill;
mod store;
mod update;
mod watch;

use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn main() {
    if let Err(error) = run() {
        let failure = error.downcast_ref::<client::RpcFailure>();
        let auth_mismatch = error.downcast_ref::<config::AuthMismatch>();
        let code = failure.map_or_else(
            || error::code_of(&error).map_or("INVALID_ARGUMENT", error::Code::as_str),
            |failure| failure.code.as_str(),
        );
        let message = failure.map_or_else(
            || auth_mismatch.map_or_else(|| format!("{error:#}"), std::string::ToString::to_string),
            |failure| failure.message.clone(),
        );
        let mut error_json = json!({"code":code,"message":message});
        if let Some(auth_mismatch) = auth_mismatch {
            error_json["hint"] = json!(auth_mismatch.hint());
        }
        if let Some(failure) = failure
            && let Some(error_object) = error_json.as_object_mut()
        {
            if let Some(session_id) = &failure.session_id {
                error_object.insert("session_id".to_owned(), json!(session_id));
            }
            if let Some(launch_id) = &failure.launch_id {
                error_object.insert("launch_id".to_owned(), json!(launch_id));
            }
            if let Some(correlation_id) = &failure.correlation_id {
                error_object.insert("correlation_id".to_owned(), json!(correlation_id));
            }
            if let Some(hint) = &failure.hint {
                error_object.insert("hint".to_owned(), json!(hint));
            }
            if let Some(session_state) = &failure.session_state {
                error_object.insert("session_state".to_owned(), json!(session_state));
            }
            if let Some(action) = &failure.action {
                error_object.insert("action".to_owned(), json!(action));
            }
            if let Some(request_ids) = &failure.request_ids {
                error_object.insert("request_ids".to_owned(), json!(request_ids));
            }
            if let Some(detail) = &failure.detail {
                error_object.insert("detail".to_owned(), json!(detail));
            }
        }
        let response = json!({"ok":false,"error":error_json});
        println!("{response}");
        std::process::exit(exit_status(code));
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return Ok(());
    };
    if command == "help" {
        return command_help(&args[1..]);
    }
    if has_help_flag(&args[1..]) {
        return print_command_usage(command);
    }
    match command {
        "server" => command_server(&args[1..]),
        "new" => command_new(&args[1..]),
        "restart" => command_restart(&args[1..]),
        "send" => command_send(&args[1..]),
        "fetch" => command_fetch(&args[1..]),
        "answer" => command_answer(&args[1..]),
        "cancel" => command_cancel(&args[1..]),
        "list" | "ls" => command_list(&args[1..]),
        "show" => command_show(&args[1..]),
        "watch" => command_watch(&args[1..]),
        "stop" => command_stop(&args[1..]),
        "models" => command_models(&args[1..]),
        "profiles" => command_profiles(&args[1..]),
        "doctor" => command_doctor(&args[1..]),
        "rpc" => command_rpc(&args[1..]),
        "update" => command_update(&args[1..]),
        "skill" => {
            print!("{}", skill::TEXT);
            Ok(())
        }
        "--version" | "-V" | "version" => {
            print_success(json!({"version":env!("CARGO_PKG_VERSION")}), false)
        }
        "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => bail!("unknown command {command:?}; run `dlgt help`"),
    }
}

fn command_help(args: &[String]) -> Result<()> {
    match args {
        [] => {
            print_usage();
            Ok(())
        }
        [flag] if matches!(flag.as_str(), "--help" | "-h") => print_command_usage("help"),
        [command] => print_command_usage(command),
        _ => bail!("help accepts at most one command"),
    }
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
}

fn command_server(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|value| value == "stop") {
        return print_success(client::call_existing("server.stop", json!({}))?, false);
    }
    if args.first().is_some_and(|value| value == "--reaper") {
        return reaper::run();
    }
    let parsed = Args::parse("server", args, &["--foreground", "--daemon-child"], &[])?;
    parsed.no_positionals()?;
    daemon::run()
}

fn command_new(args: &[String]) -> Result<()> {
    let options = acceptance_options();
    let parsed = Args::parse(
        "new",
        args,
        &["--stdin", "--pretty", "--clean-env"],
        &options,
    )?;
    let title = parsed.required("--title")?;
    // Validate caller-supplied idempotency before reading mutable config or
    // prompting for input.
    let request_id = require_request_id("new", &parsed)?;
    let configuration = config::load()?;
    let profile = parsed
        .one("--profile")
        .map(|name| configuration.profile(name).cloned())
        .transpose()?;
    let harness = parsed
        .one("--harness")
        .or_else(|| profile.as_ref().and_then(|value| value.harness.as_deref()))
        .context("missing --harness or profile harness")?;
    let explicit = explicit_launch(&parsed)?;
    let client_environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    let launch =
        configuration.resolve(harness, profile.as_ref(), &explicit, &client_environment)?;
    if harness == "claude" {
        config::validate_auth_environment(launch.auth, launch.environment.iter())?;
    }
    let cwd = launch_cwd(&parsed)?;
    let prompt = prompt_from(&parsed, 0)?
        .context("missing initial prompt; use --prompt-file, --stdin, or -- PROMPT")?;
    let result = client::call(
        "session.create",
        json!({
            "title": title,
            "request_id": request_id,
            "alias": parsed.one("--alias"),
            "harness": harness,
            "cwd": cwd,
            "model": launch.model,
            "effort": launch.effort,
            "harness_options": launch.harness_options,
            "approvals": launch.approvals.as_str(),
            "auth_policy": (harness == "claude").then_some(launch.auth.as_str()),
            "prompt": prompt,
            "startup_timeout_ms": parsed.one("--startup-timeout")
                .map(parse_duration).transpose()?.unwrap_or(Duration::from_mins(1)).as_millis(),
            "environment": launch.environment,
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

/// The approvals mode named on the command line, validated before any RPC.
fn approvals_option(parsed: &Args) -> Result<Option<&str>> {
    let Some(mode) = parsed.one("--approvals") else {
        return Ok(None);
    };
    protocol::ApprovalsMode::parse(mode)?;
    Ok(Some(mode))
}

fn auth_option(parsed: &Args) -> Result<Option<protocol::AuthPolicy>> {
    parsed
        .one("--auth")
        .map(protocol::AuthPolicy::parse)
        .transpose()
}

fn explicit_launch(parsed: &Args) -> Result<config::PartialLaunch> {
    let mut launch = config::PartialLaunch {
        model: parsed.one("--model").map(str::to_owned),
        effort: parsed.one("--effort").map(str::to_owned),
        approvals: approvals_option(parsed)?
            .map(protocol::ApprovalsMode::parse)
            .transpose()?,
        auth: auth_option(parsed)?,
        clean_env: parsed.flag("--clean-env").then_some(true),
        pass_env: parsed.many("--pass-env").map(str::to_owned).collect(),
        unset_env: parsed.many("--unset-env").map(str::to_owned).collect(),
        harness_options: parsed.many("--harness-option").map(str::to_owned).collect(),
        ..config::PartialLaunch::default()
    };
    for assignment in parsed.many("--env") {
        let (key, value) = assignment
            .split_once('=')
            .context("--env requires KEY=VALUE")?;
        if key.is_empty() {
            bail!("--env key must not be empty");
        }
        launch.env.insert(key.to_owned(), value.to_owned());
    }
    Ok(launch)
}

fn command_send(args: &[String]) -> Result<()> {
    let options = acceptance_options();
    let parsed = Args::parse(
        "send",
        args,
        &["--stdin", "--pretty", "--resume", "--steer", "--clean-env"],
        &options,
    )?;
    let session = parsed
        .positionals
        .first()
        .context("missing Session selector")?;
    let request_id = require_request_id("send", &parsed)?;
    if parsed.one("--harness").is_some() {
        bail!("--harness is derived from the provider-qualified resume selector");
    }
    if parsed.flag("--steer") && parsed.flag("--resume") {
        bail!("--steer and --resume are mutually exclusive");
    }
    if !parsed.flag("--resume")
        && let Some(option) = LAUNCH_OPTIONS
            .iter()
            .find(|option| **option != "--request-id" && parsed.one(option).is_some())
            .copied()
            .or_else(|| parsed.flag("--clean-env").then_some("--clean-env"))
    {
        bail!("{option} is accepted only with --resume");
    }
    let correlation_id = format!("corr_{}", Uuid::new_v4().simple());
    let mut params = json!({
        "session":session,
        "correlation_id":correlation_id,
        "request_id": request_id,
        "resume": parsed.flag("--resume"),
        "steer": parsed.flag("--steer"),
    });
    if parsed.flag("--resume") {
        let (harness, _) = session
            .split_once(':')
            .filter(|(harness, id)| matches!(*harness, "claude" | "codex") && !id.is_empty())
            .context("--resume requires a provider-qualified codex:<id> or claude:<id> selector")?;
        apply_resume_launch_options(&parsed, &mut params, harness)?;
    } else {
        let prompt = prompt_from(&parsed, 1)?
            .context("missing prompt; use --prompt-file, --stdin, or -- PROMPT")?;
        params["prompt"] = json!(prompt);
    }
    let route = client::find_live_session(session)?;
    if let Some(route) = &route {
        params["session"] = json!(route.session_id);
    }
    let result = if let Some(route) = &route {
        client::call_socket(&route.socket, "session.send", params)?
    } else {
        client::call("session.send", params)?
    };
    print_success(result, parsed.flag("--pretty"))
}

fn apply_resume_launch_options(parsed: &Args, params: &mut Value, harness: &str) -> Result<()> {
    let configuration = config::load()?;
    let profile = parsed
        .one("--profile")
        .map(|name| configuration.profile(name).cloned())
        .transpose()?;
    let profile_values = parsed
        .one("--profile")
        .map(|name| configuration.profile_value(name))
        .transpose()?
        .unwrap_or(Value::Null);
    if let Some(profile_harness) = profile
        .as_ref()
        .and_then(|profile| profile.harness.as_deref())
        && profile_harness != harness
    {
        bail!(
            "Profile harness {profile_harness:?} does not match resume Session harness {harness:?}"
        );
    }
    let explicit = explicit_launch(parsed)?;
    let client_environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    let launch =
        configuration.resolve(harness, profile.as_ref(), &explicit, &client_environment)?;
    let prompt = prompt_from(parsed, 1)?
        .context("missing prompt; use --prompt-file, --stdin, or -- PROMPT")?;
    let object = params.as_object_mut().context("invalid send parameters")?;
    if parsed.one("--cwd").is_some() {
        object.insert("cwd".to_owned(), json!(launch_cwd(parsed)?));
    }
    object.insert("prompt".to_owned(), json!(prompt));
    let profile_has = |name: &str| profile_values.get(name).is_some();
    if parsed.one("--model").is_some() || profile_has("model") {
        object.insert("model".to_owned(), json!(launch.model));
    }
    if parsed.one("--effort").is_some() || profile_has("effort") {
        object.insert("effort".to_owned(), json!(launch.effort));
    }
    if parsed.many("--harness-option").next().is_some() || profile_has("harness_options") {
        let profile_options = profile_values
            .get("harness_options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned);
        let options = profile_options
            .chain(parsed.many("--harness-option").map(str::to_owned))
            .collect::<Vec<_>>();
        object.insert("harness_options".to_owned(), json!(options));
    }
    if approvals_option(parsed)?.is_some() || profile_has("approvals") {
        object.insert("approvals".to_owned(), json!(launch.approvals.as_str()));
    }
    if harness == "claude" && (auth_option(parsed)?.is_some() || profile_has("auth")) {
        object.insert("auth_policy".to_owned(), json!(launch.auth.as_str()));
    }
    object.insert(
        "startup_timeout_ms".to_owned(),
        json!(
            parsed
                .one("--startup-timeout")
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_mins(1))
                .as_millis()
        ),
    );
    object.insert("environment".to_owned(), json!(launch.environment));
    Ok(())
}

fn command_restart(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "restart",
        args,
        &["--pretty", "--clean-env"],
        LAUNCH_OPTIONS,
    )?;
    let session = parsed.one_positional("Session selector")?;
    if parsed.one("--auth").is_some() {
        bail!("--auth is accepted only with new or send --resume");
    }
    let environment = launch_environment(&parsed)?;
    let result = client::call(
        "session.restart",
        json!({
            "session": session,
            "startup_timeout_ms": parsed.one("--startup-timeout")
                .map(parse_duration).transpose()?.unwrap_or(Duration::from_mins(1)).as_millis(),
            "environment": environment,
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_fetch(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "fetch",
        args,
        &["--messages", "--no-messages", "--pretty"],
        &["--cursor", "--wait", "--messages", "--max-bytes"],
    )?;
    let session = parsed.one_positional("Session selector")?;
    if parsed.flag("--no-messages") && parsed.flag("--messages") {
        bail!("--messages and --no-messages are mutually exclusive");
    }
    let messages = if parsed.flag("--no-messages") {
        json!(false)
    } else if let Some(count) = parsed.one("--messages") {
        json!(
            count
                .parse::<u64>()
                .ok()
                .filter(|count| *count > 0)
                .context("invalid --messages count")?
        )
    } else {
        Value::Null
    };
    let params = json!({
        "session": session,
        "cursor": parsed.one("--cursor"),
        "wait_ms": parsed.one("--wait").map(parse_duration).transpose()?.map(duration_ms),
        "messages": messages,
        "max_bytes": parsed.one("--max-bytes").map(str::parse::<u64>).transpose()
            .context("invalid --max-bytes")?,
    });
    let result = match client::find_live_session(session)? {
        Some(route) => client::call_socket(&route.socket, "session.fetch", params)?,
        None => client::call("session.fetch", params)?,
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_answer(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "answer",
        args,
        &["--allow", "--deny", "--pretty"],
        &["--message", "--text", "--choice", "--json"],
    )?;
    let [session, request] = parsed.positionals.as_slice() else {
        bail!("expected a Session selector and a Request ID");
    };
    let choices = [
        parsed.flag("--allow"),
        parsed.flag("--deny"),
        parsed.one("--text").is_some(),
        parsed.one("--choice").is_some(),
        parsed.one("--json").is_some(),
    ];
    if choices.iter().filter(|chosen| **chosen).count() != 1 {
        bail!("answer requires exactly one of --allow, --deny, --text, --choice, or --json");
    }
    if parsed.one("--message").is_some() && !parsed.flag("--deny") {
        bail!("--message is accepted only with --deny");
    }
    let mut params = json!({"session": session, "request": request});
    if parsed.flag("--allow") {
        params["decision"] = json!("allow");
    } else if parsed.flag("--deny") {
        params["decision"] = json!("deny");
        params["message"] = json!(parsed.one("--message"));
    } else if let Some(text) = parsed.one("--text") {
        params["decision"] = json!("text");
        params["text"] = json!(text);
    } else if let Some(choice) = parsed.one("--choice") {
        params["decision"] = json!("choice");
        params["choice"] = json!(
            choice
                .parse::<u64>()
                .ok()
                .filter(|choice| *choice > 0)
                .context("invalid --choice; use a 1-based number")?
        );
    } else if let Some(json_text) = parsed.one("--json") {
        let value: Value = serde_json::from_str(json_text).context("invalid --json value")?;
        if !value.is_object() {
            bail!("--json requires a JSON object");
        }
        params["decision"] = json!("json");
        params["json"] = value;
    }
    let result = match client::find_live_session(session)? {
        Some(route) => {
            params["session"] = json!(route.session_id);
            client::call_socket(&route.socket, "session.answer", params)?
        }
        None => client::call_existing("session.answer", params)?,
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_cancel(args: &[String]) -> Result<()> {
    let parsed = Args::parse("cancel", args, &["--pretty"], &["--timeout"])?;
    let session = parsed.one_positional("Session selector")?;
    let timeout = parsed
        .one("--timeout")
        .map(parse_duration)
        .transpose()?
        .unwrap_or(Duration::from_secs(30));
    let result = client::call(
        "session.cancel",
        json!({"session":session,"timeout_ms":duration_ms(timeout)}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_list(args: &[String]) -> Result<()> {
    let parsed = Args::parse("list", args, &["--all", "--all-versions", "--pretty"], &[])?;
    parsed.no_positionals()?;
    let sessions = if parsed.flag("--all-versions") {
        client::list_all_versions(parsed.flag("--all"))?
    } else {
        client::call("session.list", json!({"all":parsed.flag("--all")}))?
            .as_array()
            .context("invalid session list response")?
            .clone()
    };
    print_success(json!({"sessions":sessions}), parsed.flag("--pretty"))
}

fn command_show(args: &[String]) -> Result<()> {
    let parsed = Args::parse("show", args, &["--pretty"], &[])?;
    let result = client::call(
        "session.read",
        json!({"session":parsed.one_positional("Session selector")?}),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_watch(args: &[String]) -> Result<()> {
    let parsed = Args::parse("watch", args, &["--from-start", "--until-idle"], &[])?;
    watch::run(
        parsed.one_positional("Session selector")?,
        parsed.flag("--from-start"),
        parsed.flag("--until-idle"),
    )
}

fn command_stop(args: &[String]) -> Result<()> {
    let parsed = Args::parse("stop", args, &["--force", "--pretty"], &[])?;
    let result = client::call(
        "session.stop",
        json!({
            "session":parsed.one_positional("Session selector")?, "force":parsed.flag("--force")
        }),
    )?;
    print_success(result, parsed.flag("--pretty"))
}

fn command_models(args: &[String]) -> Result<()> {
    let parsed = Args::parse(
        "models",
        args,
        &["--include-hidden", "--pretty"],
        &["--harness"],
    )?;
    parsed.no_positionals()?;
    let include_hidden = parsed.flag("--include-hidden");
    if let Some(harness) = parsed.one("--harness") {
        let result = client::call(
            "model.list",
            json!({"harness":harness,"include_hidden":include_hidden}),
        )?;
        return print_success(result, parsed.flag("--pretty"));
    }
    // Bare `dlgt models` is a discovery request, not an assertion that every
    // Harness is reachable: one unavailable provider must not hide the other.
    let harnesses = ["codex", "claude"]
        .into_iter()
        .map(|harness| {
            client::call(
                "model.list",
                json!({"harness":harness,"include_hidden":include_hidden}),
            )
            .unwrap_or_else(|error| {
                json!({
                    "harness": harness,
                    "discovery": "unavailable",
                    "models": [],
                    "error": format!("{error:#}"),
                })
            })
        })
        .collect::<Vec<_>>();
    print_success(json!({"harnesses":harnesses}), parsed.flag("--pretty"))
}

fn command_profiles(args: &[String]) -> Result<()> {
    let parsed = Args::parse("profiles", args, &["--pretty"], &[])?;
    let action = parsed.positionals.first().map_or("list", String::as_str);
    let configuration = config::load()?;
    let result = match action {
        // Bare `dlgt profiles` is the same request as `profiles list`.
        "list" if parsed.positionals.len() <= 1 => {
            json!({"profiles":configuration.profiles_value()})
        }
        "show" if parsed.positionals.len() == 2 => {
            let name = &parsed.positionals[1];
            json!({"name":name,"profile":configuration.profile_value(name)?})
        }
        "defaults" if parsed.positionals.len() == 1 => {
            json!({"defaults":configuration.resolved_defaults_value()})
        }
        _ => bail!("usage: dlgt profiles list | profiles show NAME | profiles defaults"),
    };
    print_success(result, parsed.flag("--pretty"))
}

fn command_doctor(args: &[String]) -> Result<()> {
    let parsed = Args::parse("doctor", args, &["--json", "--probe"], &[])?;
    parsed.no_positionals()?;
    let report = doctor::run(parsed.flag("--probe"));
    if parsed.flag("--json") {
        print_success(report.as_json(), false)
    } else {
        print!("{}", doctor::human(&report));
        Ok(())
    }
}

fn command_rpc(args: &[String]) -> Result<()> {
    let parsed = Args::parse("rpc", args, &["--stdio"], &[])?;
    parsed.no_positionals()?;
    if !parsed.flag("--stdio") {
        bail!("rpc requires --stdio");
    }
    client::rpc_stdio()
}

fn command_update(args: &[String]) -> Result<()> {
    let parsed = Args::parse("update", args, &["--pretty"], &[])?;
    parsed.no_positionals()?;
    print_success(update::install_latest()?, parsed.flag("--pretty"))
}

/// Options accepted by the Session launch commands. `new` and `send` share
/// this set so a Profile-driven launch and a `--resume` launch stay aligned.
const LAUNCH_OPTIONS: &[&str] = &[
    "--title",
    "--alias",
    "--profile",
    "--harness",
    "--model",
    "--effort",
    "--cwd",
    "--approvals",
    "--auth",
    "--harness-option",
    "--startup-timeout",
    "--request-id",
    "--pass-env",
    "--env",
    "--unset-env",
];

fn acceptance_options() -> Vec<&'static str> {
    LAUNCH_OPTIONS
        .iter()
        .copied()
        .chain(["--prompt-file"])
        .collect()
}

#[derive(Default)]
struct Args {
    positionals: Vec<String>,
    options: HashMap<String, Vec<String>>,
    flags: HashSet<String>,
}

impl Args {
    /// Parse one command's arguments against its declared flags and options.
    ///
    /// An unrecognized long option is rejected by name instead of silently
    /// consuming the next token as its value, which used to turn a typo into
    /// an unrelated "missing value for --json" style failure.
    fn parse(command: &str, args: &[String], flags: &[&str], options: &[&str]) -> Result<Self> {
        let known_flags = flags.iter().copied().collect::<HashSet<_>>();
        let known_options = options.iter().copied().collect::<HashSet<_>>();
        let mut parsed = Self::default();
        let mut index = 0;
        let mut positional = false;
        while index < args.len() {
            let value = &args[index];
            if positional {
                parsed.positionals.push(value.clone());
            } else if value == "--" {
                positional = true;
            } else if let Some((name, inline)) = value
                .starts_with("--")
                .then(|| value.split_once('='))
                .flatten()
            {
                if !known_flags.contains(name) && !known_options.contains(name) {
                    return Err(unknown_option(command, name));
                }
                if known_flags.contains(name) {
                    parsed.flags.insert(name.to_owned());
                }
                parsed
                    .options
                    .entry(name.to_owned())
                    .or_default()
                    .push(inline.to_owned());
            } else if value.starts_with("--") {
                if known_flags.contains(value.as_str()) {
                    parsed.flags.insert(value.clone());
                } else if known_options.contains(value.as_str()) {
                    index += 1;
                    let option = args
                        .get(index)
                        .with_context(|| format!("missing value for {value}"))?;
                    parsed
                        .options
                        .entry(value.clone())
                        .or_default()
                        .push(option.clone());
                } else {
                    return Err(unknown_option(command, value));
                }
            } else {
                parsed.positionals.push(value.clone());
            }
            index += 1;
        }
        Ok(parsed)
    }
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
    fn one(&self, name: &str) -> Option<&str> {
        self.options
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }
    fn many(&self, name: &str) -> impl Iterator<Item = &str> {
        self.options
            .get(name)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }
    fn required(&self, name: &str) -> Result<&str> {
        self.one(name)
            .with_context(|| format!("missing required option {name}"))
    }
    fn one_positional(&self, label: &str) -> Result<&str> {
        if self.positionals.len() != 1 {
            bail!("expected exactly one {label}");
        }
        Ok(&self.positionals[0])
    }
    fn no_positionals(&self) -> Result<()> {
        if self.positionals.is_empty() {
            Ok(())
        } else {
            bail!(
                "unexpected positional arguments: {}",
                self.positionals.join(" ")
            )
        }
    }
}

/// Acceptance idempotency only works when the key exists before the first
/// attempt, so it cannot be something a caller remembers to add afterwards.
fn require_request_id<'a>(command: &str, parsed: &'a Args) -> Result<&'a str> {
    let Some(request_id) = parsed.one("--request-id").filter(|id| !id.is_empty()) else {
        return Err(request_id_error(
            command,
            "missing required option --request-id; every acceptance needs an idempotency key \
so a lost response can be retried without creating a second Session",
        ));
    };
    if request_id.len() > crate::protocol::MAX_ACCEPTANCE_REQUEST_ID_LEN {
        return Err(request_id_error(
            command,
            &format!(
                "invalid --request-id: must be at most {} bytes",
                crate::protocol::MAX_ACCEPTANCE_REQUEST_ID_LEN
            ),
        ));
    }
    Ok(request_id)
}

fn request_id_error(command: &str, reason: &str) -> anyhow::Error {
    command_usage(command).map_or_else(
        |_| anyhow::anyhow!("{reason}"),
        |usage| anyhow::anyhow!("{reason}\n\n{usage}"),
    )
}

fn unknown_option(command: &str, name: &str) -> anyhow::Error {
    command_usage(command).map_or_else(
        |_| anyhow::anyhow!("unknown option {name:?}"),
        |usage| anyhow::anyhow!("unknown option {name:?}\n\n{usage}"),
    )
}

fn prompt_from(parsed: &Args, skip: usize) -> Result<Option<String>> {
    let prompt_file = parsed.one("--prompt-file");
    let has_positional_prompt = parsed.positionals.len() > skip;
    let source_count = usize::from(parsed.flag("--stdin"))
        + usize::from(prompt_file.is_some())
        + usize::from(has_positional_prompt);
    if source_count > 1 {
        bail!("--prompt-file, --stdin, and positional prompt are mutually exclusive");
    }
    if let Some(path) = prompt_file {
        let path = Path::new(path);
        return std::fs::read_to_string(path)
            .with_context(|| format!("failed to read prompt file {}", path.display()))
            .map(Some);
    }
    if parsed.flag("--stdin") {
        let mut prompt = String::new();
        io::stdin().read_to_string(&mut prompt)?;
        return Ok(Some(prompt));
    }
    Ok(has_positional_prompt.then(|| parsed.positionals[skip..].join(" ")))
}

fn parse_duration(value: &str) -> Result<Duration> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split].parse::<u64>().context("invalid duration")?;
    if number == 0 {
        bail!("duration must be positive");
    }
    match &value[split..] {
        "ms" => Ok(Duration::from_millis(number)),
        "" | "s" => Ok(Duration::from_secs(number)),
        "m" => Ok(Duration::from_secs(number.saturating_mul(60))),
        "h" => Ok(Duration::from_secs(number.saturating_mul(3600))),
        unit => bail!("invalid duration unit {unit:?}; use ms, s, m, or h"),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn launch_environment(parsed: &Args) -> Result<Map<String, Value>> {
    let clean = parsed.flag("--clean-env");
    let mut values = if clean {
        HashMap::new()
    } else {
        std::env::vars().collect()
    };
    if clean {
        for key in parsed.many("--pass-env") {
            if let Ok(value) = std::env::var(key) {
                values.insert(key.to_owned(), value);
            }
        }
    }
    for assignment in parsed.many("--env") {
        let (key, value) = assignment
            .split_once('=')
            .context("--env requires KEY=VALUE")?;
        values.insert(key.to_owned(), value.to_owned());
    }
    for key in parsed.many("--unset-env") {
        values.remove(key);
    }
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect())
}

fn launch_cwd(parsed: &Args) -> Result<String> {
    let invocation_dir =
        std::env::current_dir().context("cannot determine the current directory")?;
    resolve_launch_cwd(parsed.one("--cwd"), &invocation_dir)
}

fn resolve_launch_cwd(argument: Option<&str>, invocation_dir: &Path) -> Result<String> {
    let Some(argument) = argument else {
        return Ok(invocation_dir.to_string_lossy().into_owned());
    };
    let resolved = invocation_dir
        .join(argument)
        .canonicalize()
        .with_context(|| format!("--cwd {argument:?} does not exist"))?;
    Ok(resolved.to_string_lossy().into_owned())
}

fn print_success(value: Value, pretty: bool) -> Result<()> {
    let mut object = match value {
        Value::Object(object) => object,
        other => Map::from_iter([("value".to_owned(), other)]),
    };
    object.insert("ok".to_owned(), Value::Bool(true));
    if let Some(info) = client::take_info() {
        object.insert("info".to_owned(), info);
    }
    let value = Value::Object(object);
    println!(
        "{}",
        if pretty {
            serde_json::to_string_pretty(&value)?
        } else {
            serde_json::to_string(&value)?
        }
    );
    Ok(())
}

fn exit_status(code: &str) -> i32 {
    match code {
        "CANCEL_TIMEOUT" => 3,
        "SESSION_BLOCKED" => 4,
        "SESSION_BUSY" => 5,
        _ => 1,
    }
}

fn print_usage() {
    println!(
        "dlgt - local subagent runtime\n\nUSAGE\n  dlgt <COMMAND> [OPTIONS]\n\nDELEGATION\n  new          Create a Session with its first prompt\n  send         Send work to an idle Session, --steer a busy one, or --resume\n  fetch        Read new state, results, events, and messages from a cursor\n  answer       Answer a pending approval or question\n  cancel       Interrupt the active execution\n\nSESSIONS\n  list, ls     List Sessions\n  show         Show Session state and latest result\n  watch        Follow a Session's messages as text (read-only)\n  restart      Restart a Session\n  stop         Stop the Session\n\nCONFIGURATION\n  models       Discover Harness models\n  profiles     List Profiles and show launch defaults\n  doctor       Diagnose the local dlgt installation\n  skill        Print the embedded dlgt skill\n\nRUNTIME\n  server       Run or stop the daemon\n  update       Install the latest release and embedded Skills\n  rpc          Use JSONL RPC"
    );
}

fn print_command_usage(command: &str) -> Result<()> {
    println!("{}", command_usage(command)?);
    Ok(())
}

fn command_usage(command: &str) -> Result<&'static str> {
    let usage = match command {
        "server" => {
            "dlgt server - run or stop the local daemon\n\nUSAGE\n  dlgt server [--foreground]\n  dlgt server stop\n\nOPTIONS\n  --foreground   Run in the foreground\n  -h, --help     Print this help"
        }
        "update" => {
            "dlgt update - install the latest release and embedded Skills\n\nUSAGE\n  dlgt update [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "new" => {
            "dlgt new - create a Session and submit its first prompt\n\nUSAGE\n  dlgt new --title <TITLE> --request-id <ID> [OPTIONS] -- <PROMPT>\n  dlgt new --title <TITLE> --request-id <ID> [OPTIONS] --prompt-file <PATH>\n  dlgt new --title <TITLE> --request-id <ID> [OPTIONS] --stdin\n\nOPTIONS\n  --title <TITLE>                 Human-readable Session title (required)\n  --alias <@ALIAS>               Exact active Session alias\n  --profile <PROFILE>            Reusable launch Profile\n  --harness <codex|claude>       Provider Harness (required without a Profile)\n  --model <MODEL>                Provider model\n  --effort <LEVEL>               Provider reasoning effort\n  --cwd <DIR>                    Working directory (default: current directory)\n  --approvals <auto|ask|bypass>  Who approves what the worker does (default: auto)\n  --auth <subscription|any>      Claude authentication policy (default: subscription)\n  --harness-option <KEY[=VALUE]> Harness option (repeatable)\n  --startup-timeout <DURATION>   Startup timeout (default: 60s)\n  --clean-env                    Start with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --request-id <ID>              Idempotency key (required); a retry replays the receipt\n  --prompt-file <PATH>           Read the required prompt from a file\n  --stdin                        Read the required prompt from stdin\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help\n\nAPPROVALS\n  auto     The provider's own reviewer decides; only what it does not settle is asked\n  ask      Every approval becomes a Request for `dlgt answer`\n  bypass   Nothing is asked and the provider sandbox is off"
        }
        "restart" => {
            "dlgt restart - replace a Session process and resume its provider conversation\n\nUSAGE\n  dlgt restart <SESSION_ID> [OPTIONS]\n\nOPTIONS\n  --startup-timeout <DURATION>   Startup timeout (default: 60s)\n  --clean-env                    Start with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help"
        }
        "send" => {
            "dlgt send - send work to an idle Session, steer a busy one, or resume a provider conversation\n\nUSAGE\n  dlgt send <SESSION_ID|@ALIAS> --request-id <ID> [OPTIONS] -- <PROMPT>\n  dlgt send <SESSION_ID|@ALIAS> --request-id <ID> [OPTIONS] --prompt-file <PATH>\n  dlgt send <SESSION_ID|@ALIAS> --steer --request-id <ID> -- <PROMPT>\n  dlgt send <codex:ID|claude:ID> --resume --request-id <ID> [OPTIONS] -- <PROMPT>\n\nOPTIONS\n  --steer                        Add the prompt to the execution that is already running\n  --resume                       Resume a stopped provider conversation\n  --profile <PROFILE>            Reusable launch Profile for resume\n  --model <MODEL>                Model override for resume\n  --effort <LEVEL>               Reasoning effort override for resume\n  --cwd <DIR>                    Working directory for resume (default: current directory)\n  --approvals <auto|ask|bypass>  Approvals mode for resume (default: auto)\n  --auth <subscription|any>      Claude authentication policy for resume (default: subscription)\n  --harness-option <KEY[=VALUE]> Harness option for resume (repeatable)\n  --startup-timeout <DURATION>   Resume startup timeout (default: 60s)\n  --clean-env                    Resume with an empty environment\n  --pass-env <KEY>               Pass a host variable with --clean-env (repeatable)\n  --env <KEY=VALUE>              Set an environment variable (repeatable)\n  --unset-env <KEY>              Remove an environment variable (repeatable)\n  --request-id <ID>              Idempotency key (required); a retry replays the receipt\n  --prompt-file <PATH>           Read the required prompt from a file\n  --stdin                        Read the required prompt from stdin\n  --pretty                       Pretty-print JSON output\n  -h, --help                     Print this help\n\nA busy Session rejects a plain send with SESSION_BUSY; --steer needs a busy\nSession and never creates an execution. Launch options need --resume."
        }
        "fetch" => {
            "dlgt fetch - read one Session since a cursor in one call\n\nUSAGE\n  dlgt fetch <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --cursor <N>            Observation position from a previous response\n  --wait <DURATION>       Wait for the active/latest execution's terminal result (max 24h)\n  --messages[=<MAX>]      Messages per response (default: 64, max: 512)\n  --no-messages           Omit messages\n  --max-bytes <BYTES>     Serialized response budget (default: 32768, max: 262144)\n  --pretty                Pretty-print JSON output\n  -h, --help              Print this help\n\nEvery observation exits 0. Omit --cursor to recover a bounded baseline.\nA pending Request returns at once with reason \"blocked\"; answer it with dlgt answer."
        }
        "cancel" => {
            "dlgt cancel - interrupt the active execution\n\nUSAGE\n  dlgt cancel <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --timeout <DURATION>   Cancellation timeout (default: 30s)\n  --pretty               Pretty-print JSON output\n  -h, --help             Print this help"
        }
        "list" | "ls" => {
            "dlgt list - list Sessions\n\nUSAGE\n  dlgt list [--all] [--all-versions] [--pretty]\n  dlgt ls [--all] [--all-versions] [--pretty]\n\nOPTIONS\n  --all            Include terminal Sessions from live runtimes\n  --all-versions   Query every running dlgt version\n  --pretty         Pretty-print JSON output\n  -h, --help       Print this help"
        }
        "show" => {
            "dlgt show - show Session state, pending Requests, and latest result\n\nUSAGE\n  dlgt show <SESSION_ID|@ALIAS> [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help\n\nThe response carries resume_command, the provider's own command for opening\nthe conversation by hand after dlgt stop."
        }
        "answer" => {
            "dlgt answer - answer a pending approval or question\n\nUSAGE\n  dlgt answer <SESSION_ID|@ALIAS> <REQUEST_ID> --allow\n  dlgt answer <SESSION_ID|@ALIAS> <REQUEST_ID> --deny [--message <TEXT>]\n  dlgt answer <SESSION_ID|@ALIAS> <REQUEST_ID> --text <TEXT>\n  dlgt answer <SESSION_ID|@ALIAS> <REQUEST_ID> --choice <N>\n  dlgt answer <SESSION_ID|@ALIAS> <REQUEST_ID> --json <OBJECT>\n\nOPTIONS\n  --allow            Allow this one approval Request\n  --deny             Deny this approval Request; the worker continues its turn\n  --message <TEXT>   Tell the worker why it was denied\n  --text <TEXT>      Answer a question in free text\n  --choice <N>       Answer a question with its Nth choice (1-based)\n  --json <OBJECT>    Answer an MCP form elicitation with a JSON object\n  --pretty           Pretty-print JSON output\n  -h, --help         Print this help\n\nRequest IDs come from fetch's requests list. Repeating the same answer replays\nthe receipt; a different answer to a resolved Request is REQUEST_RESOLVED."
        }
        "stop" => {
            "dlgt stop - stop a Session and its process group\n\nUSAGE\n  dlgt stop <SESSION_ID|@ALIAS> [OPTIONS]\n\nOPTIONS\n  --force      Force termination\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "watch" => {
            "dlgt watch - follow a Session's messages as text\n\nUSAGE\n  dlgt watch <SESSION_ID|@ALIAS> [--from-start] [--until-idle]\n\nOPTIONS\n  --from-start   Replay the retained messages before following new ones\n  --until-idle   Exit when the current execution ends\n  -h, --help     Print this help\n\nRead-only: it sends nothing to the worker. The text is for humans and is not a\nstable format; agents use dlgt fetch."
        }
        "models" => {
            "dlgt models - discover models supported by a Harness\n\nUSAGE\n  dlgt models [OPTIONS]\n  dlgt models --harness <codex|claude> [OPTIONS]\n\nOPTIONS\n  --harness <codex|claude>   Query one Harness; omitted queries all\n  --include-hidden           Include hidden models\n  --pretty                   Pretty-print JSON output\n  -h, --help                 Print this help\n\nWithout --harness the response lists every Harness, and one that cannot be\nreached reports discovery: \"unavailable\" instead of failing the command."
        }
        "profiles" => {
            "dlgt profiles - list Profiles or show launch defaults\n\nUSAGE\n  dlgt profiles list [--pretty]\n  dlgt profiles show <NAME> [--pretty]\n  dlgt profiles defaults [--pretty]\n\nOPTIONS\n  --pretty     Pretty-print JSON output\n  -h, --help   Print this help"
        }
        "doctor" => {
            "dlgt doctor - diagnose the local dlgt installation\n\nUSAGE\n  dlgt doctor [OPTIONS]\n\nOPTIONS\n  --json      Emit the same checks as compact JSON\n  --probe     Initialize Codex app-server and check the published release\n  -h, --help  Print this help\n\nThe default checks are read-only and offline. --probe starts no model turn."
        }
        "skill" => {
            "dlgt skill - print the embedded dlgt skill\n\nUSAGE\n  dlgt skill\n\nOPTIONS\n  -h, --help   Print this help"
        }
        "rpc" => {
            "dlgt rpc - use the JSONL RPC interface\n\nUSAGE\n  dlgt rpc --stdio\n\nOPTIONS\n  --stdio      Read JSONL requests from stdin and write responses to stdout\n  -h, --help   Print this help"
        }
        "version" => {
            "dlgt version - print the dlgt version\n\nUSAGE\n  dlgt version\n\nOPTIONS\n  -h, --help   Print this help"
        }
        "help" => {
            "dlgt help - print top-level or command-specific help\n\nUSAGE\n  dlgt help [COMMAND]\n\nOPTIONS\n  -h, --help   Print this help"
        }
        _ => bail!("unknown help topic {command:?}; run `dlgt help`"),
    };
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_cwd_resolves_against_the_invocation_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = tempfile::tempdir()?;
        std::fs::create_dir(base.path().join("sub"))?;
        let expected = base
            .path()
            .join("sub")
            .canonicalize()?
            .to_string_lossy()
            .into_owned();

        assert_eq!(resolve_launch_cwd(Some("sub"), base.path())?, expected);
        assert_eq!(resolve_launch_cwd(Some("./sub"), base.path())?, expected);
        Ok(())
    }

    #[test]
    fn absolute_cwd_ignores_the_invocation_directory() -> Result<(), Box<dyn std::error::Error>> {
        let target = tempfile::tempdir()?;
        let unrelated = tempfile::tempdir()?;
        let expected = target.path().canonicalize()?.to_string_lossy().into_owned();

        assert_eq!(
            resolve_launch_cwd(Some(&expected), unrelated.path())?,
            expected
        );
        Ok(())
    }

    #[test]
    fn missing_cwd_fails_on_the_client() -> Result<(), Box<dyn std::error::Error>> {
        let base = tempfile::tempdir()?;

        match resolve_launch_cwd(Some("missing"), base.path()) {
            Ok(resolved) => return Err(format!("expected an error, got {resolved:?}").into()),
            Err(error) => {
                let message = format!("{error:#}");
                assert!(
                    message.contains("--cwd \"missing\" does not exist"),
                    "unexpected error message: {message}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn omitted_cwd_uses_the_invocation_directory_verbatim() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(
            resolve_launch_cwd(None, Path::new("/nonexistent-base"))?,
            "/nonexistent-base"
        );
        Ok(())
    }
}
