# dlgt design

This document is the normative design for the local `dlgt` runtime. It defines
the Session lifecycle, the provider signals that establish state, and the
semantics of operations and mid-turn intervention. Command syntax belongs in
[CLI](cli.md); public JSONL schemas belong in [RPC](rpc.md).

Backward compatibility with earlier drafts is not required.

## Product boundary

`dlgt` is a local, single-binary runtime for live, addressable, and
attachable Codex and Claude subagents. `Session` is its only public runtime
object.

Each Session owns one provider conversation, one harness process set and its
provider-specific control path, one PTY and rendered screen, and one serialized
controller. The owning version's daemon keeps Session state, executions,
lifecycle events, results, bounded raw PTY bytes, and rendered scrollback in
memory. When that daemon exits, its dlgt Session state disappears; the returned
provider-qualified Session ID remains the durable lookup and resume key in
Codex or Claude.

Provider turns, steering messages, and delivery attempts are
internal correlation records. They do not have public selectors and do not
change the Session-only public object model.

The product boundary excludes panes, layouts, a terminal picker, remote
transport, an orchestration DSL, and public Turn, Queue, Operation, or Delivery
resources.

## Core invariants

1. `new` always creates a new Session; `send` never creates one.
2. A Session has at most one active execution.
3. Every accepted user turn receives a monotonically increasing
   `execution_seq`. Steering does not create an execution sequence.
4. State-changing commands, provider callbacks, and attach input are
   serialized per Session.
5. There is no server-side queue. The default send mode rejects busy work
   without side effects; steering and replacement are explicit modes with
   distinct semantics.
6. Provider lifecycle interfaces determine readiness, blocked state,
   quiescence, and completion. PTY silence and screen content never do.
7. Stale, duplicate, mismatched, or late provider events are diagnostic only;
   they cannot mutate a newer execution or restart generation.
8. Acceptance is separate from observation. `new` and `send` return as soon
   as a prompt is accepted; every later observation is one bounded `fetch`.
   Startup, cancellation, and restart waits are bounded, and a caller deadline
   reports what was observed instead of cancelling underlying work.
9. Replacement means cancel, prove provider quiescence, then start a new turn
   in the same provider conversation. It is not process restart.
10. Attach is an exclusive input lease. Semantic operations cannot interleave
    with an attached controller.
11. Control-plane commands return JSON by default. Raw terminal streams are an
    explicit exception.

## Session lifecycle

The public Session states are:

```text
starting  idle  busy  blocked  canceling  restarting  stopping  stopped  failed
```

`canceling` includes the internal quiescing interval after an interrupt was
requested but before the provider proved it can accept another turn.

```text
starting --provider ready------------------------------> idle
starting --startup failure-----------------------------> failed

idle --turn accepted-----------------------------------> busy
busy --human input required----------------------------> blocked
blocked --attach answer accepted-----------------------> busy
busy|blocked --cancel or replace-----------------------> canceling
canceling --matching provider quiescence---------------> idle

busy --terminal result---------------------------------> idle

starting|idle|busy|blocked|canceling --restart---------> restarting
restarting --replacement provider ready---------------> idle or busy
restarting --replacement failure----------------------> failed

starting|idle|busy|blocked|canceling|restarting --stop-> stopping
stopping --intentional provider exit------------------> stopped
unexpected provider or control-transport loss---------> failed
```

The daemon may pass directly from one execution to the next without publishing
an observable idle interval. It must nevertheless terminalize the old
execution before assigning a new `execution_seq`.

An accepted turn reserves its internal execution before provider delivery. If
delivery fails, dlgt records a retained failed result and clears the reservation;
it does not leave phantom busy work. Provider acceptance then binds the
provider turn ID to that reservation.

`blocked` retains the active execution. It means the provider needs a human
answer, not that the execution completed or that another semantic prompt may
start.

Provider death during an active execution creates a retained failure or
interruption before the Session becomes terminal. Explicit stop and restart
own their process-exit transitions and are not misclassified as crashes.

## State authority

Lifecycle state has three sources, in descending authority:

