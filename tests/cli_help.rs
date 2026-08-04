use std::process::Command;

fn dlgt(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_dlgt")).args(args).output()
}

#[test]
fn every_public_command_supports_both_help_spellings() -> Result<(), Box<dyn std::error::Error>> {
    let commands = [
        "server",
        "update",
        "new",
        "restart",
        "send",
        "fetch",
        "cancel",
        "list",
        "ls",
        "show",
        "attach",
        "stop",
        "events",
        "scrollback",
        "logs",
        "models",
        "profiles",
        "harnesses",
        "skill",
        "rpc",
        "version",
        "help",
    ];

    for command in commands {
        let flag_form = dlgt(&[command, "--help"])?;
        let short_form = dlgt(&[command, "-h"])?;
        let help_form = dlgt(&["help", command])?;

        assert!(flag_form.status.success(), "{command} --help failed");
        assert!(short_form.status.success(), "{command} -h failed");
        assert!(help_form.status.success(), "help {command} failed");
        assert_eq!(flag_form.stdout, short_form.stdout, "{command} -h differed");
        assert_eq!(
            flag_form.stdout, help_form.stdout,
            "help {command} differed"
        );

        let help = String::from_utf8(flag_form.stdout)?;
        assert!(help.contains("USAGE"), "{command} help had no usage");
        assert!(
            help.contains(&format!(
                "dlgt {}",
                if command == "ls" { "list" } else { command }
            )),
            "{command} help named the wrong command"
        );
    }
    Ok(())
}

#[test]
fn list_alias_uses_list_help() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        dlgt(&["list", "--help"])?.stdout,
        dlgt(&["ls", "--help"])?.stdout
    );
    Ok(())
}

#[test]
fn prompt_named_help_is_not_treated_as_a_help_flag() -> Result<(), Box<dyn std::error::Error>> {
    let output = dlgt(&["new", "--", "--help"])?;

    assert!(!output.status.success());
    assert_ne!(output.stdout, dlgt(&["new", "--help"])?.stdout);
    Ok(())
}

#[test]
fn unknown_long_options_are_named_with_the_command_usage() -> Result<(), Box<dyn std::error::Error>>
{
    let home = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_dlgt"))
        .env("DLGT_HOME", home.path())
        .args(["show", "codex:test-session", "--json"])
        .output()?;

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("unknown option") && stdout.contains("--json"),
        "unexpected output: {stdout}"
    );
    assert!(stdout.contains("USAGE"), "unexpected output: {stdout}");
    assert!(
        !stdout.contains("missing value"),
        "unknown option consumed the next token: {stdout}"
    );
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn acceptance_requires_an_idempotency_key() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    for command in [
        vec!["new", "--title", "t", "--harness", "claude", "--", "hello"],
        vec!["send", "codex:test-session", "--", "hello"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dlgt"))
            .env("DLGT_HOME", home.path())
            .args(&command)
            .output()?;

        assert!(!output.status.success(), "{command:?} was accepted");
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            stdout.contains("--request-id"),
            "the error must name the flag: {stdout}"
        );
        assert!(stdout.contains("USAGE"), "unexpected output: {stdout}");
    }
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn an_idempotency_key_is_validated_before_the_prompt_is_read()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let workdir = tempfile::tempdir()?;
    let at_limit = "k".repeat(128);
    let over_limit = "k".repeat(129);
    let cases = [
        ("", true),
        (at_limit.as_str(), false),
        (over_limit.as_str(), true),
    ];
    for (key, rejected) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_dlgt"))
            .env("DLGT_HOME", home.path())
            .current_dir(workdir.path())
            // A missing --cwd fails after the key check, so it marks how far
            // the invocation got without needing a daemon.
            .args([
                "new",
                "--title",
                "t",
                "--harness",
                "claude",
                "--request-id",
                key,
                "--cwd",
                "./missing",
                "--",
                "hello",
            ])
            .output()?;

        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        if rejected {
            assert!(
                stdout.contains("--request-id"),
                "key of {} bytes should be rejected: {stdout}",
                key.len()
            );
        } else {
            assert!(
                stdout.contains("./missing"),
                "key of {} bytes should be accepted: {stdout}",
                key.len()
            );
        }
    }
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn resuming_also_requires_an_idempotency_key() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_dlgt"))
        .env("DLGT_HOME", home.path())
        .args(["send", "claude:some-session", "--resume", "--", "hello"])
        .output()?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout)?.contains("--request-id"));
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn send_no_longer_accepts_the_removed_wait_flags() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    for flag in [["--wait", "--timeout"], ["--timeout", "1s"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_dlgt"))
            .env("DLGT_HOME", home.path())
            .args([
                "send",
                "codex:test-session",
                flag[0],
                flag[1],
                "--",
                "hello",
            ])
            .output()?;

        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            stdout.contains("unknown option"),
            "unexpected output: {stdout}"
        );
    }
    assert!(!home.path().join("run").exists());
    Ok(())
}

#[test]
fn new_rejects_a_missing_relative_cwd_before_starting_a_daemon()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let workdir = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_dlgt"))
        .env("DLGT_HOME", home.path())
        .current_dir(workdir.path())
        .args([
            "new",
            "--title",
            "t",
            "--harness",
            "claude",
            "--request-id",
            "r1",
            "--cwd",
            "./missing",
            "--",
            "hello",
        ])
        .output()?;

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("--cwd"), "unexpected output: {stdout}");
    assert!(stdout.contains("./missing"), "unexpected output: {stdout}");
    assert!(
        stdout.contains("does not exist"),
        "unexpected output: {stdout}"
    );
    assert!(!home.path().join("run").exists());
    Ok(())
}
