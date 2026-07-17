# dlgt CLI v1 reference

Status: implemented public contract for the repository binary.

This document is the normative command reference. See [RPC](rpc.md) for the
programmatic interface and [Design](design.md) for the product boundary,
invariants, lifecycle rationale, security model, and acceptance criteria.

## Product definition

`dlgt` is a local, single-binary runtime for persistent, addressable, and
attachable Codex and Claude subagents.

The only public runtime object is a **Session**:

```text
Session
  One dlgt-owned harness process and PTY
  One controller at a time
  At most one active execution
  No server-side queue
```

Provider turns and execution receipts may remain internal for lifecycle
correlation and persistence, but they are not public CLI resources and do not
have public IDs.

Other terms:

```text
Harness   The provider adapter, initially codex or claude
Profile   A reusable client-side launch specification
Alias     A human-readable address for an active Session
Title     A non-unique human description used to generate an Alias
```

## Identifier and naming model

Session IDs are short, immutable, and intended for automation:

```text
ses_7K3M9Q2X
```

The suffix is eight characters of unambiguous Crockford Base32. Generation is
random, protected by a database uniqueness constraint, and retried on collision.

Aliases are for humans:

```text
title: run review
alias: @run-review-361csx
```

By default, `new` slugifies the title and adds a random suffix. A caller may
request an exact alias with `--alias`. An alias cannot be reused while owned by
a starting, idle, busy, blocked, canceling, or stopping Session. It becomes
available after that Session reaches a terminal stopped state. Historical
records remain addressable by Session ID.

Automation should retain the returned Session ID and use it for all later
commands. Aliases are ephemeral conveniences, not primary keys.

## Output contract

All control-plane commands emit one JSON document to stdout.

Success:

```json
{"ok":true,"session":{"id":"ses_7K3M9Q2X","state":"idle"}}
```

Failure:

```json
{"ok":false,"error":{"code":"SESSION_BUSY","message":"session already has active work","session_id":"ses_7K3M9Q2X"}}
```

Failures also return a non-zero process exit status. stderr is reserved for
failures that occur before dlgt can serialize a valid response, such as a
panic or corrupted executable startup.

The exceptions are deliberate:

```text
attach             raw interactive terminal
events --follow    NDJSON event stream
logs --raw         raw PTY bytes
help / skill       text
rpc --stdio        JSONL request/response stream
```

Pretty-printed JSON is opt-in through `--pretty`; default output is compact and
deterministic.

## Top-level help

```text
dlgt - persistent local subagent runtime

USAGE
  dlgt <COMMAND> [OPTIONS]

DELEGATION
  new          Create a new Session, optionally with its first prompt
  restart      Restart a Session
  send         Send work to an existing idle Session
  wait         Wait for the Session's current or latest execution
  cancel       Interrupt the Session's active execution

SESSIONS
  list, ls     List Sessions
  show         Show Session state or historical result
  attach       Attach to the Session screen
  stop         Stop the Session and its process group

OBSERVABILITY
  events       Read or follow normalized lifecycle events
  scrollback   Read rendered plain-text terminal scrollback
  logs         Read raw retained PTY bytes for diagnosis

CONFIGURATION
  models       Discover models supported by a Harness
  profiles     List or inspect launch Profiles
  harnesses    List Harnesses and supported options
  skill        Print the embedded dlgt skill

RUNTIME
  server       Run or stop the local daemon
  rpc          Use the JSONL RPC interface
```

## `new`

`new` is the only Session creation command.

Its command-specific help is available through either equivalent spelling:

```bash
dlgt new --help
dlgt help new
```

```text
dlgt new
  --title <TITLE>
  [--alias <@ALIAS>]
  [--profile <PROFILE>]
  [--harness codex|claude]
  [--model <MODEL>]
  [--effort <LEVEL>]
  [--cwd <DIR>]
  [--harness-option <KEY=VALUE>]...
  [--no-auto-approve]
  [--startup-timeout <DURATION>]
  [--clean-env]
  [--pass-env <KEY>]...
  [--env <KEY=VALUE>]...
  [--unset-env <KEY>]...
  [--wait --timeout <DURATION>]
  [--stdin | -- <PROMPT>]
```

Rules:

- `--title` is required and may be non-unique.
- A Profile or Harness must resolve the Harness selection.
- Model and effort are optional. Omission selects the provider default.
- By default dlgt launches workers auto-approved:
  `--dangerously-bypass-approvals-and-sandbox` for Codex and
  `--permission-mode=auto` for Claude. `--no-auto-approve` (or Profile
  `auto_approve = false`) keeps the Harness's own approval prompts. An
  explicit `permission-mode=...` Harness option replaces the implicit Claude
  mode.
- Before launching either Harness, dlgt records the Session working directory
  as trusted in that provider's local workspace state. For Claude this updates
  `~/.claude.json` and suppresses only the workspace trust dialog; tool
  permissions follow the auto-approve rule above.
- `--harness-option KEY=VALUE` explicitly adds `--KEY=VALUE` to Claude Code.
  It is repeatable, stored with the Session, and reused by `restart`. Options
  whose arguments are managed by dlgt are rejected. Codex does not currently
  accept Harness options.
- `--startup-timeout` is optional and defaults to 60 seconds, but startup is
  never unbounded.
- If a prompt is supplied, Session creation and acceptance of the first prompt
  are one atomic daemon operation.
- `--stdin` reads the exact prompt from standard input and is mutually exclusive
  with a prompt after `--`. It avoids argv disclosure and length limits.
- Without a prompt, `new` returns an idle Session for attach-first workflows.
- `--wait` requires a prompt and an explicit positive `--timeout`.
- If an exact requested alias is active, `new` fails with `ALIAS_IN_USE` and
  creates no Session or provider process.
- If startup succeeds but prompt acceptance fails, dlgt terminates the Harness,
  releases the Alias, and returns one structured launch failure. A failed audit
  record may remain addressable by its Session ID, but no live half-created
  Session is returned.

Example:

```bash
dlgt new \
  --title "prompting Claude worker" \
  --harness claude \
  --no-auto-approve \
  --cwd .
```

The unsafe full bypass remains available only when deliberately requested:

```bash
dlgt new \
  --title "unrestricted Claude worker" \
  --harness claude \
  --harness-option dangerously-skip-permissions=true \
  --cwd .
```

```bash
dlgt new \
  --title "run review" \
  --profile fable-review \
  --cwd . \
  -- "Review the current design"
```

```json
{
  "ok": true,
  "session": {
    "id": "ses_7K3M9Q2X",
    "alias": "@run-review-361csx",
    "title": "run review",
    "harness": "codex",
    "provider_session_id": "019f6307-341e-7e81-8a33-7ab61e804345",
    "state": "busy"
  },
  "execution_seq": 1
}
```

Synchronous first execution:

```bash
dlgt new \
  --title "run review" \
  --profile fable-review \
  --wait \
  --timeout 15m \
  -- "Review the current design"
```

The response contains the final result instead of exposing an execution ID.

## `restart`

```text
dlgt restart <SESSION_ID>
  [--startup-timeout <DURATION>]
  [--clean-env]
  [--pass-env <KEY>]...
  [--env <KEY=VALUE>]...
  [--unset-env <KEY>]...
  [--pretty]
```

`restart` replaces a Session's provider process while preserving its dlgt
Session ID, alias, durable history, execution sequence, and provider
conversation. Codex resumes the stored thread and Claude resumes the stored
conversation.

Rules:

- Active `idle`, `busy`, and `blocked` Sessions may be restarted, as may
  terminal `stopped` and `failed` Sessions. An active execution is durably
  completed as `interrupted` before the replacement process starts.
- `starting`, `stopping`, and `restarting` Sessions reject a second lifecycle
  operation with `SESSION_UNAVAILABLE`.
- The Session must have a stored provider conversation ID.
- A terminal Session should be addressed by immutable Session ID because its
  alias may already belong to a newer active Session.
- If another active Session now owns the old alias, restart fails with
  `ALIAS_IN_USE`; it never renames either Session implicitly.
- Restarting an active Session keeps its alias reserved throughout the process
  replacement.
- Startup is bounded by `--startup-timeout`, which defaults to 60 seconds.
- Launch environment values are freshly supplied by the invoking client and
  are not recovered from durable storage.
- Existing results, events, raw output, and scrollback remain readable; new
  executions continue the same monotonic `execution_seq`.

## `send`

