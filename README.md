<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/dlgt-lockup-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/dlgt-lockup-light.png">
  <img alt="dlgt" src="assets/dlgt-lockup-light.png" width="246">
</picture>

> Let agents delegate to the competition.

Codex wasn't built to delegate to Claude. Claude wasn't built to delegate to
Codex. `dlgt` was.

Once, everyone wanted an AI CEO, AI engineers, and an entire agent fleet. Most
of those products made a splash and disappeared. The useful part was simpler:
pick the frontier model you like, use the subagents already built into its
harness, and call the other side when it has something useful to add.

`dlgt` fills that one gap. It lets Codex use Claude and Claude use Codex.

![I built an entire company with 47 AI agents. Hey Sol, ask Fable to review this.](assets/delegate-to-the-competition.jpg)

## Quick Start

From Codex:

```bash
codex "Read and follow https://combinatrix.ai/dlgt/installation-instruction to install dlgt for this harness"
```

From Claude:

```bash
claude "Read and follow https://combinatrix.ai/dlgt/installation-instruction to install dlgt for this harness"
```

Then ask naturally:

```bash
codex -m gpt-5.6-sol "Create a great game. Ask Fable to review it."

claude --model claude-fable-5 "Think of 10 funny jokes. Ask Sol at xhigh effort to review them."
```

No fleet to configure. No invented org chart. The harness you chose stays in
charge and uses `dlgt` when it needs a counterpart.

## Install

Install the latest published `dlgt` release on macOS or Linux. The installer
detects the platform, verifies the GitHub Release checksum, installs the
user-writable binary, and registers the embedded skill for existing Codex or
Claude user directories:

```bash
curl -fsSL https://raw.githubusercontent.com/combinatrix-ai/dlgt/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
```

Install a specific release with `--version v<version>`, or choose skill
registration explicitly with `--skill codex`, `--skill claude`, `--skill both`,
or `--no-skill`. The normal installation path does not require Rust, Cargo, or
a source checkout. See the [full installation instructions](https://combinatrix.ai/dlgt/installation-instruction)
for supported targets and verification steps.

## What dlgt does

`dlgt` runs Codex and Claude as durable, addressable local Sessions. Each
Session owns one harness process, one PTY, one terminal screen, and at most one
active execution.

- Provider lifecycle hooks report readiness and completion.
- Sessions survive across commands and follow-up prompts.
- JSON output and JSONL RPC make delegation automatable.
- State, events, results, and bounded terminal history stay local.
- The leader sees the counterpart's result and decides what to use.

`dlgt` is not a planner, company simulator, workflow language, or multi-agent
framework. It is the bridge between two competing harnesses.

## Why not the DIY routes

- **`tmux send-keys`** — the leader polls `capture-pane` and burns tokens on
  screen dumps, or you script UI heuristics that break on a spinner.
- **`claude -p` / `codex exec`** — every call is a cold start that throws away
  context, and headless runs sometimes aren't covered by your subscription.
- **`dlgt`** — completion is a lifecycle event, follow-ups keep their Session
  context, the managed PTY returns JSON instead of screen scrapes, and it runs
  on the plan you already pay for.

## Direct CLI use

After installing `dlgt`, create a Claude Session and wait for its review:

```bash
dlgt new \
  --title "Claude review" \
  --harness claude \
  --model claude-fable-5 \
  --effort high \
  --cwd . \
  --wait \
  --timeout 15m \
  -- "Review this repository. Return findings and trade-offs only."
```

Create a Codex Session:

```bash
dlgt new \
  --title "Codex review" \
  --harness codex \
  --model gpt-5.6-luna \
  --effort xhigh \
  --cwd . \
  -- "Review the implementation and report correctness risks."
```

The command returns a Session ID such as `ses_7K3M9Q2X`:

```bash
dlgt wait ses_7K3M9Q2X --timeout 15m
dlgt send ses_7K3M9Q2X --wait --timeout 15m -- "Review the revision"
dlgt restart ses_7K3M9Q2X
dlgt show ses_7K3M9Q2X
dlgt scrollback ses_7K3M9Q2X --lines 100
dlgt attach ses_7K3M9Q2X
dlgt stop ses_7K3M9Q2X
```

The first client command starts the local daemon automatically.

## Configuration

Store reusable launch profiles in `~/.config/dlgt/config.toml`, or point
`DLGT_CONFIG` at another file:

```toml
[profiles.fable-review]
harness = "claude"
model = "claude-fable-5"
effort = "high"
harness_options = ["permission-mode=auto"]
clean_env = true
```

Claude Code uses its own permission default unless a Profile or
`--harness-option KEY=VALUE` explicitly selects another mode. dlgt does not
pass `--dangerously-skip-permissions` by default.

Set `DLGT_HOME` to relocate the SQLite database and Unix socket. Set
`DLGT_SOCKET` to override only the socket.

## Build and verify

Contributor builds must name the `dlgt` binary explicitly:

```bash
cargo build --bin dlgt
cargo build --release --bin dlgt
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --bin dlgt && tests/smoke.sh
npm ci
npm run docs:build
```

## Documentation

- [Documentation site](https://combinatrix.ai/dlgt/)
- [Installation instructions](https://combinatrix.ai/dlgt/installation-instruction)
- [CLI reference](docs/cli.md)
- [Local RPC](docs/rpc.md)
- [Design](docs/design.md)
- [Why not an agent fleet?](docs/orchestrator-landscape.md)

Run `dlgt skill` to print the agent-facing contract embedded from
[`assets/dlgt-skill.md`](assets/dlgt-skill.md). The binary has no runtime
dependency on an installed skill directory.

The PTY and attach architecture is derived from the private `umux` project.