1. structured provider events from Codex app-server or Claude hooks;
2. serialized dlgt control operations and provider-process exit;
3. PTY input/output, used only for delivery, attach, rendering, and diagnosis.

The rendered screen may help a human understand a failure, but text such as a
prompt marker, spinner, or completion-looking sentence is never a state
transition. Missing provider events fail closed: a live process remains busy,
blocked, or canceling until authoritative evidence arrives; an unexpectedly
dead process fails.

## Codex lifecycle observation

Codex uses a private app-server connection for semantic control and lifecycle
notifications. Its remote TUI PTY exists for attach and presentation.

| App-server signal | dlgt interpretation |
| --- | --- |
| `thread/started` | Bind and validate the Codex thread, then publish `codex:<thread-id>` as the Session ID; establish startup/resume readiness for the expected generation. |
| `turn/started` | Match the bound thread and pending execution, bind `provider_turn_id`, and confirm `busy`. |
| `turn/completed`, `completed` | Store the final assistant text, terminalize the execution successfully, and become `idle`. |
| `turn/completed`, `failed` | Store the sanitized error, terminalize as failed, and become `idle`. |
| `turn/completed`, `interrupted` | Prove interruption and provider quiescence. Preserve canceled/replaced intent when dlgt initiated it. |
| retryable `error` | Record `provider.retrying`; remain `busy`. |
| terminal `error` | Fail only the matching active execution. |
| provider server request | Preserve the active execution and enter `blocked`. |
| unexpected app-server/transport close | Terminalize active work and fail the Session. |

Thread ID, turn ID, active `execution_seq`, and restart generation are checked
before mutation. A wrong thread, wrong turn, duplicate terminal notification,
or callback for an older generation is retained as an unmatched diagnostic
event and otherwise ignored.

The result returned by `turn/start` is dispatch acknowledgement. The matching
notifications remain authoritative for continuing and terminal state.

## Claude lifecycle observation

Claude uses semantic PTY input for ordinary turns and Session-scoped lifecycle
hooks for state. dlgt injects hooks into the child launch settings; it does not
modify the user's global hook configuration.

| Claude hook | dlgt interpretation |
| --- | --- |
| `SessionStart` | Bind and validate Claude `session_id`, then publish `claude:<session-id>` as the Session ID; establish startup/resume readiness. |
| `UserPromptSubmit` | Match the pending prompt, bind provider turn data when present, and confirm `busy`. |
| `Notification(permission_prompt)` | Preserve the active execution and enter `blocked`. |
| `Notification(elicitation_dialog)` | Preserve the active execution and enter `blocked`. |
| `Notification(idle_prompt)` | Evidence that Claude is waiting for input; after an interrupt it may be the only quiescence signal. |
| `Stop` | Store `last_assistant_message`, terminalize the matching execution, and prove quiescence after cancel. |
| `StopFailure` | Store sanitized failure data, terminalize the matching execution, and prove quiescence after cancel. |
| `SessionEnd` | Finish explicit stop/restart handoff, or interrupt unfinished work and stop/fail the Session. |

For dlgt-originated input, the daemon creates the execution before writing the
prompt and marks the Session busy after a successful PTY write.
`UserPromptSubmit` then confirms provider acceptance and must match the pending
prompt when the hook supplies it. A human prompt entered through attach may
create and bind the execution directly from `UserPromptSubmit`.

Hook payloads must match the expected agent, provider session, active prompt or
provider turn when supplied, and restart generation. Unmatched prompt, Stop,
StopFailure, or SessionEnd callbacks cannot complete newer work.

When Claude reports a permission or elicitation dialog, attach becomes the
human response path. The first accepted attach input moves `blocked` back to
`busy` without creating a new execution. A quiet screen never resolves
`blocked`.

## Operations

### Provider mapping

