# dlgt CLI v2 reference

This reference is also published as raw Markdown at
https://combinatrix.ai/dlgt/cli.md for agents to fetch with curl.

This document is the normative command reference. See [RPC](rpc.md) for the
programmatic interface and [Design](design.md) for the product boundary,
invariants, lifecycle rationale, security model, and acceptance criteria.

> **Status: implemented on the terminal-less branch, not released.** Released
> dlgt 0.5.x still implements the PTY-based v1 CLI. [RPC](rpc.md) and
> [Design](design.md) describe the terminal-less v2 interface. Behavior that
> has not been measured against a provider is listed under
> [Open questions](#open-questions).

## Product definition

`dlgt` is a local, single-binary runtime for live, addressable Codex and Claude
subagents. It drives each provider through its structured interface and owns
no terminal.

The only public runtime object is a **Session**:

```text
Session
  One dlgt-owned harness process, driven over a structured channel
  At most one active execution
  No server-side queue
  No PTY, no screen, no attach
```

```text
Codex    codex app-server, JSON-RPC over stdio
Claude   claude -p with stream-json input and output, one long-lived process
```

Provider turns and execution receipts may remain internal for lifecycle
correlation while the daemon is alive, but they are not public CLI resources and do not
have public IDs.

Other terms:

```text
Harness   The provider adapter, codex or claude
Profile   A reusable client-side launch specification
Alias     A human-readable address for an active Session
Title     A non-unique human description used to generate an Alias
Request   A pending approval or question that only the caller can answer
```

## Identifier and naming model

Every successful `new` returns one provider-qualified Session ID:

```text
codex:019f6307-341e-7e81-8a33-7ab61e804345
claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629
```

The suffix is the provider's own Codex thread ID or Claude session ID. This
single `session.id` is both the live dlgt address and the durable resume
address. There is no separate provider Session ID or resume reference.

During startup only, dlgt correlates the not-yet-bound process with an
`internal:<short-id>`. It atomically rekeys all retained state to the
provider-qualified ID before accepting the first prompt or returning success.
The internal launch ID is not a public Session ID; it may appear only as
`error.launch_id` when startup fails before provider binding. That failed
launch stays addressable for diagnostics: `show` and `fetch` accept the
`launch_id`.

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
{"ok":true,"session":{"id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","state":"idle"}}
```

Failure:

```json
{"ok":false,"error":{"code":"SESSION_BUSY","message":"session already has active work","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}}
```

Failures also return a non-zero process exit status. stderr is reserved for
failures that occur before dlgt can serialize a valid response, such as a
panic or corrupted executable startup.

The exceptions are deliberate:

```text
watch              human-readable text stream
help / skill       text
rpc --stdio        JSONL request/response stream
```

Pretty-printed JSON is opt-in through `--pretty`; default output is compact and
deterministic.

## Top-level help

```text
dlgt - local subagent runtime

USAGE
  dlgt <COMMAND> [OPTIONS]

DELEGATION
  new          Create a Session with its first prompt
  send         Send work to an idle Session, --steer a busy one, or --resume
  fetch        Read new state, results, events, and messages from a cursor
  answer       Answer a pending approval or question
  cancel       Interrupt the active execution

SESSIONS
  list, ls     List Sessions
  show         Show Session state and latest result
  watch        Follow a Session's messages as text (read-only)
  restart      Restart a Session
  stop         Stop the Session

CONFIGURATION
  models       Discover Harness models
  profiles     List or inspect Profiles
  doctor       Diagnose the local dlgt installation
  skill        Print the embedded dlgt skill

RUNTIME
  server       Run or stop the daemon
  update       Install the latest release and embedded Skills
  rpc          Use JSONL RPC
```

Removed from v1: `attach`, `scrollback`, `logs`, `harnesses`,
`fetch --screen`, `--no-auto-approve`, and the `cursor`, `grok`, `opencode`,
and `pi` Harnesses. `--harness` remains so that further Harnesses can return
as values of the same option.

Each release uses its own socket at
`$DLGT_HOME/run/<version>/dlgt.sock`. Most commands address the daemon for the
invoking binary's version. `send` first scans all live versioned sockets for an
exact provider-qualified Session ID and routes to its owning daemon; multiple
matches fail rather than choosing or launching. `dlgt list
--all-versions` queries every currently running version and annotates each
Session with `runtime_version` and `runtime_socket`. Session state, results,
events, and messages exist only while their owning daemon remains alive.

Busy Session snapshots include two integer diagnostics when `state` is exactly
`busy`:

```json
{
  "state": "busy",
  "busy_for_ms": 183000,
  "quiet_for_ms": 72000
}
```

`busy_for_ms` measures from reservation of the current turn. `quiet_for_ms`
measures time since the provider last emitted any event, clamped to that same
busy interval. If no event has occurred, it equals `busy_for_ms`. These
counters are diagnostic only; provider silence never changes Session state or
makes a busy Session accept another prompt. Both fields are omitted for every
other state.

Successful commands may include an optional non-error notice:

```json
{"ok":true,"sessions":[],"info":{"code":"UPDATE_AVAILABLE","current_version":"0.1.4","latest_version":"0.2.0","command":"dlgt update"}}
```

Each long-lived versioned daemon performs the first update check
asynchronously at startup and repeats it every six hours. Transient check
failures preserve the last successful notice; a successful check with no
newer release clears the notice.

`dlgt update` verifies the release's attested checksum manifest, then installs
the archive through the installer embedded in the running binary. It never
downloads executable installer code after verification. The embedded installer
checks the authenticated archive digest, pins the archive target to the running
binary's build target, atomically replaces the current executable, and
refreshes the embedded Codex and Claude Skills. Existing older-version daemons
and their live Sessions continue on their versioned sockets.

## Approvals

One option decides who approves what a worker does. It is set at `new`, stored
with the Session, and reused by `restart` and `send --resume`. `send --resume`
keeps the stored policy unless an explicit `--approvals` option or Profile
supplies a replacement; built-in and `[defaults]` values do not replace it.

```text
--approvals auto|ask|bypass     (default: auto)
```

| Value | Claude | Codex | What reaches the caller |
| --- | --- | --- | --- |
| `auto` | `--permission-mode auto` | `sandbox=workspace-write`, `approvalPolicy=on-request`, `approvalsReviewer=auto_review` | Only what the provider's own reviewer does not settle |
| `ask` | `--permission-mode manual` | `sandbox=workspace-write`, `approvalPolicy=on-request`, `approvalsReviewer=user` | Every approval request |
| `bypass` | `--dangerously-skip-permissions` | `sandbox=danger-full-access`, `approvalPolicy=never` | Nothing |

In `ask`, dlgt forwards every approval request the provider raises. Allow rules
in your own provider configuration, such as Claude's `settings.json`, can mean
the provider raises no requests for those tools.

The Codex column is the `thread/start` and `thread/resume` parameters. The
app-server has no `--dangerously-bypass-approvals-and-sandbox` flag, so
`bypass` is the pair of thread parameters that flag stands for. Claude always
runs with `--permission-prompt-tool stdio`, which is how its requests reach
dlgt.

Rules:

- dlgt never approves on its own. In `auto` the provider's reviewer decides;
  in `ask` the caller decides through [`answer`](#blocked-input-and-answer); in `bypass` nothing
  is asked.
- Every reviewer decision that dlgt can observe is recorded as an `approval`
  message, so the caller can read afterwards what was approved or denied and
  by whom. See [Messages](#messages).
- Claude's auto mode is not available for every model. When Claude reports a
  different mode than the one requested, the Session snapshot says so and the
  Session behaves as the effective mode:

  ```json
  {"approvals":{"requested":"auto","effective":"ask","reason":"provider reported permission mode default"}}
  ```

  Claude reports its mode when it starts and again on each turn. A fallback
  can therefore first show up during the first execution, where it is also
  recorded as a `notice` message.
- Claude does not report what its auto mode approved. Only its denials are
  observable, so in `auto` a Claude Session has no `approval` message for an
  action the provider let through.
- A request that nobody answers never times out into an approval. It stays
  pending and the Session stays `blocked` until `answer`, `cancel`, or `stop`.
- dlgt-supplied Harness options (from `--harness-option`, a Profile, or
  `[defaults]`) cannot change the mapping above or carry configuration. They
  are rejected with `INVALID_ARGUMENT`, naming the option, under every mode
  and before a provider process starts:
  - Claude: `permission-mode`, `permission-prompt-tool`, `permission-prompts`,
    `dangerously-skip-permissions`, `allow-dangerously-skip-permissions`,
    `allowedTools`, `allowed-tools`, `bare`, and the configuration options
    `settings`, `setting-sources`, `agents`, and `plugin-dir`, whose contents
    can carry permission rules, agent permission modes, or approving hooks.
  - Codex: any key with a segment named `profile`, `profiles`,
    `approval_policy`, `approvals_reviewer`, `sandbox`, `sandbox_mode`, or
    `sandbox_workspace_write`, and any value that is an inline table or an
    array of tables.
- `ask` forwards every request the provider raises. Configuration you own is
  your policy and dlgt leaves it alone: allow rules in your Claude
  `settings.json` or a `CLAUDE_CONFIG_DIR` you select, your
  `~/.codex/config.toml`, and options such as `--add-dir`. A tool your own
  configuration allows produces no request, so `ask` cannot show it.
- Codex `workspace-write` still allows writes under the system temporary
  directories. Those writes produce no approval request in any mode.

## `new`

`new` is the only Session creation command and always requires its first
prompt. Launch and local reservation are one atomic operation. The command
waits briefly for provider confirmation from Codex's app-server turn lifecycle
or from Claude starting the submitted user message, then returns success with
`submission` set to `confirmed` or `pending`. Observation of later progress and
the answer is a separate `fetch`.

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
  [--approvals auto|ask|bypass]
  [--harness-option <KEY[=VALUE]>]...
  [--startup-timeout <DURATION>]
  [--clean-env]
  [--pass-env <KEY>]...
  [--env <KEY=VALUE>]...
  [--unset-env <KEY>]...
  --request-id <ID>         (required)
  [--prompt-file <PATH> | --stdin | -- <PROMPT>]   (required)
```

Rules:

- `--title` is required and may be non-unique.
- Every `fetch` repeats a Session's metadata, so it is bounded: `--title` at
  most 200 characters, `--alias` 64, `--model` 128, `--effort` 32, and the
  working directory 4,096. A longer value is `INVALID_ARGUMENT`.
- A Profile or Harness must resolve the Harness selection.
- Model and effort are optional. Omission selects the provider default.
- `--approvals` defaults to `auto`. See [Approvals](#approvals).
- `--harness-option KEY[=VALUE]` is repeatable, stored with the Session, and
  reused by `restart`. Claude receives a bare key as `--KEY` and a keyed value
  as `--KEY=VALUE`. Codex requires `KEY=VALUE` and receives it as
  `--config KEY=VALUE` on its app-server process. Options whose values are
  managed by dlgt are rejected.
- `--startup-timeout` is optional and defaults to 60 seconds, but startup is
  never unbounded.
- Session creation and acceptance of the first prompt are one atomic daemon
  operation; omitting it returns `INVALID_ARGUMENT`.
- `--prompt-file` reads the exact UTF-8 prompt from a file. A relative path is
  resolved from the directory where `dlgt` is invoked, independently of the
  Session's `--cwd`.
- `--prompt-file`, `--stdin`, and a prompt after `--` are mutually exclusive.
  Both file and stdin input avoid putting the prompt text in argv.
- Prefer `--prompt-file` for long prompts from an agent-controlled shell. It
  needs no pipe, heredoc, or input redirection.
- `--request-id` is required, and must be non-empty and at most 128 bytes.
  Retrying the same ID with a byte-identical payload returns the original
  acceptance receipt with `"replayed": true` instead of creating a second
  Session, even if the Session has since moved on. The same ID with a
  different payload is `INVALID_ARGUMENT`. The daemon retains the last 1,024
  receipts for its lifetime.

  It is required rather than optional because an idempotency key only works if
  it exists *before* the first attempt: by the time a caller notices a lost
  response, an optional key would already be too late, and callers reliably do
  not add safety options they were not forced to. `--alias` is unaffected and
  remains a human convenience, not the deduplication key.
- If an exact requested alias is active, `new` fails with `ALIAS_IN_USE` and
  creates no Session or provider process.
- If startup succeeds but prompt acceptance fails, dlgt terminates the Harness,
  releases the Alias, and returns one structured launch failure. A failed audit
  record may remain addressable by its Session ID, but no live half-created
  Session is returned.
- If local delivery succeeds but the provider does not confirm submission
  within five seconds, the command still exits successfully with
  `submission: "pending"`, the Session ID, and a bounded `fetch` action. The
  prompt may still start. Do not resend it with a new request ID; replaying the
  original request ID returns the same execution and can later report
  `submission: "confirmed"`.

Example:

```bash
dlgt new \
  --title "careful Claude worker" \
  --harness claude \
  --approvals ask \
  --cwd . \
  --request-id review-1 \
  -- "Review the current design"
```

The unsafe full bypass remains available only when deliberately requested:

```bash
dlgt new \
  --title "unrestricted Codex worker" \
  --harness codex \
  --approvals bypass \
  --cwd . \
  --request-id review-2 \
  -- "Review the current design"
```

Codex configuration uses the same option with a TOML value:

```bash
dlgt new \
  --title "Codex with live search" \
  --harness codex \
  --harness-option 'web_search="live"' \
  --cwd . \
  --request-id codex-search-1 \
  -- "Research the current behavior"
```

```bash
dlgt new \
  --title "run review" \
  --profile fable-review \
  --cwd . \
  --request-id run-review-1 \
  -- "Review the current design"
```

```json
{
  "ok": true,
  "session": {
    "id": "codex:019f6307-341e-7e81-8a33-7ab61e804345",
    "alias": "@run-review-361csx",
    "title": "run review",
    "harness": "codex",
    "state": "busy"
  },
  "execution_seq": 1,
  "submission": "confirmed",
  "cursor": "4"
}
```

`cursor` is this Session's observation position, taken immediately before the
local reservation is recorded, so passing it to `fetch --cursor` cannot miss output a
fast provider emitted in between. Positions are small counting numbers, so
they survive an agent's conversation context. Follow the submission with:

```bash
dlgt fetch codex:019f6307-341e-7e81-8a33-7ab61e804345 \
  --cursor 4 --wait 15m
```

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

`restart` replaces a Session's provider process while preserving its alias,
retained history, execution sequence, and provider conversation. Codex keeps
the same Session ID. Claude names its session on the first turn of a process,
so a rotated provider session ID becomes known when the next execution starts.
dlgt then atomically rekeys the Session, and every later snapshot carries the
new canonical `session.id`. The ID `restart` returned stays valid for `fetch`
and its cursors for the daemon lifetime; other commands need the new ID or the
alias.

Rules:

- Active `idle`, `busy`, `blocked`, and `canceling` Sessions may be restarted, as may
  terminal `stopped` and `failed` Sessions. An active execution is durably
  completed as `interrupted` before the replacement process starts.
- `starting`, `stopping`, and `restarting` Sessions reject a second lifecycle
  operation with `SESSION_UNAVAILABLE`.
- The Session ID must contain a provider conversation ID.
- A terminal Session should be addressed by its provider-qualified Session ID
  because its alias may already belong to a newer active Session.
- If another active Session now owns the old alias, restart fails with
  `ALIAS_IN_USE`; it never renames either Session implicitly.
- Restarting an active Session keeps its alias reserved throughout the process
  replacement.
- Startup is bounded by `--startup-timeout`, which defaults to 60 seconds.
- Launch environment values are freshly supplied by the invoking client and
  are not retained for replay.
- Existing results, events, and messages remain readable; new executions
  continue the same monotonic `execution_seq`.
- Pending Requests of the replaced process are closed as `canceled`.

## `send`

```text
dlgt send <SESSION_ID|@ALIAS>
  [--steer]
  --request-id <ID>         (required)
  [--pretty]
  [--prompt-file <PATH> | --stdin | -- <PROMPT>]   (required)
```

Resume a provider conversation after its owning daemon exits with the same
Session ID returned by `new`:

```text
dlgt send <codex:PROVIDER_THREAD_ID|claude:PROVIDER_SESSION_ID> --resume
  [--profile <PROFILE>]
  [--model <MODEL>]
  [--effort <LEVEL>]
  [--cwd <DIR>]
  [--approvals auto|ask|bypass]
  [--harness-option <KEY[=VALUE]>]...
  [--startup-timeout <DURATION>]
  [--clean-env]
  [--pass-env <KEY>]...
  [--env <KEY=VALUE>]...
  [--unset-env <KEY>]...
  --request-id <ID>         (required)
  [--pretty]
  [--prompt-file <PATH> | --stdin | -- <PROMPT>]   (required)
```

The launch options above are accepted only with `--resume`, and `--harness`
is rejected because the provider prefix selects the Harness. `--profile` is
also accepted only with `--resume` and must match the Harness. A matching live
Session is reused; if
none exists dlgt reserves the provider conversation, launches a new Session,
waits for bind/readiness, and atomically accepts the prompt. Success returns
the same canonical `session.id` and caller correlation ID. If Claude rotates
its provider session ID while resuming, the Session is rekeyed when the
resumed execution starts, as described under [`restart`](#restart).
Plain `send` never launches and returns `SESSION_NOT_RUNNING` with a
`--resume` hint when its target is not live. dlgt holds one process per
provider conversation: a second `--resume` of a conversation that is already
live reuses that Session and never starts a second process on it.

Rules:

- The target Session must already exist.
- Launch, model, and environment options are accepted only with `--resume`;
  `--profile` is accepted only with `--resume` and must match the Session's
  Harness.
- The prompt is required. `--` is recommended so prompt text beginning with an
  option-like token is never parsed as a CLI option; multiple remaining words
  are joined with spaces.
- `--prompt-file`, `--stdin`, and a positional prompt are mutually exclusive.
  Prefer `--prompt-file` for long prompts from an agent-controlled shell so
  no shell pipe or redirection wraps the direct `dlgt send` invocation.
- If the Session is idle, dlgt accepts the prompt and transitions it to busy.
- A Codex `turn/started` reported without an active dlgt execution is retained
  as a new execution, so its later result remains observable.
- If the Session is busy, canceling, blocked, stopping, or stopped, plain
  `send` fails immediately and has no side effects. Busy and canceling return
  `SESSION_BUSY`; blocked returns `SESSION_BLOCKED` with an `answer` action;
  other non-idle states return `SESSION_UNAVAILABLE`.
- `--request-id` is required and behaves exactly as it does for `new`,
  including with `--resume` and `--steer`: the same ID and payload replay the
  original receipt with `"replayed": true`, and the same ID with a different
  payload is `INVALID_ARGUMENT`.
- There is no `--wait`, `--create`, `--enqueue`, `--after`, or `--fail-if-busy`.
  Creation, queueing, and waiting are not `send` responsibilities, and busy
  rejection is always the default.

Asynchronous example:

```bash
dlgt send codex:019f6307-341e-7e81-8a33-7ab61e804345 --request-id review-3 \
  -- "Review the revised design"
```

```json
{"ok":true,"session":{"id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","state":"busy"},"execution_seq":2,"submission":"confirmed","cursor":"4"}
```

Busy rejection:

```json
{"ok":false,"error":{"code":"SESSION_BUSY","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}}
```

Read the answer with one `fetch`:

```bash
dlgt send codex:019f6307-341e-7e81-8a33-7ab61e804345 --request-id review-4 \
  -- "Review the revised design" \
  && dlgt fetch codex:019f6307-341e-7e81-8a33-7ab61e804345 --wait 15m
```

### `send --steer`

`--steer` adds a message to the execution that is already running. It never
creates an execution, never queues work for later, and never interrupts.

```bash
dlgt send claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629 --steer \
  --request-id steer-1 -- "Skip the integration tests; unit tests are enough"
```

```json
{"ok":true,"session":{"id":"claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629","state":"busy"},"execution_seq":2,"steer":"accepted","cursor":"9"}
```

- The Session must be `busy`. An idle Session returns `NO_ACTIVE_EXECUTION`;
  send the prompt without `--steer` instead. A blocked Session returns
  `SESSION_BLOCKED`: answer the Request first. A canceling Session returns
  `SESSION_UNAVAILABLE`.
- Before Codex acknowledges a turn and supplies its provider turn ID, a steer
  returns `SESSION_UNAVAILABLE` and has no side effects.
- The response carries the running `execution_seq`. A steer has no result of
  its own; the execution still ends with exactly one retained result.
- Acceptance means dlgt handed the message to the provider and is recorded
  as a `steer` message with `delivered: false`. Delivery to the model is
  reported later as a second `steer` message with `delivered: true`. A steer
  the provider never read is reported with `expired: true` instead.

The two Harnesses deliver at different moments, and callers should not expect
them to match:

```text
Codex    turn/steer; the message joins the active turn immediately
Claude   the message is read at the next model request, which is after the
         tool call in flight returns. If the turn ends first, Claude runs the
         message as a continuation and dlgt keeps it in the same execution
```

For Claude, dlgt treats an execution as finished only when the provider has
reported a turn result and every accepted steer has been replayed back by the
provider. A steer therefore cannot be lost between two executions. When a
steer runs as a continuation, the execution's `final_text` is the text of each
provider turn result in order, joined by a blank line.

To replace the running work instead of adding to it, use `cancel` and then a
plain `send`.

## Retained result

Every accepted execution receives a per-Session monotonic `execution_seq`.
This number is returned by `new` or `send`, echoed by lifecycle events and the
retained result, and never accepted as a CLI selector. It is correlation data,
not a public execution resource or ID.

The retained result shape is:

```json
{
  "execution_seq": 2,
  "status": "completed",
  "final_text": "Review result...",
  "final_text_source": "provider",
  "denied_requests": 0,
  "error": null,
  "started_at_ms": 1784024104395,
  "completed_at_ms": 1784024252019,
  "usage": null
}
```

`status` is one of `completed`, `failed`, `canceled`, or `interrupted`.
`final_text` is the Harness-reported final assistant message and is always a
string for `completed`, although it may be empty. Failed terminal states may
provide partial final text and must provide an error object with `code` and
`message`, or `null` when there is no error. Usage is
nullable because availability differs by Harness: Claude reports its token
counts, and Codex reports none.

`denied_requests` counts approvals that were denied during the execution, by
the provider's reviewer or by the caller. A provider reports a turn as
successful even when a tool it needed was denied, so a `completed` result with
a non-zero count is not evidence that the work was done. The matching
`approval` messages say what was denied.

`final_text_source` is a diagnostic:

```text
provider     the Harness turn result reported the text
messages     the turn result carried no text and dlgt used the last assistant
             message the provider streamed during this execution
missing      no text was recovered; the execution status is still authoritative
```

A failed recovery never turns a completed execution into a failure.

## `fetch`

```text
dlgt fetch <SESSION_ID|@ALIAS>
  [--cursor <N>]
  [--wait <DURATION>]
  [--messages[=<MAX>] | --no-messages]
  [--max-bytes <BYTES>]
  [--pretty]
```

`fetch` is the one observation command. It returns, in a single JSON document,
the current Session snapshot, pending Requests, every terminal result and
lifecycle event after the cursor, the forward message delta, and the response's
cursor position.

The `from_start` parameter is available only through raw RPC; the CLI has no
`fetch --from-start` flag. Use cursors with `fetch`, or `watch --from-start`
when replaying retained messages for a person.

Every observation succeeds. A long poll that expires with nothing new is
`{"ok":true,"reason":"timeout"}` with empty deltas, not an error. The same
cursor position is returned whenever the underlying watermark vector did not
move; an empty public delta can still mint a new position when internal
bookkeeping advanced beneath it. `reason` is one of:

```text
snapshot    bounded baseline; no cursor was supplied
change      new events, results, or messages were delivered
result      the bound execution has a retained terminal result
blocked     the Session has a pending Request; it is included, and the
            response returns at once even with --wait
page_full   more data already exists beyond the returned cursor
gap         retention evicted part of the cursor window
timeout     nothing new in the cursor window
```

Rules:

- Without `--cursor`, `fetch` returns a bounded baseline: current state,
  pending Requests, the latest retained result, the last 64 messages, and a
  fresh position. This is the documented recovery path after a lost position
  or a lost response.
- `--wait` is optional. Omitted, the command returns immediately. Given, it
  requires an explicit duration and accepts up to 24h; there is no unbounded
  wait.
- `--wait` binds to the execution that is active, or latest, at the first
  evaluation and completes when that execution has a retained terminal result.
  A later execution never extends the bind. A pending Request, a page-full
  response, a retention gap, and the deadline all return early.
- New messages alone never complete a `--wait`. They are included when
  something else wakes the poll or the deadline fires.
- Replaying the same cursor replays the same immutable events, results, and
  messages. Nothing is consumed or advanced server-side; the Session snapshot
  and pending Requests are always current.

Response:

```json
{
  "ok": true,
  "schema_version": 2,
  "runtime": {"version":"0.6.0","instance_id":"1f2c…"},
  "reason": "result",
  "has_more": false,
  "gaps": [],
  "cursor": "4",
  "sessions": [
    {
      "session": {"id":"claude:8bc7859c","state":"idle","approvals":{"requested":"auto","effective":"auto"}},
      "requests": [],
      "events": [
        {"schema_version":2,"seq":104,"type":"session.idle","session_id":"claude:8bc7859c","execution_seq":7,"result_status":"completed"}
      ],
      "results": [
        {
          "execution_seq": 7,
          "status": "completed",
          "final_text": "Review result...",
          "final_text_source": "provider",
          "final_text_offset": 0,
          "final_text_complete": true,
          "denied_requests": 0,
          "error": null,
          "started_at_ms": 1784024104395,
          "completed_at_ms": 1784024252019,
          "usage": null
        }
      ],
      "messages": [
        {"seq":41,"execution_seq":7,"kind":"tool_call","tool":"Bash","summary":"cargo test"},
        {"seq":42,"execution_seq":7,"kind":"tool_result","tool":"Bash","ok":true,"summary":"42 passed"},
        {"seq":43,"execution_seq":7,"kind":"assistant","text":"Found one race."}
      ],
      "gaps": []
    }
  ]
}
```

### Messages

Messages are the structured replacement for the v1 screen. They are what the
provider reported, in order, reduced to a bounded form. They are for following
progress; the authoritative answer is always `results[].final_text`.

```text
assistant     text the model wrote between tool calls
tool_call     a tool invocation: tool name and a one-line summary of its input
tool_result   the outcome: ok or not, and a one-line summary
approval      a reviewer decision on an approval request
steer         a message accepted through send --steer
notice        a provider warning or a dlgt observation about the Session
```

```json
{"seq":57,"execution_seq":7,"kind":"approval","decision":"approved","by":"provider_reviewer","risk":"low","tool":"exec","summary":"sudo -n true"}
{"seq":58,"execution_seq":7,"kind":"approval","decision":"denied","by":"caller","request_id":"r3","tool":"Bash","summary":"git push --force origin main"}
{"seq":61,"execution_seq":7,"kind":"steer","delivered":true,"text":"Skip the integration tests; unit tests are enough"}
```

- `decision` is `approved`, `denied`, `timed_out`, or `aborted`. `denied` and
  `timed_out` count toward the result's `denied_requests`.
- `by` is `provider_reviewer`, `caller`, or `provider_policy`. dlgt is never a
  value of `by`.
- A `tool_result` `summary` is the first non-empty line of the tool's output,
  cut at 240 bytes. When there is no output it is the exit status.
- A `steer` message carries `delivered` and, when the provider never read it,
  `expired: true`.
- `text` and `summary` are truncated at 4 KiB with `"truncated": true`. A
  message is never split across responses. Full tool output is not retained.
- Model reasoning is not included.
- `--messages=<MAX>` caps messages per response (default 64, limit 512).
  `--no-messages` omits them; Requests, results, and events are unaffected.

### Bounds and pagination

Response size is part of the contract:

| Dimension | Default | Limit |
| --- | ---: | ---: |
| Serialized response | 32 KiB | 256 KiB |
| Long poll | none | 24h |
| Lifecycle events per page | 64 | 64 |
| Results per page | 4 | 4 |
| Messages per page | 64 | 512 |
| Pending Requests | 16 | 16 per response |

`has_more: true` means requested data already exists beyond the returned
cursor; that response uses `reason: "page_full"` and the next call returns
immediately even with `--wait`.

`--max-bytes` is a hard bound on the **complete compact response line**: the
`{"ok":true,...}` wrapper and its trailing newline are reserved before any
content is committed, and the finished line is measured. The cursor is derived
from what the document actually carries, so it can never advance past content
that was left out. When the budget squeezes a response, dlgt keeps state and
gaps, then pending Requests, then terminal results, then events, and drops
messages first. Requests are current state rather than cursor history, so they
are never paged: when more are pending than the response carries, the bucket
has `"requests_truncated": true` and the rest appear as the published ones
are answered. A Request's `input` larger than 2 KiB is replaced by
`{"truncated": true, "preview": "..."}`.

While a Session is blocked, `fetch` always includes an answerable minimum
Request with its ID, kind, tool, short summary, and the data required to answer
it. Every published field is bounded so the minimum Request fits under the
256 KiB hard limit. Choice indexes and form schemas remain exact;
dlgt truncates display text with a marker. If a form schema cannot fit,
dlgt declines it to the provider and records a `notice` instead of publishing a
changed schema. If a smaller `--max-bytes` cannot hold a Request, `fetch` fails
with `INVALID_ARGUMENT` and names a measured working budget; retry at that
value. A blocked response never reports an empty Request list as complete.

Three things are outside the bound, all deliberately:

- an optional `info` notice, which the daemon injects rarely and independently
  of the request (see `UPDATE_AVAILABLE` above);
- `--pretty`, which exists for humans and inflates the output arbitrarily;
- the raw JSONL RPC envelope, whose request `id` the caller chooses. See
  [RPC](rpc.md#transport-and-framing); that id is capped at 200 bytes.

The contract covers the compact CLI response without `info`.

Progress comes from chunking, never from oversizing. A long `final_text` is
chunked at a UTF-8 boundary and continued through `final_text_offset` and
`final_text_complete`. The continuation is carried in the cursor, so every
`has_more` response advances at least one watermark.

Lifecycle events and messages are each delivered as a strictly ascending
prefix: the response never skips one to deliver a later one, so each watermark
is always a position with nothing outstanding behind it.

If `--max-bytes` is too small to carry the response envelope plus one chunk of
progress, `fetch` fails with `INVALID_ARGUMENT` and names a budget verified to
work rather than emitting an oversized document. That number is obtained by
actually rendering a response at it, not estimated, so retrying at exactly the
reported value succeeds. It is a verified candidate, not a minimum: whether a
budget makes progress is not monotonic, so a value between the rejected budget
and the reported one may or may not work, and only the reported value carries
the guarantee. The practical floor is roughly 6 KiB for a single Session,
because one message can carry 4 KiB of text; the default of 32 KiB is far above
it.

### Cursors

A cursor is a position, not a token. Every acceptance, and every `fetch`
response whose watermark vector advanced, mints the addressed scope's next
position -- 1, 2, 3 -- and the daemon holds the watermarks behind it. A fetch
whose vector did not move returns the caller's own position; an empty public
delta can still mint a new position when internal bookkeeping advanced.
Leading zeros in a supplied position are accepted and canonicalized away.
```json
{"cursor":"4"}
```

Positions are deliberately small: a caller carries one across turns, and an
opaque 200-character token does not survive an agent's context. Over RPC the
value may be spelled as a JSON number or a string of digits; anything else is
`CURSOR_INVALID`. The number is
an ordinal within the scope you address, not a capability: `fetch B --cursor 3`
returns Session B's own third observation window, which is a legitimate replay,
not an error. Idempotency is carried by `--request-id`, never by the cursor.

Positions bind to an internal Session identity, so a Claude provider-ID
rotation does not invalidate them.

A position is meaningful only within one daemon lifetime. Nothing in the number
says which daemon minted it, so a number kept across a restart is not rejected:
it names the *new* daemon's window of that position, or fails `CURSOR_EXPIRED`
if that daemon has not minted that far. What comes back is always current data
-- the previous daemon's state is gone, so there is nothing stale to return --
but the number no longer means what you think it means. After
`RPC_UNAVAILABLE`, `SESSION_NOT_RUNNING`, or any daemon restart, drop
remembered positions and re-enter with `send --resume` (whose acceptance
carries a fresh position) or one cursorless `fetch`.

The daemon retains the most recent 64 positions per Session, within a global
cap of 4,096 across every scope; beyond the bound the oldest are dropped. A
response whose watermark vector did not move keeps
the position you sent instead of minting a new one, so an idle long poll
normally spends none of them; internal bookkeeping can still advance the
vector beneath an empty public delta and mint.

```text
CURSOR_EXPIRED   the position was never minted, or is no longer retained
CURSOR_INVALID   the value is not a number
```

Both are non-zero exits, and the recovery for both is a single cursorless
`fetch`.

### Retention gaps

Retention is bounded per daemon: 10,000 messages per Session, 50,000
lifecycle events, and 128 results or 16 MiB of result bodies per Session. When
a cursor predates an eviction, `fetch` exits 0 with `reason: "gap"`, a
structured `gaps` entry, a bounded baseline, and a fresh cursor. It never
silently resets.

```json
{"gaps":[{"component":"messages","reason":"retention_overrun"}]}
```

`component` is `messages`, `events`, or `results`. Per-Session gaps
(`messages`, `results`) appear on the Session bucket; scope-wide gaps
(`events`) appear in the top-level `gaps` array, so a gap cannot disappear with
a bucket that the response did not carry.

### Waiting from an agent harness

- Claude callers: run the long `fetch` through the harness's background
  mechanism. The acceptance receipt is already in hand from `new` or `send`,
  so a lost foreground response costs nothing.
- Codex callers: run the long `fetch` inside one exec cell and use a single
  long cell wait. Pre-yield output is retained and delivered at the next wait.
- After any lost response, run a cursorless `fetch` to rebase.

## Blocked input and `answer`

Input that only the caller can give is a first-class Session state, not a
failure and not an infinite wait. A Session is `blocked` while it has at least
one pending Request. There are two kinds:

```text
approval   the worker wants to run something that needs permission
question   the worker asked the user something and is waiting for the reply
```

`fetch` reports the state as `reason: "blocked"` and carries the Requests:

```json
{
  "requests": [
    {
      "id": "r3",
      "kind": "approval",
      "execution_seq": 7,
      "tool": "Bash",
      "summary": "git push --force origin main",
      "input": {"command": "git push --force origin main"},
      "cwd": "/work/repo",
      "reason": "May I force-push the rebased branch?",
      "created_at_ms": 1784024104395
    }
  ]
}
```

A question carries a short `summary` and `text`, plus either numbered choices
or a form `schema`. Choice objects keep their 1-based `index` when labels are
truncated. An MCP form's `schema` is the provider's `requestedSchema`,
unchanged. Display-only text is truncated with a marker. A JSON answer must be
an object, include every field in the schema's `required` list, and name only
fields in its `properties`. If the complete schema
cannot fit in a minimum Request, dlgt declines the provider request and records
a `notice`. `id` is a short per-Session ordinal, so it survives an agent's
context. When a provider asks several questions in one request, `text` lists
them one per line, there are no `choices`, and one `--text` answer is given to
all of them.

```text
dlgt answer <SESSION_ID|@ALIAS> <REQUEST_ID>
  (--allow | --deny [--message <TEXT>] | --text <TEXT> | --choice <N> | --json <OBJECT>)
  [--pretty]
```

```bash
dlgt answer claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629 r3 --deny \
  --message "Do not force-push. Open a new branch instead."
```

```json
{"ok":true,"session":{"id":"claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629","state":"busy"},"request":{"id":"r3","resolution":"denied"},"cursor":"11"}
```

Rules:

- `--allow` and `--deny` answer an `approval`. `--text` and `--choice` answer
  a `question`. `--json` answers an MCP form elicitation with a JSON object;
  required and unknown fields are checked before delivery. Values are checked
  by the provider, not by dlgt. The wrong pairing is
  `INVALID_ARGUMENT`.
- An allow applies to that one Request. There is no "allow for the rest of
  the Session"; use `--approvals` to change the policy.
- `--deny --message` gives the worker the reason. The worker continues its
  turn and normally reports that it could not do the step. Claude receives
  the message as the denial itself. Codex's decline carries no text, so dlgt
  sends the message through `turn/steer` before the decline when a provider
  turn is available. The receipt includes `reason_delivered: true|false`; if
  the reason could not be sent, `reason_error` explains why.
- `--choice <N>` is 1-based and answers with that choice's label. `--text`
  answers in free text. For an MCP form elicitation, use `--json
  '{"name":"Ada","color":"blue"}'`; missing required fields and fields the
  schema does not name are rejected before dlgt sends an answer.
- The Session must be live: a stopped Session returns `SESSION_NOT_RUNNING`.
- `answer` returns as soon as the provider accepted the answer. It does not
  wait for the execution; follow it with `fetch --wait`.
- An answer moves through `pending`, `sending`, `delivered`, and `failed`.
  While delivery is in progress, a duplicate answer waits for that attempt's
  outcome and never starts another one. The Request returns to `pending` only
  when dlgt knows no bytes reached the provider. A write whose outcome is
  unknown stops the provider transport and fails the Session with
  `REQUEST_DELIVERY_FAILED`, so the answer can never arrive later.
- Answering is idempotent by Request. Repeating the same answer returns the
  original receipt with `"replayed": true`. A different answer to a resolved
  Request is `REQUEST_RESOLVED`. An unknown ID is `REQUEST_NOT_FOUND`.
- While a Session is blocked, plain `send` and `send --steer` fail with
  `SESSION_BLOCKED` and name the pending Request IDs:

  ```json
  {
    "ok": false,
    "error": {
      "code": "SESSION_BLOCKED",
      "session_id": "claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629",
      "request_ids": ["r3"],
      "action": "dlgt answer claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629 r3 --allow|--deny"
    }
  }
  ```

- `cancel` closes every pending Request as `canceled` and interrupts the
  execution. `stop` and `restart` close them the same way.
- dlgt never infers completion, approval, or denial from provider silence.

## `cancel`

```text
dlgt cancel <SESSION_ID|@ALIAS> [--timeout <DURATION>]
```

`cancel` interrupts the active provider execution. It does not stop the
Session or its process. A successful cancellation terminalizes the current
result as `canceled` or `interrupted` according to the normalized provider
mapping and returns the Session to idle only after the provider has reported
the interrupted turn's result.

```text
Codex    turn/interrupt
Claude   interrupt control request on the same process
```

A steer that the provider had not yet read is discarded with the execution; it
never starts a turn of its own after a cancel. For Claude, dlgt first withdraws
each unread message with a `cancel_async_message` control request and then
interrupts. The discarded steer is reported as a `steer` message with
`expired: true`.

Cancellation is bounded and defaults to 30 seconds. On timeout, dlgt returns
`CANCEL_TIMEOUT`, leaves the Session in `canceling`, and continues observing
provider quiescence in the background. `fetch` reveals the eventual terminal
state. A successful `cancel` returns the result for the execution it captured,
even if another execution starts before the response is written. If the
provider process exits while a cancel waits, the cancel returns at once with
that execution's result and the Session's `failed` state.

The timeout includes dispatch and completion of all provider control messages.
If cancellation begins before Codex acknowledges the active turn, dlgt keeps
the cancel intent bound to that execution and applies `turn/interrupt` when the
turn ID arrives, within the same timeout. The Session remains unavailable until
its cancellation controls settle. A late control can never interrupt a later
execution.

Canceling an idle Session is idempotent: it returns exit 0 with
`{"canceled":false,"reason":"NO_ACTIVE_WORK"}`.

## Session commands

```text
dlgt list [--all] [--all-versions] [--pretty]
dlgt show <SESSION_ID|@ALIAS> [--pretty]
dlgt watch <SESSION_ID|@ALIAS> [--from-start] [--until-idle]
dlgt stop <SESSION_ID|@ALIAS> [--force]
dlgt restart <SESSION_ID> [environment options]
```

- `list` returns active Sessions.
- `list --all` includes terminal historical Sessions.
- `list --all-versions` queries every live versioned daemon socket and adds
  `runtime_version` and `runtime_socket` to each Session.
- `show` returns identity, Harness, model selection, the provider-resolved
  model as `resolved_model`, approvals, state, current timing, pending
  Requests, latest retained result, and relevant failure data.
  A Session that failed before it became ready also has `launch_failure` with
  the same `code`, `message`, `hint`, and `action` that the launch error
  returned. `show` also carries `resume_command`, the provider's own command
  for opening the conversation in its interactive UI.
- A Claude Session also has a stable `marker` such as `dlgt:8bc7859c4c82` in
  its snapshot and provider conversation name. Codex Sessions have no marker.
- `stop` requests graceful Session termination.
- `stop --force` terminates the provider process group.

The daemon owns each provider process in its own process group. A sibling
reaper has its own process group, ignores ordinary terminal and service
shutdown signals, and keeps provider groups registered until normal teardown.
If the daemon exits without running destructors, control-pipe EOF makes the
reaper terminate every remaining registered group.

### `watch`

`watch` is the human view. It prints a Session's messages as text and follows
new ones until interrupted.

```text
$ dlgt watch @run-review-361csx
[exec 7] tool   Bash  cargo test
[exec 7] ok     Bash  42 passed
[exec 7] claude Found one race.
[exec 7] ask    r3  Bash  git push --force origin main
[exec 7] deny   r3  by caller
[exec 7] done   completed
```

- It is read-only. It takes no lease, sends nothing to the provider, and any
  number of watchers may follow one Session. It polls `fetch` twice a second.
- By default it starts at the current position. `--from-start` replays the
  retained messages first.
- It exits when the Session reaches a terminal state, or with `--until-idle`
  as soon as the Session is idle.
- It prints text, not JSON, and is not a stable format. Agents use `fetch`.
- There is no way to type into a worker. To take a conversation over by hand,
  `stop` the Session and run its `resume_command`:

  ```json
  {"resume_command":"claude --resume 8bc7859c-4c82-4b9a-a00d-2f3c483a9629"}
  ```

## Lifecycle events

Lifecycle events are read through `fetch`, which returns them for one Session
alongside state, Requests, results, and messages. There is no separate event
command and no cross-Session event stream: `list` reports current state across
Sessions, and `fetch` reports the ordered history of one.

The versioned event schema and the complete event set are defined in
[RPC](rpc.md#lifecycle-events).

## Model discovery

```text
dlgt models [--include-hidden]
dlgt models --harness codex [--include-hidden]
dlgt models --harness claude
```

Without `--harness`, `dlgt models` queries every Harness and returns
`{"harnesses":[...]}`. A Harness that cannot be reached reports
`"discovery":"unavailable"` with its error rather than failing the command.

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
picker API. dlgt always returns the stable Claude Code aliases, then augments
them with current canonical IDs from the public, daily refreshed
[`claude-models-list`](https://github.com/combinatrix-ai/claude-models-list)
snapshot. Date-pinned IDs ending in `-YYYYMMDD` are normalized to their undated
alias, so `claude-haiku-4-5-20251001` is reported as `claude-haiku-4-5`. The
snapshot is account-scoped and is not presented as the Claude Code picker. If
it cannot be fetched or validated, dlgt falls back to the aliases and reports
discovery as `partial`.

```json
{
  "ok": true,
  "harness": "claude",
  "source": "https://raw.githubusercontent.com/combinatrix-ai/claude-models-list/main/models.json",
  "discovery": "snapshot",
  "models": [
    {"id":"default","kind":"alias","recommended":true},
    {"id":"best","kind":"alias"},
    {"id":"sonnet","kind":"alias"},
    {"id":"opus","kind":"alias"},
    {"id":"haiku","kind":"alias"},
    {"id":"claude-fable-5","display_name":"Claude Fable 5"}
  ]
}
```

Model and effort are optional at launch. Omission selects the provider's
recommended default. Profiles should prefer stable provider aliases unless an
exact version pin is required.

For an exact canonical Claude model ID, dlgt validates `--effort` against the
snapshot's `capabilities.effort` before starting Claude Code. Floating aliases
such as `default`, `opus`, and `sonnet` remain provider-validated because their
target model can change independently of the snapshot. If the snapshot is
unavailable, launch validation fails open and Claude Code remains authoritative.

Model aliases are resolved by the Harness when `new` launches the Session, not
on each `send`. dlgt does not silently pin a drifting alias; `show` reports the
provider-resolved model when the Harness makes it available.

## `doctor`

```text
dlgt doctor [--json] [--probe]
```

The default command is offline and read-only. It reports the running binary,
configuration parse status, every versioned daemon socket and its permissions,
Codex and Claude CLI versions, and whether each installed Skill is byte-for-byte
identical to the Skill embedded in this binary. Human-readable output is the
default; `--json` emits the same check records with `ok`, `warn`, `fail`, or
`skip` status.

`--probe` additionally initializes Codex app-server and runs `model/list`, then
checks the latest published dlgt release. It starts no model turn and performs
no repair. If no current daemon exists, the probe may leave the versioned
daemon running until its normal idle timeout. Every finding includes evidence
and, when applicable, one explicit recovery hint; `doctor` never silently
deletes sockets or overwrites Skills. The command exits successfully after
producing a complete report; automation should gate on the report's `status`,
not the top-level transport `ok` field.

## Profiles and launch environment

```text
dlgt profiles
dlgt profiles list
dlgt profiles show <NAME>
dlgt profiles defaults
```

Bare `dlgt profiles` is the same request as `dlgt profiles list`.

Profiles are client-side launch specifications. The client expands them before
RPC so the daemon does not need to reread mutable configuration.

The config file is `~/.config/dlgt/config.toml`; set `DLGT_CONFIG` to use a
different path. It may contain a top-level `[defaults]` table and
`[defaults.claude]` and `[defaults.codex]` subtables, alongside Profiles:

```toml
[defaults]
approvals = "auto"

[defaults.claude]
model = "sonnet"
effort = "high"
auth = "subscription"
unset_env = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
pass_env = []
clean_env = false

[defaults.codex]
model = "gpt-6-luna"
effort = "max"

[profiles.fable-review]
harness = "claude"
model = "best"
effort = "high"
approvals = "auto"
harness_options = ["chrome"]
clean_env = true
pass_env = ["PATH", "HOME", "SSH_AUTH_SOCK"]

[profiles.codex-live-search]
harness = "codex"
harness_options = ['web_search="live"']
```

`[defaults]` accepts only `approvals`. A harness subtable or Profile accepts
`model`, `effort`, `approvals`, `clean_env`, `pass_env`, `unset_env`, `env`,
and `harness_options`; `auth` is Claude-only. `env` accepts a TOML table or an
array of `KEY=VALUE` strings. Unknown keys and harness names are configuration
errors, and `doctor` reports them in its config check.

Launch precedence:

```text
built-in < [defaults] < [defaults.<harness>] < Profile < explicit CLI flag
```

`pass_env`, `unset_env`, and `harness_options` lists concatenate in
precedence order. Scalars are replaced by each higher layer. Environment
assignments merge by key, with higher layers replacing the same key.
`dlgt profiles defaults` shows the resolved defaults for both harnesses.

Profile and default `harness_options` are followed by explicit
`--harness-option` values.
They configure the provider CLI rather than the launch environment. Claude
bare entries become `--KEY` and keyed entries become `--KEY=VALUE`; Codex
requires keyed entries and converts them to `--config KEY=VALUE`. An explicit
`--approvals` replaces the Profile's `approvals`. Explicit CLI flags replace
scalar Profile and default values.

- Default launch environment is a snapshot of the invoking client's
  environment, never the daemon's startup environment.
- `--clean-env` starts from a minimal runtime base.
- `--pass-env KEY` copies one client value with `--clean-env`.
- `--env KEY=VALUE` sets or overrides a value.
- `--unset-env KEY` removes a value.
- `pass_env` has the same effect as repeated `--pass-env` and only applies
  with `clean_env`.
- Environment options apply when creating or restarting a Session. Values are
  freshly snapshotted for each process launch and are never stored for replay.
- dlgt applies final lifecycle safety overrides to owned Harness children:
  `check_for_update_on_startup=false` for Codex and `DISABLE_AUTOUPDATER=1` for
  Claude. They cannot be overridden per Session and do not change provider
  global configuration.
- The Session snapshot reports how Claude authenticated, as the provider
  itself reported it:

  ```json
  {"auth":"subscription","auth_policy":"subscription"}
  ```

  `auth` is `subscription` or `api_key`. A client environment that carries
  a non-empty `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` makes Claude use
  that key instead of the subscription. `auth_policy` is Claude's retained
  `subscription` or `any` launch policy and is shown by `show`; it defaults to
  `subscription`, including when no config file exists. Use `--auth
  subscription|any` on `new` or `send --resume` to override it; `send --resume`
  retains the Session's policy when no explicit `--auth` or Profile value is
  supplied. Built-in and `[defaults]` values do not replace the stored policy.
  Under `subscription`, dlgt runs `claude auth status` with inherited
  environment variables cleared and requires `loggedIn: true` and
  `authMethod: "claude.ai"`. The non-interactive status command reports its
  result as JSON; see the [Claude Code CLI reference](https://code.claude.com/docs/en/cli-reference).
  Preflight also rejects non-empty API, custom endpoint, and provider-routing
  variables, including `ANTHROPIC_BASE_URL` and truthy `CLAUDE_CODE_USE_*`
  flags. The provider variable list follows the current [Claude Code
  environment-variable reference](https://code.claude.com/docs/en/env-vars).
  `AUTH_MISMATCH` names every offending variable without printing its value;
  remove each one with its matching `--unset-env <NAME>` option (or
  `unset_env` in `[defaults.claude]`), or explicitly opt out with `--auth any`.
  As a backstop, `system/init` must include `apiKeySource: "none"`; a missing
  value or any other value causes dlgt to interrupt the turn, record a
  structured failure, and stop the Session.
  The same `AUTH_MISMATCH` is returned when this occurs during the `new` or
  `send --resume` confirmation window. The observed `auth` and existing API-key
  `notice` remain available in the stopped Session. With `any`, dlgt allows the
  run and reports `auth: "api_key"` plus the notice. Claude reports `auth` on
  the first turn of a process, so that field is absent until then. Codex
  Sessions have no `auth` or `auth_policy` field.
- Launch environment values are passed in RPC memory, never argv, and are never
  directly serialized into Session records, `list`, `show`, `fetch`, Profiles,
  or error JSON. Provider output is untrusted and can deliberately echo its
  environment, so results and messages must be treated as potentially
  sensitive output rather than as a redaction boundary.

## Exit statuses

```text
0  command succeeded, including every fetch observation
1  usage, configuration, identity, launch, cursor, or RPC error
3  bounded cancellation timeout; the cancellation continues
4  Session is blocked on a pending Request
5  Session is busy and rejected a send
```

The JSON error code is the primary machine-readable reason. Exit status is the
shell-level summary. `SESSION_BLOCKED` uses exit 4 and `SESSION_BUSY` uses exit
5. `NO_ACTIVE_EXECUTION`, `REQUEST_NOT_FOUND`, `REQUEST_RESOLVED`,
`ALIAS_IN_USE`, `SESSION_NOT_RUNNING`, `SESSION_UNAVAILABLE`, `CURSOR_EXPIRED`,
and `CURSOR_INVALID` use exit 1.
`CANCEL_TIMEOUT` uses exit 3. Idle `cancel` is an
idempotent exit-0 no-op.
`AUTH_MISMATCH` uses exit 1.

`fetch` never uses a non-zero exit to describe an execution. A failed,
canceled, or interrupted execution is reported inside `results[].status`, and a
blocked Session is reported as `reason: "blocked"`.

The stable v2 structured error-code families are:

```text
INVALID_ARGUMENT       Invocation cannot be retried unchanged
NOT_FOUND              Session or configuration object does not exist
NO_ACTIVE_EXECUTION    send --steer found no running execution
SESSION_NOT_RUNNING    No live Session matches; retry with --resume
ALIAS_IN_USE           Exact Alias belongs to a non-terminal Session
SESSION_BUSY           Active execution; retry after it terminalizes
SESSION_BLOCKED        A Request is pending; answer it
REQUEST_BUSY           An identical answer is still being delivered
REQUEST_DELIVERY_FAILED Answer delivery failed; an ambiguous write fails the Session
REQUEST_FAILED         Request delivery failed and the provider Session is unavailable
SESSION_UNAVAILABLE    Session state cannot accept the requested operation
REQUEST_NOT_FOUND      No such Request on this Session
REQUEST_RESOLVED       The Request already has a different answer
CURSOR_EXPIRED         Cursor position was never minted or is no longer held
CURSOR_INVALID         Cursor value is not a position number
CANCEL_TIMEOUT         Cancel wait expired; cancellation continues
LAUNCH_FAILED          Harness startup or initial prompt acceptance failed
AUTH_MISMATCH          Claude subscription auth failed; blocking variables are named when present
RPC_UNAVAILABLE        Daemon transport is unavailable; retry may succeed
INTERNAL               dlgt runtime invariant failure
```

Commands may add contextual fields, but must not overload one code with a
different retry or human-action policy.

If `new` fails before provider binding, its error includes the temporary
`launch_id` for diagnostics and never presents it as a Session ID. A failure
after binding includes the canonical `session_id`.

A launch failure carries the provider's stderr tail in `detail`, and a `hint`.
There is no `LAUNCH_BLOCKED`: without a terminal there is no startup screen
for a human to answer, so a provider that cannot start without interaction is
a `LAUNCH_FAILED` whose `action` names the provider command, `claude` or
`codex`, that the user runs once by hand in the Session's working directory.
A provider process that dies later fails the Session, and its stderr tail is
kept as a `notice` message.

## Design and RPC contracts

The provider lifecycle mapping, acceptance criteria, and design rationale are
in [Design](design.md). The public JSONL method set and schemas are in
[RPC](rpc.md).

## Open questions

The contract above rests on measurements taken on 2026-11-15 with Claude Code
2.1.283 and codex-cli 0.167.0-alpha.4.1, through the probe scripts and through
this implementation against both providers. These points were not measured and
can change the contract:

- **Claude auto mode, blocked action.** Whether an action that Claude's auto
  mode classifier stops arrives as an approval Request or is denied in place.
  dlgt handles both: a `can_use_tool` request becomes a Request, and a
  `permission_denied` event becomes a `denied` approval message. Only allowed
  actions were observed.
- **Codex auto review, denial.** The protocol schema gives an auto review four
  terminal states (`approved`, `denied`, `timedOut`, `aborted`) and none that
  hands the request to the client, so in `auto` a Codex approval is expected
  never to become a Request. An approval by the reviewer was observed; a
  denial was not.
- **Concurrent Requests.** More than one pending Request on a Session, for
  example from parallel tool calls or from a provider-side subagent. dlgt
  keeps any number pending and answers each by ID, but only one at a time was
  observed.
- **Other Request kinds.** Codex file-change and permission approvals, Codex
  `request_user_input` questions, and MCP elicitations are mapped from the
  protocol schema and covered by fixtures. Only command approvals on Codex,
  and tool approvals and `AskUserQuestion` on Claude, were observed live.
- **`bypass`.** The provider arguments are unit-tested. No Session was run in
  `bypass` against a real provider.
- **Steer and cancel during a provider-side subagent.** Both were measured
  only on a top-level tool call.
- **Claude background tasks.** Claude can end a turn while a command it
  started keeps running in the background. No provider turn was seen to start
  on its own when such a command finished, but the wait was one minute. A
  result from a turn dlgt did not start is ignored rather than attributed to
  an execution.
- **Claude Code version drift.** Stream-json input, its control requests, and
  the `command_lifecycle` events are not a documented stable interface. A
  Claude that does not answer the `initialize` control request is a
  `LAUNCH_FAILED`. dlgt still needs a minimum supported version and a `doctor`
  check.
- **Claude session ID rotation.** `--resume` kept the same session ID in every
  run. The rekey path is covered by fixtures only.
- **Codex hooks that need review.** v1 stops at a `LAUNCH_BLOCKED` screen.
  `codex exec` skips untrusted hooks silently; the app-server path without a
  TUI was not measured.
- **Claude authentication policy.** Subscription authentication is the
  built-in default. A non-empty `ANTHROPIC_API_KEY` or
  `ANTHROPIC_AUTH_TOKEN` is rejected before launch unless the caller removes
  it with `--unset-env`/`unset_env` or opts out with `--auth any`;
  `system/init` is also checked before the first turn is accepted.
- **Interactive-only features.** Which user slash commands, skills, plugins,
  and hooks load under `claude -p` with stream-json.
