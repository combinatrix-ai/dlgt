# dlgt CLI v1 reference

Status: implemented public contract for the repository binary.

This reference is also published as raw Markdown at
https://combinatrix.ai/dlgt/cli.md for agents to fetch with curl.

This document is the normative command reference. See [RPC](rpc.md) for the
programmatic interface and [Design](design.md) for the product boundary,
invariants, lifecycle rationale, security model, and acceptance criteria.

## Product definition

`dlgt` is a local, single-binary runtime for live, addressable, and
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
correlation while the daemon is alive, but they are not public CLI resources and do not
have public IDs.

Other terms:

```text
Harness   The provider adapter, initially codex or claude
Profile   A reusable client-side launch specification
Alias     A human-readable address for an active Session
Title     A non-unique human description used to generate an Alias
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
`error.launch_id` when startup fails before provider binding.

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
dlgt - local subagent runtime

USAGE
  dlgt <COMMAND> [OPTIONS]

DELEGATION
  new          Create a Session with its first prompt
  restart      Restart a Session
  send         Send work to an existing idle Session or --resume a provider conversation
  fetch        Read new state, results, events, and screen from a cursor
  cancel       Interrupt the active execution

SESSIONS
  list, ls     List Sessions
  show         Show Session state and latest result
  attach       Attach to the Session screen
  stop         Stop the Session

OBSERVABILITY
  events       Read or follow lifecycle events
  scrollback   Read rendered terminal scrollback
  logs         Read raw retained PTY bytes (requires --raw)

CONFIGURATION
  models       Discover Harness models
  profiles     List or inspect Profiles
  harnesses    List Harness capabilities
  skill        Print the embedded dlgt skill

RUNTIME
  server       Run or stop the daemon
  update       Install the latest release and embedded Skills
  rpc          Use JSONL RPC
```

Each release uses its own socket at
`$DLGT_HOME/run/<version>/dlgt.sock`. Most commands address the daemon for the
invoking binary's version. `send` first scans all live versioned sockets for an
exact provider-qualified Session ID and routes to its owning daemon; multiple
matches fail rather than choosing or launching. `dlgt list
--all-versions` queries every currently running version and annotates each
Session with `runtime_version` and `runtime_socket`. Session state, results,
events, and terminal history exist only while their owning daemon remains
alive.

Busy Session snapshots include two integer diagnostics when `state` is exactly
`busy`:

```json
{
  "state": "busy",
  "busy_for_ms": 183000,
  "pty_quiet_for_ms": 72000
}
```

`busy_for_ms` measures from reservation of the current turn. `pty_quiet_for_ms`
measures PTY silence, clamped to that same busy interval so output from an
earlier turn cannot inflate it. If no PTY output has occurred, it equals
`busy_for_ms`. These counters are diagnostic only; PTY silence never changes
Session state or makes a busy Session accept another prompt. Both fields are
omitted for every other state.

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

## `new`

`new` is the only Session creation command and always requires its first
prompt. Launch and acceptance are one atomic operation, and the command always
returns as soon as the prompt is accepted. Observation is a separate `fetch`.

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
  [--request-id <ID>]
  [--stdin | -- <PROMPT>]   (required)
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
- Session creation and acceptance of the first prompt are one atomic daemon
  operation; omitting it returns `INVALID_ARGUMENT`.
- `--stdin` reads the exact prompt from standard input and is mutually exclusive
  with a prompt after `--`. It avoids argv disclosure and length limits.
- Use `--stdin` when the required prompt should not appear in argv.
- `--request-id` is an optional idempotency key. Retrying the same ID with a
  byte-identical payload returns the original acceptance receipt with
  `"replayed": true` instead of creating a second Session, even if the Session
  has since moved on. The same ID with a different payload is
  `INVALID_ARGUMENT`. The daemon retains the last 1,024 receipts for its
  lifetime. Use it whenever a caller cannot tell whether an earlier `new`
  reached the daemon; never re-issue a bare `new` on that ambiguity.
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
  --cwd . \
  -- "Review the current design"