| Operation | Codex | Claude |
| --- | --- | --- |
| Start or resume Session | app-server thread start/resume plus remote TUI readiness | CLI process plus `SessionStart` hook |
| Start ordinary turn | `turn/start` | bracketed-paste prompt, settle, then Enter |
| Steer active turn | `turn/steer` | Session-scoped hook bridge at model-request boundaries |
| Cancel active turn | `turn/interrupt` | state-aware TUI interrupt, then wait for interrupt quiescence evidence (see Cancel) |
| Answer blocked input | provider response path through exclusive attach | exclusive attach input |
| Restart process | stop old control/process generation, resume provider conversation, wait for readiness | stop old process generation, `--resume` provider session, wait for `SessionStart` |
| Stop Session | close control and terminate owned process group | terminate owned process group |

Codex semantic operations never fall back to typing into its TUI. Claude
semantic PTY writes use bracketed paste so prompt bytes are treated as one input
and reject embedded terminal escape bytes. Enter is written separately after a
short settle interval because large pastes may install asynchronously.

Before an ordinary Claude turn, dlgt holds the Session controller, requires
authoritative `idle`, sends one input-clear control, and rechecks that no
lifecycle transition occurred before pasting. This removes drafts left by a
detached human. It must not use double `Esc`, which opens rewind when the editor
is already empty, or `Ctrl+S`, which may restore a prior stash.

### Send modes

The semantic send operation has an internally first-class mode:

```text
reject  steer  replace
```

CLI flags and RPC fields may expose these names differently, but must preserve
the following behavior.

| Mode | Idle | Busy | Blocked/canceling | Durable pending record |
| --- | --- | --- | --- | --- |
| `reject` | Start a new execution. | `SESSION_BUSY`, no side effects. | Reject with the state-specific error. | No |
| `steer` | `NO_ACTIVE_EXECUTION`. | Target the current execution. | Reject; steering cannot answer a human dialog or a turn already canceling. | Yes, until injected or target execution terminalizes |
| `replace` | Start a new execution. | Atomically reserve replacement, cancel, await quiescence, then start it. | Replace blocked work; reject an already canceling/replacing Session. | Yes, until started or failed |

All modes are decided under the per-Session controller lock. Concurrent
operations cannot leapfrog an accepted replacement. A busy Session rejects a
new prompt; dlgt never holds one for later dispatch.

Pending replacement records live in the owning daemon. If that daemon exits, it
stops its provider processes and discards the Session, execution, event, and
terminal-history state. The provider conversation ID returned to the caller
remains available for provider-native lookup or resume.

### Steering

Steering belongs to the active execution and never becomes a later user turn.
It preserves `execution_seq` and ordering among steering messages accepted for
that execution.

Codex sends steering through app-server `turn/steer` with the bound thread and
turn. A successful response means the provider accepted the input for the
active turn; it does not mean the model followed it.

Claude has no equivalent semantic control API. dlgt therefore keeps a
Session-scoped steering inbox and installs a non-blocking hook bridge:

1. `PostToolBatch` checks the inbox once after a complete parallel tool batch
   and before the next model request. It is preferred over `PostToolUse`, whose
   hooks may run concurrently and race while draining the same inbox.
2. Pending text is returned as hook `additionalContext` for the next model
   request.
3. `Stop` is the final normal boundary. If steering remains pending for the
   active execution, the hook blocks that Stop and returns the text as the
   continuation reason.
4. Hook handlers never long-poll. An empty inbox returns immediately.

Claude steering is best effort and boundary-delayed. `additionalContext` is a
system reminder, not a user message. Long thinking or text generation without
a tool boundary cannot be interrupted by it, and a user interrupt does not fire
`Stop`. A steering record is therefore scoped to its target `execution_seq` and
expires when that execution terminalizes; it is never silently promoted into
the next turn.

Observability distinguishes:

```text
accepted  retained by dlgt for the target execution while its daemon lives
injected  handed to app-server or emitted by the Claude hook bridge
expired   target execution ended before injection
```

`injected` does not mean `acted_on`; provider compliance is not observable.

### Replace

Replace is one atomic intent even though provider cancellation and the new
turn are asynchronous:

```text
reserve replacement
cancel active execution
wait for matching provider quiescence
terminalize the replaced execution
start replacement in the same provider conversation
```