```text
dlgt send <SESSION_ID|@ALIAS>
  [--wait --timeout <DURATION>]
  [--stdin | -- <PROMPT>]
```

Rules:

- The target Session must already exist.
- Launch, model, environment, and Profile options are not accepted by `send`.
- The prompt is exactly one argument after mandatory `--`.
- `--stdin` is the mutually exclusive safe path for long or sensitive prompts.
- If the Session is idle, dlgt accepts the prompt and transitions it to busy.
- If the Session is busy, canceling, blocked, stopping, stopped, or
  attached, the command fails immediately and has no side effects. Busy and
  canceling return `SESSION_BUSY`; blocked returns `SESSION_BLOCKED`; attached
  returns `SESSION_ATTACHED`; other non-idle states return
  `SESSION_UNAVAILABLE`.
- `--wait` requires an explicit positive `--timeout`.
- There is no `--create`, `--enqueue`, `--after`, or `--fail-if-busy`; creation
  and queueing are not `send` responsibilities, and busy rejection is always
  the default.

Asynchronous example:

```bash
dlgt send ses_7K3M9Q2X -- "Review the revised design"
```

```json
{"ok":true,"session":{"id":"ses_7K3M9Q2X","state":"busy"},"execution_seq":2}
```

Busy rejection:

```json
{"ok":false,"error":{"code":"SESSION_BUSY","session_id":"ses_7K3M9Q2X"}}
```

Synchronous example:

```bash
dlgt send ses_7K3M9Q2X \
  --wait \
  --timeout 15m \
  -- "Review the revised design"
```

```json
{
  "ok": true,
  "session": {"id":"ses_7K3M9Q2X","state":"idle"},
  "result": {
    "execution_seq": 2,
    "status": "completed",
    "final_text": "Review result...",
    "error": null,
    "started_at_ms": 1784024104395,
    "completed_at_ms": 1784024252019,
    "usage": null
  }
}
```

## Durable result

Every accepted execution receives a per-Session monotonic `execution_seq`.
This number is returned by `new` or `send`, echoed by lifecycle events and the
durable result, and never accepted as a CLI selector. It is correlation data,
not a public execution resource or ID.

The durable result shape is:

```json
{
  "execution_seq": 2,
  "status": "completed",
  "final_text": "Review result...",
  "error": null,
  "started_at_ms": 1784024104395,
  "completed_at_ms": 1784024252019,
  "usage": null
}
```

`status` is one of `completed`, `failed`, `canceled`, or `interrupted`.
`final_text` is the Harness-reported final assistant message and is always a
string for `completed`, although it may be empty. Failed terminal states may
provide partial final text and must provide a structured error. Usage is
nullable because availability differs by Harness.

## `wait`

```text
dlgt wait <SESSION_ID|@ALIAS> --timeout <DURATION>
```

The timeout is required and positive.

`wait` binds to the Session's active execution and `execution_seq` at request
time. If the Session is already idle and has a latest durable result, it returns
that result. If the Session has never accepted work, it returns `NO_RESULT`.

Because a Session has one controller and no queue, the public contract does not
need an addressable execution or Turn identifier. The non-addressable sequence
number lets callers correlate acceptance and result without expanding the
object model. Internally, dlgt may retain provider IDs to reject stale
lifecycle events and persist history correctly.

A wait timeout returns `WAIT_TIMEOUT` and leaves the Session busy:

```json
{
  "ok": false,
  "error": {
    "code": "WAIT_TIMEOUT",
    "session_id": "ses_7K3M9Q2X",
    "session_state": "busy"
  }
}
```

If the Session transitions to blocked, `wait` returns immediately with
`SESSION_BLOCKED` and exit 4. It does not wait for the timeout deadline.

## `cancel`

```text
dlgt cancel <SESSION_ID|@ALIAS> [--timeout <DURATION>]
```

`cancel` interrupts the active provider execution. It does not stop the
Session. A successful cancellation terminalizes the current result as
`canceled` or `interrupted` according to the normalized provider mapping and
returns the Session to idle only after provider quiescence is proven.

Cancellation is bounded and defaults to 30 seconds. On timeout, dlgt returns
`CANCEL_TIMEOUT`, leaves the Session in `canceling`, and continues observing
provider quiescence in the background. `events` and `wait` reveal the eventual
terminal state.