```

The unsafe full bypass remains available only when deliberately requested:

```bash
dlgt new \
  --title "unrestricted Claude worker" \
  --harness claude \
  --harness-option dangerously-skip-permissions=true \
  --cwd . \
  -- "Review the current design"
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
    "id": "codex:019f6307-341e-7e81-8a33-7ab61e804345",
    "alias": "@run-review-361csx",
    "title": "run review",
    "harness": "codex",
    "state": "busy"
  },
  "execution_seq": 1,
  "cursor": "f1.eyJ2IjoxLCJi..."
}
```

`cursor` is an observation cursor positioned immediately before this
acceptance. It is captured under the runtime lock before the prompt is
recorded, so passing it to `fetch --cursor` cannot miss output a fast provider
emitted in between. Follow the acceptance with:

```bash
dlgt fetch codex:019f6307-341e-7e81-8a33-7ab61e804345 \
  --cursor "f1.eyJ2IjoxLCJi..." --until result --wait 15m
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
retained history, execution sequence, and provider conversation. Codex normally
keeps the same Session ID. If Claude reports a rotated provider session ID,
dlgt atomically rekeys the Session and returns the new canonical `session.id`.

Rules:

- Active `idle`, `busy`, and `blocked` Sessions may be restarted, as may
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
- Existing results, events, raw output, and scrollback remain readable; new
  executions continue the same monotonic `execution_seq`.

## `send`

```text
dlgt send <SESSION_ID|@ALIAS>
  [--request-id <ID>]
  [--pretty]
  [--stdin | -- <PROMPT>]   (required)
```

Resume a provider conversation after its owning daemon exits with the same
Session ID returned by `new`:

```text
dlgt send <codex:PROVIDER_THREAD_ID|claude:PROVIDER_SESSION_ID> --resume
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
  [--request-id <ID>]
  [--pretty]
  [--stdin | -- <PROMPT>]   (required)
```

The launch options above are accepted only with `--resume`, and `--harness`
is rejected because the provider prefix selects the Harness. A matching live Session is reused; if
none exists dlgt reserves the provider conversation, launches a new Session,
waits for bind/readiness, and atomically accepts the prompt. Success returns
the same canonical `session.id` and caller correlation ID. If Claude rotates
its provider session ID while resuming, success returns that new canonical ID.
Plain `send` never launches and returns `SESSION_NOT_RUNNING` with a
`--resume` hint when its target is not live.

Rules:

- The target Session must already exist.
- Launch, model, and environment options are accepted only with `--resume`;
  Profile options are not accepted by `send`.
- The prompt is required. `--` is recommended so prompt text beginning with an
  option-like token is never parsed as a CLI option; multiple remaining words
  are joined with spaces.
- `--stdin` is the mutually exclusive safe path for long or sensitive prompts.
- If the Session is idle, dlgt accepts the prompt and transitions it to busy.
- If the Session is busy, canceling, blocked, stopping, stopped, or
  attached, the command fails immediately and has no side effects. Busy and
  canceling return `SESSION_BUSY`; blocked returns `SESSION_BLOCKED`; attached
  returns `SESSION_ATTACHED`; other non-idle states return
  `SESSION_UNAVAILABLE`.
- `--request-id` behaves exactly as it does for `new`: the same ID and payload
  replay the original receipt with `"replayed": true`, and the same ID with a
  different payload is `INVALID_ARGUMENT`.
- There is no `--wait`, `--create`, `--enqueue`, `--after`, or `--fail-if-busy`.
  Creation, queueing, and waiting are not `send` responsibilities, and busy
  rejection is always the default.

Asynchronous example:

```bash
dlgt send codex:019f6307-341e-7e81-8a33-7ab61e804345 -- "Review the revised design"
```

```json
{"ok":true,"session":{"id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","state":"busy"},"execution_seq":2,"cursor":"f1.eyJ2IjoxLCJi..."}
```

Busy rejection:

```json
{"ok":false,"error":{"code":"SESSION_BUSY","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}}
```

Read the answer with one `fetch`:

```bash
dlgt send codex:019f6307-341e-7e81-8a33-7ab61e804345 -- "Review the revised design" \
  && dlgt fetch codex:019f6307-341e-7e81-8a33-7ab61e804345 --until result --wait 15m
```

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
  "final_text_source": "hook",
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

`final_text_source` is a diagnostic:

```text
hook         the Harness lifecycle event reported the text
transcript   the Harness reported nothing and dlgt recovered the text from
             the Session's own provider transcript, bounded to this execution
missing      no text was recovered; the execution status is still authoritative
```

A failed recovery never turns a completed execution into a failure.

## `fetch`

```text
dlgt fetch (<SESSION_ID|@ALIAS> | --all)
  [--cursor <CURSOR>]
  [--wait <DURATION>]
  [--until any|result]
  [--screen[=<MAX_STABLE_LINES>] | --no-screen]
  [--max-bytes <BYTES>]
  [--pretty]
```

`fetch` is the one observation command. It returns, in a single JSON document,
the current Session snapshot, every terminal result and lifecycle event after
the cursor, the forward screen delta, and a new cursor.