A cancellation timeout returns `CANCEL_TIMEOUT` while the replacement remains
reserved and the Session remains `canceling`; it does not start the replacement
early. Process kill/restart is never an implicit consequence of replace. If the
caller wants process recovery, it invokes restart explicitly.

### Cancel, restart, attach, and stop

Cancel affects only the active execution. Idle cancellation is idempotent.
Codex quiescence comes from matching interrupted completion.

Claude interrupt quiescence is weaker. A dlgt-initiated TUI interrupt is a
user interrupt from Claude's perspective, and Claude Code documents that manual
interruption fires neither `Stop` nor `StopFailure`; `StopFailure` covers API
errors only. Interrupt quiescence therefore comes from a matching
`Notification(idle_prompt)`, from a `Stop`/`StopFailure` terminalizing a turn
that finished on its own before the interrupt landed, or from owned process
exit. Whether `idle_prompt` fires after an interrupt is not documented, so a
Claude cancel may legitimately exhaust its bounded wait even though the turn
was interrupted. A timeout still does not infer quiescence; the caller
escalates with explicit restart.

For a running Claude turn, the adapter uses the documented interrupt key. When
Claude is `blocked`, `Esc` may first dismiss the permission or elicitation
dialog rather than interrupt the turn, so the adapter treats dialog dismissal
and turn interruption as separate steps and continues waiting for lifecycle
proof. It never declares cancellation from the key write itself. If semantic
interruption cannot reach quiescence, cancel/replace times out and the caller
may request an explicit process restart.

Restart replaces the provider process/control generation while retaining the
alias, provider conversation, execution sequence, and retained history. A
replacement PTY always starts a new screen epoch. The
provider-qualified Session ID remains the same unless Claude rotates its
provider session ID, in which case dlgt atomically rekeys the retained Session.
Active work receives a retained interrupted result before the replacement
generation can dispatch new work.

Attach grants one exclusive writer lease. While attached, semantic send modes
are rejected with `SESSION_ATTACHED`; the attached human owns provider input.
Detach does not prove that the provider input editor is empty, so Claude input
must be normalized before a later semantic turn is submitted.

Stop rejects new work, interrupts active work durably, terminates the owned
process group, and releases the active alias after the Session reaches
`stopped`. History and pending-work diagnostics remain addressable by immutable
Session ID.

## Rendered scrollback and raw PTY

dlgt retains two bounded representations:

```text
PTY bytes
  |-- raw ring ------------------------------> attach / logs --raw
  `-- live VT emulator -> stable rows -------> fetch / scrollback
