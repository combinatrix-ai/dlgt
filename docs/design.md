# dlgt v1 design

This document records the product boundary, invariants, provider lifecycle
mapping, security model, acceptance criteria, and decisions behind the public
contract. Command syntax belongs in [CLI](cli.md); public JSONL schemas belong
in [RPC](rpc.md).

## Product boundary

`dlgt` is a local, single-binary runtime for persistent, addressable, and
attachable Codex and Claude subagents. Its only public runtime object is the
Session.

The v1 boundary is deliberately narrow:

- one Rust binary containing the daemon and embedded skill;
- one harness process, PTY, and screen per Session;
- official provider lifecycle surfaces as the authority for readiness and
  completion;
- SQLite-backed state, events, results, input audit, bounded raw PTY retention,
  and rendered scrollback;
- no panes, splits, layouts, terminal picker, remote transport, arbitrary raw
  provider arguments, orchestration DSL, plugin system, or server-side queue.

Provider turns and execution receipts may exist internally for correlation and
persistence, but they are not public resources and do not have public IDs.

## Core invariants

1. `new` always creates a new Session.
2. `send` only addresses an existing Session.
3. A Session has at most one active execution.
4. Sending to a busy Session fails immediately with `SESSION_BUSY`.
5. dlgt never queues a prompt for later execution.
6. A Session has one controller. Concurrent sends are serialized so exactly one
   may be accepted; cross-controller `cancel`, `stop`, and interactive input are
   unsupported interference.
7. Startup and wait operations are always bounded.
8. A wait timeout never cancels the underlying execution.
9. Provider lifecycle events, not PTY text, determine readiness and completion.
10. Control-plane commands return JSON by default without a `--json` flag.

## Provider lifecycle

Codex uses its private app-server connection:

```text
thread/started                    -> Session provider binding
turn/started                      -> session.busy
error, willRetry=true             -> provider.retrying
turn/completed status=completed   -> durable result + session.idle
turn/completed status=failed      -> durable failure + session.idle
turn/completed status=interrupted -> durable interruption + session.idle
```

Claude uses Session-scoped lifecycle hooks:

```text
SessionStart      -> session.ready
UserPromptSubmit  -> session.busy
Stop              -> durable result + session.idle
StopFailure       -> durable failure + session.idle
SessionEnd        -> session.stopped
```

Provider IDs and internal execution records are retained privately to reject
stale, duplicated, or unmatched lifecycle messages. They never become public
identity. Loss of an authoritative provider transport fails closed: PTY
silence, screen content, and a quiet window never prove completion.

## Rendered scrollback and raw PTY

dlgt retains two bounded representations:

```text
PTY bytes
  |-- raw ring -----------------> attach / logs --raw
  `-- VT parser -> scrollback --> scrollback --lines N