Every observation succeeds. A long poll that expires with nothing new is
`{"ok":true,"reason":"timeout"}` with empty deltas and the same cursor
position, not an error. `reason` is one of:

```text
snapshot    bounded baseline; no cursor was supplied
change      new events, results, or stable rows were delivered
result      the bound execution has a retained terminal result
blocked     the Session needs a human answer; the live screen is included
page_full   more data already exists beyond the returned cursor
gap         retention evicted part of the cursor window
timeout     nothing new in the cursor window
```

Rules:

- Without `--cursor`, `fetch` returns a bounded baseline: current state, the
  latest retained result, the last 128 stable screen lines, the live screen,
  and a fresh cursor. This is the documented recovery path after a lost
  cursor or a lost response.
- `--wait` requires an explicit duration and accepts up to 24h. Omitted, the
  command returns immediately.
- `--until result` binds to the execution that is active, or latest, at the
  first evaluation and completes when that execution has a retained terminal
  result. A later execution never extends the bind. Blocked input, a page-full
  response, a retention gap, and the deadline all return early.
- A live-screen repaint alone never completes a `--wait`. Spinner redraws are
  included opportunistically when something else wakes the poll or the deadline
  fires.
- Replaying the same cursor replays the same immutable events, results, and
  stable rows. Nothing is consumed or advanced server-side; the Session
  snapshot and live screen are always current.
- `--all` covers every Session of the addressed daemon. It returns one bucket
  per Session and pages at 32 Sessions. A cursorless `--all` enumerates every
  Session, carrying its position in the cursor and staying in baseline mode
  with `has_more: true` until enumeration completes; after that, only Sessions
  with changes are returned. Screen aggregation and `--until result` are
  rejected with `INVALID_ARGUMENT`.

Response:

```json
{
  "ok": true,
  "schema_version": 1,
  "runtime": {"version":"0.4.0","instance_id":"1f2c…"},
  "reason": "result",
  "has_more": false,
  "gaps": [],
  "cursor": "f1.eyJ2IjoxLCJi...",
  "sessions": [
    {
      "session": {"id":"claude:8bc7859c","state":"idle"},
      "events": [
        {"schema_version":1,"seq":104,"type":"session.idle","session_id":"claude:8bc7859c","execution_seq":7,"result_status":"completed"}
      ],
      "results": [
        {
          "execution_seq": 7,
          "status": "completed",
          "final_text": "Review result...",
          "final_text_source": "hook",
          "final_text_offset": 0,
          "final_text_complete": true,
          "error": null,
          "started_at_ms": 1784024104395,
          "completed_at_ms": 1784024252019,
          "usage": null
        }
      ],
      "screen": {
        "epoch": 3,
        "reset": false,
        "reset_reason": null,
        "stable": ["Checking tests...", "Found one race."],
        "live": ["Writing final review..."],
        "live_truncated": false
      },
      "gaps": []
    }
  ]
}
```

### Bounds and pagination

Response size is part of the contract:

| Dimension | Default | Limit |
| --- | ---: | ---: |
| Serialized response | 32 KiB | 256 KiB |
| Long poll | none | 24h |
| Lifecycle events per page | 64 | 64 |
| Results per page | 4 | 4 |
| Stable screen lines per page | 128 | 512 |
| Live rows | current screen, cropped to 40 | 40 |
| Changed Sessions in `--all` | 32 | 32 |

`has_more: true` means requested data already exists beyond the returned
cursor; that response uses `reason: "page_full"` and the next call returns
immediately even with `--wait` or `--until result`.

`--max-bytes` is a hard bound on the **complete compact response line**: the
`{"ok":true,...}` wrapper and its trailing newline are reserved before any
content is committed, and the finished line is measured. The cursor is derived
from what the document actually carries, so it can never advance past content
that was left out. When the budget squeezes a response, dlgt keeps state, then
gaps, then terminal results, then events, then blocked information, and drops
screen text first.

Two things are outside the bound, both deliberately:

- an optional `info` notice, which the daemon injects rarely and independently
  of the request (see `UPDATE_AVAILABLE` above);
- `--pretty`, which exists for humans and inflates the output arbitrarily.

The contract covers the compact response without `info`.

Progress comes from chunking, never from oversizing. A long `final_text` is
chunked at a UTF-8 boundary and continued through `final_text_offset` and
`final_text_complete`; a screen row wider than the remaining budget is chunked
mid-line. Both continuations are carried in the cursor, so every `has_more`
response advances at least one watermark.