Canceling an idle Session is idempotent: it returns exit 0 with
`{"canceled":false,"reason":"NO_ACTIVE_WORK"}`.

## Blocked input

Input required from a human is a first-class Session state, not a failure and
not an infinite wait.

```json
{
  "ok": false,
  "error": {
    "code": "SESSION_BLOCKED",
    "session_id": "ses_7K3M9Q2X",
    "action": "dlgt attach ses_7K3M9Q2X"
  }
}
```

After a human attaches, answers, and detaches, the same `wait` command may be
issued again. Provider-specific detection may initially be conservative, but
dlgt must never infer completion from a quiet screen.

## Session commands

```text
dlgt list [--all] [--pretty]
dlgt show <SESSION_ID|@ALIAS> [--pretty]
dlgt attach <SESSION_ID|@ALIAS> [--steal]
dlgt stop <SESSION_ID|@ALIAS> [--force]
dlgt restart <SESSION_ID> [environment options]
```

- `list` returns active Sessions.
- `list --all` includes terminal historical Sessions.
- `show` returns identity, Harness, model selection, state, current timing,
  latest durable result, and relevant failure data.
- `attach` takes an exclusive input lease, replays the retained terminal view,
  and follows the live PTY. A second attach returns `ALREADY_ATTACHED` unless
  `--steal` explicitly transfers the lease. Detach with `Ctrl-b d`. Mirrored
  multi-attach is outside v1.
- `stop` requests graceful Session termination.
- `stop --force` terminates the provider process group.

## Lifecycle events

```text
dlgt events [<SESSION_ID|@ALIAS>] [--after <SEQ>] [--follow]
```

Without `--follow`, the command returns a JSON array. With `--follow`, it emits
one normalized NDJSON event per line until interrupted or the connection ends.