```

Raw bytes are an explicit diagnostic capability because they are noisy, may
contain terminal control sequences, and may contain provider-emitted secrets.
Normal agent observation uses a headless VT emulator and rendered plain-text
scrollback.

Evidence from a 92-second Claude review Session shows why stripping ANSI bytes
is insufficient:

```text
raw PTY bytes          59,841
ANSI sequence bytes    23,182 (38.7%)
ESC sequences           4,610
CSI sequences           4,509
carriage returns         4,130
non-empty stripped rows    768
unique stripped rows       336
```

Terminal applications move the cursor, erase lines, overwrite spinners, and
repaint the screen. A VT emulator must interpret those operations; byte
stripping would expose duplicate or false text.

## Launch environment and security

Profiles are expanded by the client before RPC. Launch environment precedence
is:

```text
client snapshot or clean base < Profile < explicit launch options
```

The default environment is a snapshot of the invoking client, never the
daemon's startup environment. `--clean-env` starts from a minimal runtime base;
`--pass-env`, `--env`, and `--unset-env` modify the environment used by `new`
or `restart`. Values are freshly supplied for every process launch and are not
persisted for later replay.

After that expansion, dlgt applies non-configurable lifecycle safety overrides
to its child Harness processes. Codex commands receive
`check_for_update_on_startup=false`; Claude receives
`DISABLE_AUTOUPDATER=1`. These overrides prevent updater UI from blocking the
bounded readiness transition, take precedence over launch environment values,
and do not mutate either provider's global configuration.

Environment values travel in RPC memory rather than argv and are not directly
serialized into Session records, `list`, `show`, events, Profiles, or error
JSON. This is a metadata boundary, not an output redaction guarantee. Provider
output is untrusted and can deliberately echo environment values, so durable
results, scrollback, and especially raw logs remain potentially sensitive.

The local RPC socket is mode 0600. `rpc --stdio` exposes only the documented
public method allowlist; private provider and execution methods are not an
escape hatch. Raw PTY bytes require an explicit request.

## Acceptance criteria

1. Two concurrent `send` calls to one idle Session produce exactly one accepted
   execution and one side-effect-free `SESSION_BUSY` response.
2. No accepted prompt remains queued behind another prompt.
3. `new` with an initial prompt creates the Session and accepts the prompt
   atomically, or reports one failure without a live half-created alias.
4. Every startup attempt reaches ready or a terminal failure within its startup
   deadline.
5. `wait` requires a positive timeout, and timeout leaves the Session active.
6. Every accepted execution receives a monotonic `execution_seq`; every
   terminal result echoes it and contains `status` and `final_text`.
7. Provider death during execution produces a durable failed result in bounded
   time.
8. Blocked input becomes observable without being misclassified as completion.
9. `cancel` reaches provider quiescence or `CANCEL_TIMEOUT` within its deadline.
10. A stopped Session releases its exact alias while its history remains
    readable by Session ID.
11. All control-plane success and error paths emit schema-valid JSON.
12. Codex model discovery matches app-server `model/list`; Claude discovery is
    explicitly marked partial.
13. Rendered scrollback represents the VT screen and history without ANSI
    control sequences or repeated spinner redraws.
14. Raw PTY bytes are unavailable without explicit `logs --raw` access.
15. Launch environment values never appear through direct metadata
    serialization; tests do not claim that provider output cannot echo them.
16. A second attach is rejected unless `--steal` transfers the exclusive lease.
17. Every public event matches the versioned v1 event enum and carries
    `schema_version`.
18. Restart preserves Session identity, alias ownership, provider context, and
    execution sequence. Active work becomes a durable interrupted result before
    the replacement process starts.

## Design decisions

### Session creation belongs to `new`

`send` never creates a missing Session. Common aliases can collide when several
controllers share a daemon, and implicit creation would blur ownership. `new`
generates a collision-resistant alias and returns the immutable Session ID.

### Alias and Session ID serve different roles

Aliases are readable terminal conveniences. Short Session IDs are immutable
automation addresses and remain unambiguous after alias reuse. The eight
unambiguous Crockford Base32 characters are protected by a local database
uniqueness constraint and collision retry; a UUID would add little value for a
single local runtime.

An alias is reserved while its Session is starting, idle, busy, blocked,
canceling, or stopping. Stopped and failed Sessions release it;
history stays addressable by Session ID.

Restart atomically reclaims that alias. If a newer active Session already owns
it, restart fails with `ALIAS_IN_USE` instead of silently renaming either
Session.

### JSON is the default control-plane format

dlgt is primarily an agent and automation control plane. One stable JSON shape
avoids per-command `--json` flags, ambiguous tables, and fragile parsing. Raw
terminal, text help, and streaming commands are explicit exceptions.

### Execution sequence is correlation, not identity

The supported ownership contract has one controller and at most one active
execution per Session. `wait`, `cancel`, and `show` can therefore address the
Session. A monotonic `execution_seq` correlates acceptance with a durable result
without creating a public Turn or Operation resource.

### There is no server-side queue

Hidden queueing makes prompts stale, complicates dependencies and cancellation,
creates attach contention, and makes recovery harder. Sequential work is
expressed by the controller:

```bash
dlgt send ses_7K3M9Q2X --wait --timeout 15m -- "First task"
dlgt send ses_7K3M9Q2X --wait --timeout 15m -- "Second task"
```

The agent retains conversational context; dlgt need not retain pending work.

### Timeouts express bounded ownership

Startup always has a finite 60-second default. Wait duration expresses caller
intent and is mandatory for `wait` and `--wait`. Cancellation is bounded by a
30-second default with an optional override. A timeout observes state; it does
not cancel the underlying execution.

### Notifications are clients, not daemon hooks

Arbitrary daemon-executed completion commands introduce secret handling,
reentrancy, and failure-policy problems. `events --follow` is the normalized
extension point for desktop notifications, webhooks, and orchestrator wakeups.

### Events contain lifecycle, not generated text

Codex provides structured message deltas, while an attachable Claude Session
exposes generated text through its PTY without an equivalent hook-backed
stream. The public event API normalizes only lifecycle and actionable state
that both Harnesses support. Humans inspect incremental output with `attach`;
agents use bounded `scrollback`; `wait` returns the final result.

### Attach is exclusive

v1 uses one attach writer. A second attach is rejected unless `--steal`
explicitly transfers the lease, preventing interleaved terminal input and
matching the single-controller model.

### Titles are mandatory

The title is the human description and source of the generated alias. Requiring
it adds intentional friction at creation; automation uses the returned Session
ID afterwards.

### Idle cancellation is idempotent

Canceling an idle Session succeeds with `canceled:false`, which keeps cleanup
scripts simple. `NO_RESULT` remains a real error when waiting on a Session that
has never accepted work.

### Model discovery reflects provider capabilities

Codex discovery uses app-server `model/list`. Claude Code has no equivalent
documented machine-readable picker, so dlgt returns stable aliases and marks
discovery partial. Model selection remains optional; provider defaults and
Profiles are the normal path.

## Superseded concepts

v1 intentionally removes these earlier draft concepts:

- `start` as a separate Session-creation command;
- `send` creating or reusing Sessions;
- public Turn and Operation resources or IDs;
- FIFO queues and queue positions;
- `--enqueue`, `--after-success`, and `--fail-if-busy`;
- optional or implicit wait deadlines;
- default human-formatted control-plane output;
- `logs --follow` as an agent observation surface;
- a top-level `input-log` command;
- permanent alias reservation after a Session stops.

Backward compatibility with the superseded draft is not required.