`screen.stable` always contains complete rows and nothing else. A row that had
to be split is carried in one of two explicitly ordered slots:

```json
{
  "screen": {
    "fragment_before": {"row_id": 8412, "offset": 4096, "text": "...", "complete": true},
    "stable": ["Found one race."],
    "fragment_after": {"row_id": 8414, "offset": 0, "text": "...", "complete": false}
  }
}
```

- `fragment_before` continues the row the *previous* response split. It
  logically precedes `stable[0]` and is the only place `complete: true`
  appears.
- `fragment_after` is the row *this* response had to split. It logically
  follows the last entry of `stable` and is never complete.

The screen delta of one response is therefore, in order: `fragment_before`,
then every row of `stable`, then `fragment_after`.

Callers **must** retain every fragment piece: concatenate the pieces for a
given `row_id` in `offset` order until one arrives with `complete: true`, which
yields the whole row. Discarding pieces until a complete one arrives loses
data, because the completing piece carries only its own tail. A row never
appears in both `stable` and a fragment slot.

Lifecycle events are delivered as a strictly ascending prefix: the response
never skips an event to deliver a later one, so the event watermark is always
a position with nothing outstanding behind it.

If `--max-bytes` is too small to carry the response envelope plus one chunk of
progress, `fetch` fails with `INVALID_ARGUMENT` and names the smallest budget
that would work rather than emitting an oversized document. That number is
obtained by rendering the minimal response, not estimated, so retrying at
exactly the reported value succeeds. The practical floor is roughly 1 KiB for a
single Session; the default of 32 KiB is far above it.

A `--all` cursor carries watermarks for at most 256 Sessions. A daemon holding
retained state for more than that rejects `--all` with `INVALID_ARGUMENT` and
must be read one Session at a time.

### Cursors

A cursor is opaque. It begins with `f1.` and encodes the codec version, the
daemon boot identity, the addressed scope, and the per-Session watermarks. It
binds to an internal Session identity, so a Claude provider-ID rotation does
not invalidate it.

```text
CURSOR_VERSION_UNSUPPORTED   the prefix or payload version is not understood
CURSOR_EXPIRED               the cursor belongs to a previous daemon instance
CURSOR_SCOPE_MISMATCH        the cursor addresses a different Session or --all
CURSOR_INVALID               the payload is not a decodable cursor
```

All four are non-zero exits, and the recovery for every one of them is a single
cursorless `fetch`.

### Retention gaps

Retention is bounded per daemon: 10,000 stable screen rows per Session, 50,000
lifecycle events, and 128 results or 16 MiB of result bodies per Session. When
a cursor predates an eviction, `fetch` exits 0 with `reason: "gap"`, a
structured `gaps` entry, a bounded baseline, and a fresh cursor. It never
silently resets.

```json
{"gaps":[{"component":"screen","reason":"retention_overrun"}]}
```

`component` is `screen`, `events`, or `results`. Per-Session gaps (`screen`,
`results`) appear on the Session bucket; scope-wide gaps (`events`) appear in
the top-level `gaps` array, so a gap cannot disappear with a bucket that the
response did not carry.

### Waiting from an agent harness

- Claude callers: run the long `fetch` through the harness's background
  mechanism. The acceptance receipt is already in hand from `new` or `send`,
  so a lost foreground response costs nothing.
- Codex callers: run the long `fetch` inside one exec cell and use a single
  long cell wait. Pre-yield output is retained and delivered at the next wait.