```

Terminal applications move cursors, erase lines, overwrite spinners, and
repaint. A VT emulator must interpret those operations; stripping ANSI bytes
would expose duplicate and false text. Raw bytes are an explicit diagnostic
capability and may contain secrets. Normal observation uses the rendered
screen, lifecycle events, and retained results.

One VT emulator per Session process generation is fed as PTY bytes arrive.
Rows that scroll out of the live grid are appended to a per-Session vector with
monotonically increasing absolute row IDs; the live grid is rendered on demand
and is a replaceable snapshot, never history. The raw ring cannot back a
forward cursor because eviction can cut mid escape sequence and change the
interpretation and indexing of every remaining byte.

A screen epoch identifies one continuous rendering. It advances on process
restart, `RIS`, erase-scrollback (`CSI 3 J`), and a resize that changes the
column count. Ordinary clears and alt-screen transitions mutate only the live
grid. Stable rows are immutable text and are never rewritten by a later epoch.

## Observation model

Acceptance and observation are separate operations:

```text
new / send   accept a prompt, return an acceptance cursor, exit
fetch        one composite forward-delta read from a cursor
```

The acceptance cursor is captured under the store lock immediately before the
acceptance is recorded, so a provider that emits output or finishes instantly
cannot move in front of the position the caller was handed.

`fetch` joins the lifecycle event log to retained results, forward stable rows,
and the live screen. A cursor is opaque, carries the codec version, the daemon
boot identity, one Session UID or the daemon-wide scope, and per-Session
watermarks. It binds to the immutable Session UID rather than the public
Session ID, so a Claude rekey does not invalidate it, and pre-rekey IDs stay
resolvable for the daemon lifetime.

Retention is bounded and eviction is observable: 10,000 stable rows per
Session, 50,000 lifecycle events per daemon, and 128 results or 16 MiB of
result bodies per Session. A cursor that predates an eviction returns a
structured gap with a bounded baseline and a fresh cursor; a silent reset is
never acceptable. A cursor from another daemon boot, a newer codec, or a
different scope is a structured error whose recovery is one cursorless
baseline fetch.

Every observation is a success. Timeout, blocked input, a failed execution, and
a canceled result all exit zero and are described by the response `reason` and
result status. Non-zero exits remain for malformed requests, unknown Sessions,
unusable cursors, and transport failure.

A response is assembled one unit at a time. Each watermark is recorded only
when the unit it describes is committed and survives the final measurement,
and the cursor is encoded last from the committed set, so a squeezed response
cannot leave a watermark past data the caller never received.

### Known limitations

- Rebasing the VT emulator serializes only the visible grid. Scroll margins
  (`DECSTBM`), origin mode, the saved cursor, and pending wrap are lost across
  a main-screen rebase because the emulator exposes none of them. Already
  promoted rows are unaffected, and full-screen applications re-issue these
  modes on their next repaint.
- The Claude transcript fallback canonicalizes a path, then opens it with
  `O_NOFOLLOW` and verifies the descriptor by device and inode. Swapping an
  *intermediate* directory between those two steps is not detected; closing it
  needs an `openat2(RESOLVE_BENEATH)`-style walk. The transcript is written by
  a same-user provider that can already modify the target directly, so this
  race grants no privilege it does not already have.

## Launch environment and security

Profiles are expanded by the client before RPC:

```text
client snapshot or clean base < Profile < explicit launch options
```

The invoking client's environment, not the daemon startup environment, is the
default launch base. Launch environment values travel in RPC memory and are not
serialized directly into Session metadata, events, Profiles, or errors.
Provider output can still echo them, so results and terminal output remain
sensitive.

dlgt applies child-scoped lifecycle overrides without mutating global provider
configuration: Codex receives `check_for_update_on_startup=false`; Claude
receives `DISABLE_AUTOUPDATER=1`. It also marks the Session working directory as
trusted in each provider's workspace state.

Workers are auto-approved by default using the provider's supported bypass
mode. An explicit Session/Profile opt-out keeps provider approval prompts, which
then appear as `blocked` input. Lifecycle hooks and steering hooks are injected
only into the owned child launch and coexist with user hooks.

The local RPC socket is mode 0600. `rpc --stdio` exposes only the public method
allowlist. Raw PTY bytes require explicit `logs --raw` access.

## Events and results

Events normalize lifecycle, actionable blocked state, and steering delivery;
they do not pretend both providers offer equivalent token or message streams.
Humans inspect live output with attach; agents use one `fetch`, which returns
the retained terminal result together with the forward screen delta.

At minimum, the event model distinguishes:

```text
session.ready        session.busy        session.blocked
session.canceling    session.idle        session.restarting
session.stopping     session.stopped     session.failed
input.steer.accepted input.steer.injected input.steer.expired
turn.completed       turn.failed         turn.interrupted
provider.retrying    provider.unmatched
```

Every public event is versioned and ordered by a daemon sequence number.

## Acceptance criteria

1. Two concurrent default sends to one idle Session yield one accepted
   execution and one side-effect-free busy rejection.
2. Two concurrent state-changing operations are serialized, and accepted
   replacement order is stable for the daemon lifetime.
3. `new` plus an initial prompt succeeds atomically or reports one failure
   without a live half-created alias.
4. Every startup reaches ready or terminal failure within its deadline.
5. Every accepted execution receives exactly one `execution_seq`, one
   acceptance cursor, and one retained terminal result.
6. Provider death during execution produces a retained result and terminal
   Session state in bounded time.
7. Blocked input remains observable and is never inferred as completion.
8. Cancel and replace never start new work before matching provider
   quiescence.
9. Steering never leaks into a later execution, and a retried acceptance
   carrying the same request ID replays its original receipt instead of
   creating a second Session or execution.
10. Claude concurrent tool hooks cannot double-drain steering; Stop provides
    the final normal injection boundary.
11. Daemon exit stops owned provider processes and discards all runtime-local
    Session state; no work is recovered or dispatched after restart.
12. Attach excludes semantic sends and a second writer unless ownership is
    explicitly transferred.
13. PTY silence, rendered text, and timeout are never accepted as lifecycle
    proof, and a live-screen repaint alone never completes a long poll.
14. All control success and error paths emit schema-valid JSON, and every
    public event carries `schema_version`.
15. Raw PTY bytes are unavailable without explicit diagnostic access.

## Design decisions

### Session remains the public address

Callers operate on one live Session identity. Internal execution and
delivery records exist for atomicity and observation, not as a second public
API.

### Steering is not a queue

Steering changes the current execution and expires with it. dlgt does not hold
a prompt for later dispatch: a busy Session rejects new work without side
effects, so ordering, cancellation, and result ownership stay unambiguous.

### Replacement is not restart

Replacement changes active work inside the same provider conversation. Restart
replaces the provider process generation. Keeping them separate avoids using a
process kill as routine flow control and preserves provider context and
diagnostics.

### Provider asymmetry is explicit

Codex has semantic app-server methods for turn start, steer, and interrupt.
Claude provides lifecycle hooks and an interactive input surface. dlgt exposes
one intent model but documents weaker, boundary-delayed Claude steering and
weaker Claude interrupt quiescence instead of claiming identical guarantees.

### Acceptance is never fused with a long wait

A command that accepts work and then blocks for minutes loses its acceptance
receipt whenever the caller's harness kills, backgrounds, or swallows the
response, which produces duplicate Sessions and Session-ID rediscovery. `new`
and `send` therefore return immediately with a cursor, and waiting is a
separate, replayable `fetch`.

### Timeouts express bounded observation

Startup, cancellation, and restart waits are bounded, and a `fetch` deadline is
an observation bound. Timeout never manufactures completion, quiescence, or
cancellation, and it is reported as a successful observation rather than an
error.

### Notifications are clients

Arbitrary completion commands in the daemon create reentrancy, secret, and
failure-policy problems. Consumers follow normalized events instead.

### Identifiers have distinct roles

Aliases are readable active-session conveniences. Provider-qualified Session
IDs remain unambiguous after alias reuse. `execution_seq` correlates accepted
work and results without creating a public Turn identity.

### Idle cancel is idempotent

Canceling an idle Session succeeds with `canceled:false`. Fetching a Session
that has never accepted work returns a baseline snapshot with no result rather
than an error.

### Model discovery reflects provider capability

Codex discovery uses app-server `model/list`. Claude keeps the stable Claude
Code aliases and augments them from the public, daily refreshed
`claude-models-list` snapshot. Date-pinned `-YYYYMMDD` IDs are normalized to
their undated alias, which Claude Code accepts and which keeps the default
result a concise list of aliases and current canonical IDs. Exact canonical IDs
also use the snapshot's effort capabilities for pre-launch model/effort
validation, and a date-pinned ID validates against its alias. Floating aliases
remain provider-validated.

## Provider references

- [Codex app-server](https://developers.openai.com/codex/app-server/)
- [Claude Code hooks](https://code.claude.com/docs/en/hooks)
- [Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode)

## Superseded concepts

The final design removes these earlier assumptions:

- a daemon-owned prompt queue can coexist with a one-execution Session;
- one overloaded queue operation can mean both steering and a future turn;
- replacement should kill and restart the provider process;
- PTY quietness or prompt text can prove readiness or completion;
- public Turn, Queue, Operation, or Delivery resources are required;
- provider implementations must pretend to offer identical steering strength;
- optional or unbounded lifecycle waits are acceptable;
- acceptance and a long wait can share one command;
- a bounded observation timeout is an error rather than an observation.