The versioned event schema, complete event set, and streaming boundary are
defined in [RPC](rpc.md#lifecycle-events).

## Rendered scrollback and raw logs

Normal observation uses a headless VT emulator and plain-text scrollback:

```text
dlgt scrollback <SESSION_ID|@ALIAS>
  [--lines <COUNT>]
  [--before <CURSOR>]
```

The default is the latest 100 rendered lines. v1 retains at most 10,000
rendered rows per Session. The response includes the
terminal dimensions, plain-text lines, truncation state, and an opaque cursor
for older pages.

```json
{
  "ok": true,
  "session_id": "ses_7K3M9Q2X",
  "screen": {"rows":24,"cols":120},
  "lines": ["Review complete.","","Main concerns:","1. Timeout behavior..."],
  "truncated": true,
  "before": "scr_84A2"
}
```

Raw PTY bytes are explicitly diagnostic:

```text
dlgt logs <SESSION_ID|@ALIAS> --raw
dlgt logs <SESSION_ID|@ALIAS> --raw --json
```

Plain `dlgt logs` without `--raw` is invalid. `--raw` writes bytes directly;
`--raw --json` returns base64. There is no `logs --follow`. Live lifecycle
observation uses `events --follow`, and live terminal observation uses
`attach`.

Requiring `--raw` is an intentional capability gate. See
[Design](design.md#rendered-scrollback-and-raw-pty) for the VT rendering and
raw-retention rationale.

## Model discovery

```text
dlgt models --harness codex [--include-hidden]
dlgt models --harness claude
```

Codex discovery uses app-server `model/list` and returns account-aware model
IDs, display names, descriptions, defaults, supported reasoning efforts, input
modalities, and service tiers.

```json
{
  "ok": true,
  "harness": "codex",
  "source": "app-server",
  "discovery": "complete",
  "models": []
}
```

Claude Code does not currently expose an equivalent documented non-interactive
picker API. dlgt returns stable Claude Code aliases and reports discovery as
partial. When API-key authentication provides access to the Anthropic Models
API, those results may be included separately but must not be presented as the
Claude Code subscription picker.

```json
{
  "ok": true,
  "harness": "claude",
  "source": "claude-code-aliases",
  "discovery": "partial",
  "models": [
    {"id":"default","recommended":true},
    {"id":"best"},
    {"id":"sonnet"},
    {"id":"opus"},
    {"id":"haiku"}
  ]
}
```

Model and effort are optional at launch. Omission selects the provider's
recommended default. Profiles should prefer stable provider aliases unless an
exact version pin is required.

Model aliases are resolved by the Harness when `new` launches the Session, not
on each `send`. dlgt does not silently pin a drifting alias; `show` reports the
provider-resolved model when the Harness makes it available.

## Profiles and launch environment

```text
dlgt profiles list
dlgt profiles show <NAME>
dlgt harnesses [<HARNESS>]
```

Profiles are client-side launch specifications. The client expands them before
RPC so the daemon does not need to reread mutable configuration.

```toml
[profiles.fable-review]
harness = "claude"
model = "best"
effort = "high"
harness_options = ["permission-mode=auto"]
clean_env = true
pass_env = ["PATH", "HOME", "SSH_AUTH_SOCK"]
```

Environment precedence:

```text
client snapshot or clean base < Profile < explicit launch options
```

Profile `harness_options` are followed by explicit `--harness-option` values.
They configure the provider CLI rather than the launch environment.

- Default launch environment is a snapshot of the invoking client's
  environment, never the daemon's startup environment.
- `--clean-env` starts from a minimal runtime base.
- `--pass-env KEY` copies one client value with `--clean-env`.
- `--env KEY=VALUE` sets or overrides a value.
- `--unset-env KEY` removes a value.
- Environment options apply when creating or restarting a Session. Values are
  freshly snapshotted for each process launch and are never stored for replay.
- dlgt applies final lifecycle safety overrides to owned Harness children:
  `check_for_update_on_startup=false` for Codex and `DISABLE_AUTOUPDATER=1` for
  Claude. They cannot be overridden per Session and do not change provider
  global configuration.
- Launch environment values are passed in RPC memory, never argv, and are never
  directly serialized into Session records, `list`, `show`, `events`, Profiles,
  or error JSON. Provider output is untrusted and can deliberately echo its
  environment, so results, scrollback, and especially `logs --raw` must be
  treated as potentially sensitive output rather than as a redaction boundary.

## Exit statuses

```text
0  command succeeded, or a waited execution completed
1  usage, configuration, identity, launch, or RPC error
2  waited execution failed, canceled, or was interrupted
3  bounded wait timeout; the underlying execution or cancellation continues
4  Session is blocked on input
5  Session is busy and rejected a send
```

The JSON error code is the primary machine-readable reason. Exit status is the
shell-level summary. `SESSION_BLOCKED` uses exit 4 and `SESSION_BUSY` uses exit
5. `NO_RESULT`, `SESSION_ATTACHED`, `ALREADY_ATTACHED`, `ALIAS_IN_USE`, and
`SESSION_UNAVAILABLE` use exit 1. `WAIT_TIMEOUT` and `CANCEL_TIMEOUT` use exit
3. A Session stopped during `wait` produces a durable `interrupted` result and
exit 2. Idle `cancel` is an idempotent exit-0 no-op.

The stable v1 structured error-code families are:

```text
INVALID_ARGUMENT       Invocation cannot be retried unchanged
NOT_FOUND              Session or configuration object does not exist
NO_RESULT              Session has never accepted work
ALIAS_IN_USE           Exact Alias belongs to a non-terminal Session
SESSION_BUSY           Active execution; retry after it terminalizes
SESSION_BLOCKED        Human input is required
SESSION_ATTACHED       Exclusive attach lease prevents semantic send
SESSION_UNAVAILABLE    Session state cannot accept the requested operation
ALREADY_ATTACHED       Another client owns the attach lease
WAIT_TIMEOUT           Wait expired; execution continues
CANCEL_TIMEOUT         Cancel wait expired; cancellation continues
LAUNCH_FAILED          Harness startup or initial prompt acceptance failed
PROVIDER_FAILED        Provider terminalized work as failed
RPC_UNAVAILABLE        Daemon transport is unavailable; retry may succeed
INTERNAL               dlgt invariant or persistence failure
```

Commands may add contextual fields, but must not overload one code with a
different retry or human-action policy.

`new` launch failures include the failed audit record's `session_id`. If Codex
or Claude assigned its own session ID before the failure, the error also
includes `provider_session_id` so provider-native logs can be correlated.

## Design and RPC contracts

The provider lifecycle mapping, acceptance criteria, and design rationale are
in [Design](design.md). The public JSONL method set and schemas are in
[RPC](rpc.md).