- After any lost response, run a cursorless `fetch` to rebase.

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
provider quiescence in the background. `fetch` reveals the eventual terminal
state.

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
    "session_id": "codex:019f6307-341e-7e81-8a33-7ab61e804345",
    "action": "dlgt attach codex:019f6307-341e-7e81-8a33-7ab61e804345"
  }
}
```

`fetch` reports the same state as `reason: "blocked"` with the live screen
attached, so the caller can see the question before attaching. After a human
attaches, answers, and detaches, the same `fetch` may be issued again.
Provider-specific detection may initially be conservative, but dlgt must never
infer completion from a quiet screen.

## Session commands

```text
dlgt list [--all] [--all-versions] [--pretty]
dlgt show <SESSION_ID|@ALIAS> [--pretty]
dlgt attach <SESSION_ID|@ALIAS> [--steal]
dlgt stop <SESSION_ID|@ALIAS> [--force]
dlgt restart <SESSION_ID> [environment options]
```

- `list` returns active Sessions.
- `list --all` includes terminal historical Sessions.
- `list --all-versions` queries every live versioned daemon socket and adds
  `runtime_version` and `runtime_socket` to each Session.
- `show` returns identity, Harness, model selection, state, current timing,
  latest retained result, and relevant failure data.
- `attach` takes an exclusive input lease, replays the retained terminal view,
  and follows the live PTY. It requires an interactive terminal on stdin and
  stdout and otherwise returns `ATTACH_REQUIRES_TTY` pointing at `fetch`. A
  second attach returns `ALREADY_ATTACHED` unless `--steal` explicitly
  transfers the lease. Detach with `Ctrl-b d`. Mirrored multi-attach is
  outside v1.
- `stop` requests graceful Session termination.
- `stop --force` terminates the provider process group.

The daemon owns provider PTYs and the Codex app-server in separate process
groups. A sibling reaper has its own process group, ignores ordinary terminal
and service shutdown signals, and keeps provider groups registered until
normal teardown. If the daemon exits without running destructors,
control-pipe EOF makes the reaper terminate every remaining registered group.

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
rendered rows per Session. Rows are served from the same persistent stable-row
store that backs `fetch`, so a read no longer re-renders retained raw bytes.
The response includes the terminal dimensions, plain-text lines, truncation
state, and an opaque cursor for older pages. `scrollback` is the human
debugging view; agents should use `fetch`, which returns the forward delta
instead of an overlapping tail.

```json
{
  "ok": true,
  "session_id": "codex:019f6307-341e-7e81-8a33-7ab61e804345",
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

## Profiles and launch environment

```text
dlgt profiles
dlgt profiles list
dlgt profiles show <NAME>
dlgt harnesses [<HARNESS>]
```

Bare `dlgt profiles` is the same request as `dlgt profiles list`.

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
0  command succeeded, including every fetch observation
1  usage, configuration, identity, launch, cursor, or RPC error
3  bounded cancellation timeout; the cancellation continues
4  Session is blocked on input
5  Session is busy and rejected a send
```

The JSON error code is the primary machine-readable reason. Exit status is the
shell-level summary. `SESSION_BLOCKED` uses exit 4 and `SESSION_BUSY` uses exit
5. `NO_RESULT`, `SESSION_ATTACHED`, `ALREADY_ATTACHED`, `ATTACH_REQUIRES_TTY`,
`ALIAS_IN_USE`, `SESSION_NOT_RUNNING`, `SESSION_UNAVAILABLE`, and every
`CURSOR_*` code use exit 1. `CANCEL_TIMEOUT` uses exit 3. Idle `cancel` is an
idempotent exit-0 no-op.

`fetch` never uses a non-zero exit to describe an execution. A failed,
canceled, or interrupted execution is reported inside `results[].status`, and a
blocked Session is reported as `reason: "blocked"`.

The stable v1 structured error-code families are:

```text
INVALID_ARGUMENT       Invocation cannot be retried unchanged
NOT_FOUND              Session or configuration object does not exist
NO_RESULT              Session has never accepted work
SESSION_NOT_RUNNING    No live Session matches; retry with --resume
ALIAS_IN_USE           Exact Alias belongs to a non-terminal Session
SESSION_BUSY           Active execution; retry after it terminalizes
SESSION_BLOCKED        Human input is required
SESSION_ATTACHED       Exclusive attach lease prevents semantic send
SESSION_UNAVAILABLE    Session state cannot accept the requested operation
ALREADY_ATTACHED       Another client owns the attach lease
ATTACH_REQUIRES_TTY    attach needs an interactive terminal; use fetch
CURSOR_VERSION_UNSUPPORTED  Cursor codec is not understood
CURSOR_EXPIRED         Cursor belongs to a previous daemon instance
CURSOR_SCOPE_MISMATCH  Cursor addresses a different Session or scope
CURSOR_INVALID         Cursor payload is not decodable
CANCEL_TIMEOUT         Cancel wait expired; cancellation continues
LAUNCH_FAILED          Harness startup or initial prompt acceptance failed
RPC_UNAVAILABLE        Daemon transport is unavailable; retry may succeed
INTERNAL               dlgt runtime invariant failure
```

Commands may add contextual fields, but must not overload one code with a
different retry or human-action policy.

If `new` fails before provider binding, its error includes the temporary
`launch_id` for diagnostics and never presents it as a Session ID. A failure
after binding includes the canonical `session_id`.

## Design and RPC contracts

The provider lifecycle mapping, acceptance criteria, and design rationale are
in [Design](design.md). The public JSONL method set and schemas are in
[RPC](rpc.md).
