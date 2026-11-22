# dlgt design

This document is the normative design for the local `dlgt` runtime. It defines
the Session lifecycle, the provider signals that establish state, and the
semantics of operations, steering, and Requests. Command syntax belongs in
[CLI](cli.md); public JSONL schemas belong in [RPC](rpc.md).

Backward compatibility with earlier drafts is not required.

## Product boundary

`dlgt` is a local, single-binary runtime for live, addressable Codex and Claude
subagents. `Session` is its only public runtime object.

Each Session owns one provider conversation, one provider process driven over
that provider's structured channel, and its serialized lifecycle state:

```text
Codex    codex app-server, JSON-RPC over the child's stdio
Claude   one long-lived claude -p process, stream-json on stdin and stdout
```

dlgt owns no terminal. There is no PTY, no rendered screen, no attach, and no
hook bridge. What the provider reported is retained as bounded
[messages](#messages-replace-the-screen); input that only the caller can give
is retained as [Requests](#requests-and-approvals).

The owning version's daemon keeps Session state, executions, lifecycle events,
results, messages, and Requests in memory. When that daemon exits, its dlgt
Session state disappears; the returned provider-qualified Session ID remains
the durable lookup and resume key in the provider.

Provider turns, steering messages, and provider request IDs are internal
correlation records. They do not have public selectors and do not change the
Session-only public object model.

The product boundary excludes panes, layouts, a terminal picker, remote
transport, an orchestration DSL, and public Turn, Queue, Operation, or Delivery
resources.

## Core invariants

1. `new` always creates a new Session; `send` never creates one.
2. A Session has at most one active execution.
3. Every accepted prompt receives a monotonically increasing `execution_seq`.
   Steering does not create an execution sequence.
4. State-changing commands and provider events are serialized per Session.
5. There is no server-side queue. The default send mode rejects busy work
   without side effects; steering is an explicit mode that only adds to the
   running execution.
6. Structured provider events determine readiness, blocked state, quiescence,
   and completion. Provider silence and elapsed time never do.
7. Stale, duplicate, mismatched, or late provider events are diagnostic only;
   they cannot mutate a newer execution or restart generation.
8. Acceptance is separate from observation. `new` and `send` return as soon
   as a prompt is accepted; every later observation is one bounded `fetch`.
   Startup, cancellation, and restart waits are bounded, and a caller deadline
   reports what was observed instead of canceling underlying work.
9. Replacing running work is `cancel` followed by a plain `send`. It is not
   process restart.
10. dlgt never approves on its own. Approvals come from the provider's
    reviewer, the caller, or the selected `bypass` policy.
11. Control-plane commands return JSON by default. `watch` is the explicit
    human text exception.

## Session lifecycle

The public Session states are:

```text
starting  idle  busy  blocked  canceling  restarting  stopping  stopped  failed
```

`canceling` covers the interval after an interrupt was requested and before
the provider reported the interrupted turn's result and every cancel control
for that execution was dispatched and settled.

```text
starting --provider ready------------------------------> idle
starting --startup failure-----------------------------> failed

idle --prompt accepted---------------------------------> busy
busy --Request opened----------------------------------> blocked
blocked --last pending Request closed------------------> busy
busy|blocked --cancel----------------------------------> canceling
canceling --interrupted result and controls settled----> idle

busy --execution settled-------------------------------> idle

idle|busy|blocked|canceling|stopped|failed --restart---> restarting
restarting --replacement provider ready---------------> idle
restarting --replacement failure----------------------> failed

starting|idle|busy|blocked|canceling|restarting --stop-> stopping
stopping --intentional provider exit------------------> stopped
unexpected provider exit or transport loss-------------> failed
```

An accepted prompt reserves its internal execution and moves the Session to
`busy` before provider delivery, so a provider event for that execution never
finds the Session idle. If delivery fails, dlgt records a retained failed
result; it does not leave phantom busy work. Provider acceptance then binds
the provider turn to that reservation.

`blocked` retains the active execution. It means at least one Request is
pending, not that the execution completed or that another prompt may start.
A Request is closed by `answer`, by the provider withdrawing it, or by
`cancel`, `stop`, or `restart`, which close every pending Request as
`canceled`.

Provider death during an active execution creates a retained failure or
interruption before the Session becomes terminal, and the provider's stderr
tail is kept as a `notice` message. Explicit stop and restart own their
process-exit transitions and are not misclassified as crashes. Process exit is
observed apart from the provider's output stream: dlgt waits at most two
seconds for that stream to drain so the provider's final events apply first,
then reports the exit, even if a process that left the provider's group still
holds the stream open.

### Lifecycle authority

Every phase change is one call to a pure transition function
(`src/lifecycle.rs`). An event names the provider generation it comes from;
execution events also name their execution sequence. The function refuses an
event from a replaced generation or a finished execution and changes nothing,
so a stop, failure, exit, or late provider report from an old process can
never act on the replacement. It returns the effects the store applies and the
lifecycle events it records; callers never set a phase or record a lifecycle
event themselves. The one exception is publication: when a new Session gets
its public ID, dlgt repeats `session.created` and `session.ready` under that
ID so the timeline a caller can read starts there.

The function guarantees, and a bounded exhaustive test of the function
itself checks over every interleaving of events from the current and a
replaced generation, across several executions:

- a refused event changes nothing;
- each public state change is announced by its event exactly once;
- an execution that leaves the Session other than by the provider's own result
  (restart, stop, exit, failure, including while canceling) gets a terminal
  result;
- an event naming an execution other than the active one is refused as stale;
- leaving an execution closes its Requests, and a published result never
  changes;
- each state accepts exactly the events its transition table lists;
- no state is a dead end: an execution settles through its own provider
  events, and provider events alone reach `idle`, `stopped`, or `failed`.

Generations come from one daemon-wide counter and are never reused, so a
Session recreated by resume does not share a generation with the process it
replaced.

The exhaustive test covers the transition function, not the code around it.
Whether callers pass the generation they captured when their operation
began, whether the store applies the effects, and whether published results
stay unchanged are covered by focused integration regressions instead. Each
operation captures its generation (or execution) at its start and carries
it to every later step: launch, create cleanup, rekey, answer receipts,
cancel controls, and provider callbacks. A published result is frozen: no
later report changes any of its fields.

A stop already in progress may be forced. Restart claims the generation the
caller observed and is refused if another restart replaced it first.

## State authority

Lifecycle state has two sources, in descending authority:

1. structured provider events: Codex app-server responses and notifications,
   and Claude stream-json events and control messages;
2. serialized dlgt control operations and provider-process exit.

Messages are derived from the same events but are never a state transition.
An assistant sentence that looks final does not complete an execution.
Missing provider events fail closed: a live process remains busy, blocked, or
canceling until authoritative evidence arrives; an unexpectedly dead process
or a closed transport fails the Session. Provider writes use dedicated
writer threads or queued controls. RPC handlers release the store and provider
state locks before enqueueing or waiting for writes, so blocked stdin never
stalls stdout draining or unrelated Sessions.

Busy snapshots report `busy_for_ms` and `quiet_for_ms`, the time since the
provider last emitted any event. They are diagnostics. Silence never changes
state.

## Codex lifecycle observation

dlgt runs one `codex app-server` per Session and speaks JSON-RPC over its
stdin and stdout. It sends `initialize` and `initialized`, then starts or
resumes the thread itself.

| App-server message | dlgt interpretation |
| --- | --- |
| `thread/start` or `thread/resume` response | Bind the thread ID, publish `codex:<thread-id>` as the Session ID, record the resolved model, set the thread name with `thread/name/set`, and become `idle`. |
| `thread/started` | Diagnostic only. The thread was already bound from the response. |
| `turn/start` response | Bind the provider turn ID to the reserved execution and confirm the submission. |
| `turn/started` | Match the thread and confirm `busy`. A turn that starts while no execution is active is recorded as a new execution so its result is retained. |
| `turn/completed`, `completed` | Store the final agent message, or an empty string with `final_text_source: "missing"`, terminalize the matching execution as completed, and become `idle`. |
| `turn/completed`, `failed` | Store the sanitized error, terminalize as failed, and become `idle`. |
| `turn/completed`, `interrupted` | Terminalize as interrupted, or settle a dlgt-initiated cancel as `canceled`, and become `idle`. |
| `error` with `willRetry` | Emit `provider.retrying`; remain `busy`. |
| `error` without `willRetry` | Record a `notice` message; the turn's own completion still decides the result. |
| `item/started`, `item/completed` | Append `tool_call`, `tool_result`, and `assistant` messages. |
| `item/autoApprovalReview/completed` | Append an `approval` message decided by `provider_reviewer`, with its risk level. |
| caller server request | Open a Request and enter `blocked`. See [Requests and approvals](#requests-and-approvals). |
| `serverRequest/resolved` | Close the matching pending Request as `canceled`. |
| `warning`, `guardianWarning`, `configWarning` | Append a `notice` message. |
| transport close or process exit | Terminalize active work and fail the Session. |

Thread ID, provider turn ID, and active execution are checked before
mutation. A wrong thread, wrong turn, or duplicate terminal notification is
retained as an unmatched diagnostic event and otherwise ignored. Each provider
turn is terminalized at most once.

A `turn/completed` notification with status `completed` terminalizes the
matching execution even when it carries no agent message. Dlgt records an
empty `final_text` with `final_text_source: "missing"`; later completion
notifications cannot replace that result. After `turn/start`, dlgt also polls
`thread/read` every 250 ms to reconcile terminal turns whose notifications
were missed. It requires two consecutive reads for a non-completed status.

## Claude lifecycle observation

dlgt runs one long-lived Claude Code process per Session:

```text
claude -p --input-format stream-json --output-format stream-json --verbose
  --replay-user-messages --permission-prompt-tool stdio
  --name <display name> [--model M] [--effort E] <approvals flags>
  --session-id <uuid> | --resume <session-id>
```

dlgt chooses the UUID for a new conversation, so the Session ID
`claude:<uuid>` is known before the first turn. Each prompt and each steer is
one `user` line on stdin with a dlgt-chosen `uuid`. Control requests travel
on the same channel in both directions.

| Stream-json event | dlgt interpretation |
| --- | --- |
| `initialize` control response | Startup proof: the control channel works. Record `current_permission_mode` and become `idle`. No answer within the startup timeout is `LAUNCH_FAILED`. |
| `command_lifecycle`, `started` | Claude dequeued a written message and began a provider turn. For the execution's own prompt, confirm the submission. |
| `command_lifecycle`, `cancelled` | The message will never be read. Drop it; a dropped steer becomes a `steer` message with `expired: true`. |
| `user` with `isReplay: true` | The model read the message with that `uuid`. A steer becomes a `steer` message with `delivered: true`. |
| `system`, `init` | Record the resolved model, how Claude authenticated, and the permission mode. A different `session_id` rekeys the Session. |
| `system`, `status` | Record the permission mode Claude now runs. |
| `system`, `permission_denied` | Append a `denied` `approval` message, decided by `provider_reviewer` in `auto` and by `provider_policy` otherwise. |
| `system`, `api_retry` | Emit `provider.retrying`; remain `busy`. |
| `assistant` | Append `assistant` text and `tool_call` messages. Reasoning blocks are dropped. |
| `user` with `tool_result` blocks | Append `tool_result` messages. |
| `result` | End one provider turn: keep its text, error, token usage, and permission denials, then try to settle the execution. |
| `control_request`, `can_use_tool` | Open a Request and enter `blocked`. |
| any other `control_request` | Refuse it with an error response. Left unanswered it would hold the provider forever. |
| `control_cancel_request` | Close the matching pending Request as `canceled`. |
| process exit | Terminalize active work and fail the Session, keeping the stderr tail as a `notice` message. |

### Settling a Claude execution

One dlgt execution can span more than one Claude provider turn, because a
steer written while the model generates text runs as a continuation turn after
the current `result`. dlgt therefore terminalizes a Claude execution only when
both hold:

1. no provider turn is open: the last turn's `result` has arrived; and
2. every message written for the execution, the prompt and each steer, has
   been replayed or confirmed dropped.

`final_text` is the text of each provider turn's `result` in order, joined by
a blank line. If no `result` carried text, dlgt uses the last top-level
assistant message the provider streamed during this execution and reports
`final_text_source: "messages"`. An error `result` fails the execution and the
Session stays usable.

A `result` that arrives before the execution's own prompt has started belongs
to a provider turn dlgt did not start, such as one triggered by a background
task. It is recorded as unmatched and cannot end the execution.

## Operations

### Provider mapping

| Operation | Codex | Claude |
| --- | --- | --- |
| Start Session | spawn app-server, `initialize`, `thread/start`, `thread/name/set` | spawn `claude -p --session-id`, `initialize` control request |
| Resume conversation | `thread/resume` with the stored thread ID | spawn `claude -p --resume`, `initialize` control request |
| Start ordinary turn | `turn/start` | `user` message on stdin |
| Steer active turn | `turn/steer` with `expectedTurnId` | `user` message on stdin, read at the next model request |
| Answer a Request | JSON-RPC response to the server request | `control_response` to `can_use_tool` |
| Cancel active turn | `turn/interrupt` | `cancel_async_message` for each unread message, then `interrupt` |
| Restart process | stop the old app-server, spawn a new one, `thread/resume` | stop the old process, spawn with `--resume` |
| Stop Session | close stdin, terminate the owned process group | close stdin, terminate the owned process group |

### Send modes

The semantic send operation has two modes:

```text
reject  steer
```

| Mode | Idle | Busy | Blocked | Canceling or other |
| --- | --- | --- | --- | --- |
| `reject` | Start a new execution. | `SESSION_BUSY`, no side effects. | `SESSION_BLOCKED` with the pending Request IDs. | `SESSION_BUSY` while canceling; `SESSION_NOT_RUNNING` when starting, stopping, stopped, or failed; `SESSION_UNAVAILABLE` while restarting. |
| `steer` | `NO_ACTIVE_EXECUTION`. | Add the message to the running execution. | `SESSION_BLOCKED`; a steer cannot answer a Request. | `SESSION_NOT_RUNNING` when starting, stopping, stopped, or failed; `SESSION_UNAVAILABLE` otherwise. |

Both modes are decided against current Session state under the store lock.
A busy Session rejects a new prompt; dlgt never holds one for later dispatch.

### Steering

Steering belongs to the active execution and never becomes a later execution.
It preserves `execution_seq`. Acceptance is recorded as a `steer` message with
`delivered: false`. A second `steer` message later reports `delivered: true`,
or `expired: true` when the provider never read it.

Codex sends steering through `turn/steer` with the bound thread and the
expected provider turn ID. The message joins the active turn immediately. A
successful response means the provider accepted the input; it does not mean
the model followed it. A Codex execution whose provider turn ID is not yet
bound returns `SESSION_UNAVAILABLE`.

Claude has no steer method. dlgt writes the steer as another `user` message
while the turn runs. Claude reads it at the next model request, which is after
the tool call in flight returns. If the turn ends first, Claude runs the
message as a continuation turn, and the [settling
rule](#settling-a-claude-execution) keeps that turn in the same execution. A
steer therefore cannot leak into a later execution.

The check that the execution is still running and the reservation of its
steer happen under the execution lock. The transport write is queued after
releasing the store and execution locks; a failed write rolls back the pending
delivery or fails the Session, and cannot block the event reader.

`delivered` does not mean acted on. Provider compliance is not observable.

### Cancel

Cancel affects only the execution it was issued for. Idle cancellation is
idempotent. Cancel records the intent, closes every pending Request as
`canceled`, and asks the provider to interrupt. The Session stays
`canceling` until the matching provider result arrives and all cancel controls
for that execution are dispatched and settled; an old interrupt cannot reach
a later execution.

Codex quiescence comes from the matching `turn/completed` with
`interrupted`.

If a Codex `turn/start` acknowledgement has not arrived, dlgt retains the
cancel intent and waits for that execution's provider turn ID before sending
`turn/interrupt`. The wait is part of the caller's timeout. For Claude, dlgt
first sends `cancel_async_message` for each message the model
has not read. Each one Claude confirms as cancelled is dropped and reported as
an expired steer. dlgt then sends the `interrupt` control request. Quiescence
comes from the interrupted turn's `result`, or, when the prompt was canceled
while still queued, from the settling rule finding nothing left to wait for.
If a leftover message still starts a provider turn after the cancel, dlgt
interrupts that turn too.

Cancellation is bounded from before the first provider control. A timeout
returns `CANCEL_TIMEOUT` and leaves the Session `canceling`; control work
continues for that execution and dlgt never infers quiescence. The caller may
escalate with an explicit `restart` or `stop`.

### Restart and stop

Restart replaces the provider process while retaining the alias, provider
conversation, execution sequence, and retained history. Active work receives a
retained interrupted result and pending Requests are closed before the
replacement process starts. The replacement reports its own effective
approvals mode. Codex keeps the same Session ID. Claude names its session in
`system/init` on the first turn of a process, so a rotated provider session ID
becomes known when the next execution starts; dlgt then rekeys the retained
Session atomically.

Stop rejects new work, closes the provider's stdin, terminates the owned
process group, and releases the alias after the Session reaches `stopped`.
History remains addressable by immutable Session ID.

## Requests and approvals

A Request is input that only the caller can give:

```text
approval   the worker wants to run something that needs permission
question   the worker asked the user something and waits for the reply
```

`--approvals auto|ask|bypass` decides who approves. The exact provider
parameters are in [CLI](cli.md#approvals).

| Mode | Codex reviewer | Claude permission mode | Who decides |
| --- | --- | --- | --- |
| `auto` | `approvalsReviewer=auto_review` | `--permission-mode auto` | The provider's own reviewer; only what it does not settle reaches the caller. |
| `ask` | `approvalsReviewer=user` | `--permission-mode manual` | Every approval request raised by the provider reaches the caller. |
| `bypass` | `approvalPolicy=never`, `sandbox=danger-full-access` | `--dangerously-skip-permissions` | Nobody is asked. |

Provider requests reach dlgt on the structured channel:

| Source | Becomes |
| --- | --- |
| Codex `item/commandExecution/requestApproval` | approval Request |
| Codex `item/fileChange/requestApproval` | approval Request |
| Codex `item/permissions/requestApproval` | approval Request |
| Codex `item/tool/requestUserInput` | question Request |
| Codex `mcpServer/elicitation/request` | question Request |
| supported Codex server request owned by the active execution | matching approval or question Request; enter `blocked` |
| unsupported, stale, idle, or canceling Codex server request | explicit JSON-RPC refusal; never attach it to another execution |
| Claude `can_use_tool` in an active execution | approval Request, or a question Request for `AskUserQuestion` |
| Claude `can_use_tool` with no active or canceling execution | refused, with a `notice` message |

Dlgt bounds every field in a published Request so its minimum form fits under
the 256 KiB response limit. A form's `requestedSchema` is published unchanged;
dlgt truncates display-only text with a marker. If the schema cannot fit, dlgt declines the request to the provider and
records a `notice` instead of publishing a changed form.

Codex `auto_review` decisions arrive as `item/autoApprovalReview/completed`
and are recorded as `approval` messages; they do not become Requests. Claude
does not report what its auto mode approved, only its denials through
`system/permission_denied`.

Answers map back onto the provider's own channel:

| Request | Allow or answer | Deny |
| --- | --- | --- |
| Claude tool | `{"behavior":"allow","updatedInput":...}` | `{"behavior":"deny","message":...}` |
| Claude `AskUserQuestion` | `allow` with the answers in `updatedInput` | not applicable |
| Codex command or file change | `{"decision":"accept"}` | `--message` through `turn/steer` before `{"decision":"decline"}`; the receipt says whether the reason was delivered |
| Codex permissions | the requested permissions | `{"permissions":{}}` |
| Codex user input | `{"answers":...}` keyed by question ID | not applicable |
| Codex elicitation | `{"action":"accept","content":...}` | not applicable |

The Request moves through `pending`, `sending`, `delivered`, and `failed`. It
leaves `blocked` only after provider delivery succeeds or the provider
withdraws it. A duplicate answer waits for the same attempt and replays only a
delivered receipt. Dlgt returns a Request to `pending` only when it knows that
no bytes reached the transport and the same execution still owns the Request.
An ambiguous write stops the provider transport and fails the Session with
`REQUEST_DELIVERY_FAILED`; dlgt does not permit a retry. Provider withdrawal
closes only an undelivered Request. A successful delivery remains replayable
if withdrawal or execution completion arrives before dlgt commits its receipt.
Every reviewer decision dlgt can observe is an `approval` message whose `by`
is `provider_reviewer`,
`caller`, or `provider_policy`. Denials and reviewer timeouts count toward the
result's `denied_requests`, because a provider reports a turn as successful
even when a tool it needed was refused.

### Why dlgt never approves

The caller is the principal that chose the worker and its policy. A daemon
that approved on the caller's behalf would be an unseen third policy: it would
widen what a worker can do beyond what the caller selected, and its decisions
would not be attributable. dlgt therefore has no approval rule of its own.
In `auto` the provider's reviewer decides, in `ask` the caller decides, and
`bypass` is a policy the caller selected explicitly. A Request that nobody
answers stays pending and the Session stays `blocked` until `answer`,
`cancel`, or `stop`. It never times out into an approval.

Harness options that would override the approval mapping are rejected with
`INVALID_ARGUMENT` before dlgt starts the provider process, so the selected
mode is what dlgt passes. User-owned provider configuration remains the
caller's policy. When Claude reports a different permission mode than the one
requested, for example because auto mode is unavailable for the model, the
Session snapshot reports
`approvals.effective` with a reason, a `notice` message records it, and the
Session behaves as the effective mode.

## Messages replace the screen

v1 retained a rendered terminal screen for humans and agents to follow
progress. Without a terminal, progress is what the provider reported, reduced
to bounded messages:

```text
assistant     text the model wrote between tool calls
tool_call     a tool invocation: tool name and a one-line summary of its input
tool_result   the outcome: ok or not, and a one-line summary
approval      a reviewer decision on an approval request
steer         a message accepted through send --steer
notice        a provider warning or a dlgt observation about the Session
```

Messages are appended in provider order with a per-Session sequence. `text`
and `summary` are cut at 4 KiB, a tool result summary is the first non-empty
output line cut at 240 bytes, model reasoning is dropped, and full tool output
is not retained. A message is never split across responses.

Messages are for following progress. They never complete a `fetch --wait` on
their own, and the authoritative answer is always the retained result's
`final_text`. `watch` prints the same messages as text for a human.

## Observation model

Submission and observation are separate operations:

```text
new / send   deliver a prompt, briefly await confirmation, return a cursor
fetch        one composite forward-delta read from a cursor
```

The submission receipt reports `confirmed` when provider evidence arrives
within five seconds and `pending` otherwise. Codex confirms with the
`turn/start` response; Claude confirms when it starts the prompt or replays
it. Both receipts are successful: the execution is reserved. A `pending`
receipt can precede the end of delivery; a delivery that then fails ends the
execution with a failed result. Replaying
the same idempotency key refreshes a pending receipt from live execution
state; a new key would risk a duplicate execution.

The acceptance cursor is captured under the store lock immediately before the
acceptance is recorded, so a provider that emits output or finishes instantly
cannot move in front of the position the caller was handed.

`fetch` joins the Session snapshot, pending Requests, retained results,
lifecycle events, and the forward message delta. A cursor is an ordinal
position within one Session: the daemon mints the next number and holds the
watermark vector behind it. Encoding that vector into the token bought no
durability, because it addresses memory-only state either way, while costing
an LLM caller hundreds of characters of context per turn. Positions bind to
the immutable internal Session identity rather than the public Session ID, so
a Claude rekey does not invalidate them, and pre-rekey IDs stay resolvable
for the daemon lifetime.

Retention is bounded and eviction is observable: 10,000 messages per Session,
50,000 lifecycle events per daemon, and 128 results or 16 MiB of result
bodies per Session. A cursor that predates an eviction returns a structured
gap with a bounded baseline and a fresh cursor; a silent reset is never
acceptable. A position the daemon never minted or no longer retains is a
structured error whose recovery is one cursorless baseline fetch. Positions
are meaningful only within one daemon lifetime: a number kept across a restart
names the new daemon's window rather than being detected as stale, so callers
re-enter through an acceptance cursor or a baseline.

Every observation is a success. Timeout, blocked input, a failed execution, and
a canceled result all exit zero and are described by the response `reason` and
result status. Non-zero exits remain for malformed requests, unknown Sessions,
unusable cursors, and transport failure.

A response is assembled one unit at a time. Each watermark is recorded only
when the unit it describes is committed and survives the final measurement,
and the published vector is derived last from the committed set, so a squeezed
response cannot leave a watermark past data the caller never received. The
position number is reserved before rendering, so the document carries its own
number while it is being measured; a response whose watermark vector did not
move keeps the caller's position instead of spending a new one.

## Authentication reporting

dlgt reports authentication and never enforces it. Before each process
launch it matches the final launch environment against the Harness's list of
credential-related variable names and records the non-empty ones as
`auth_env`. A non-empty list adds a `warning` to the launch receipt and a
`notice` message to the Session. Only names are recorded, never values. The
list reflects the launch environment the client sent, not the daemon's own.

For Claude, `system/init` is the second source. `apiKeySource: "none"` reports
`auth: "subscription"`, and any other value reports `auth: "api_key"` with a
notice. This catches API-key helpers and settings that no environment variable
reveals. The process-level auth report and resolved model survive turn
settlement, even when Claude emits `system/init` only once per process. Other
Harnesses have no `auth` field. The variable lists are defined in
[CLI](cli.md#profiles-and-launch-environment).

## Process ownership

The daemon spawns each provider in its own process group with piped stdin,
stdout, and stderr, and keeps a bounded stderr tail for launch failures and
crash notices. A sibling reaper process, `dlgt server --reaper`, runs in a
separate process group, ignores ordinary terminal and service shutdown
signals, and holds the registered provider groups. Normal teardown
unregisters a group. If the daemon exits without running destructors,
control-pipe EOF makes the reaper terminate every group still registered,
including the Codex app-server group.

Runtime state is memory-only. A daemon that owns no Sessions exits after five
minutes. A restarted daemon recovers nothing and dispatches nothing; the
caller re-enters with `send --resume`.

dlgt holds one process per provider conversation. Resuming a conversation
reserves it first, so a second `--resume` of a conversation that is already
live reuses that Session instead of starting a second process on it.

## Launch environment and security

Profiles are expanded by the client before RPC:

```text
client snapshot or clean base < Profile < explicit launch options
```

The invoking client's environment, not the daemon startup environment, is the
default launch base. Launch environment values travel in RPC memory and are not
serialized into Session metadata, events, Profiles, or errors. Provider output
can still echo them, so results and messages remain sensitive output rather
than a redaction boundary. Configuration defaults and the Claude
authentication policy are defined in
[CLI](cli.md#profiles-and-launch-environment).

dlgt applies child-scoped lifecycle overrides without mutating global provider
configuration: Codex receives `check_for_update_on_startup=false`; Claude
receives `DISABLE_AUTOUPDATER=1`. It also marks the Session working directory
as trusted in each provider's workspace state.

The default approvals mode is `auto`. The provider's reviewer, not dlgt,
decides what runs; `ask` sends every approval to the caller, and `bypass`
disables approvals and the Codex sandbox only when the caller selects it.
Codex `workspace-write` still allows writes under the system temporary
directories without an approval request.

Provider channels are untrusted input. dlgt answers only the Codex server
requests and Claude control requests it maps to Requests and refuses every
other request explicitly, so no provider request can hang a Session or make
dlgt act on its own.

The local RPC socket is mode 0600. `rpc --stdio` exposes only the public method
allowlist.

## Events and results

Events normalize lifecycle and actionable blocked state. Steering, approvals,
and provider output are messages, not events. The public event set is defined
in [RPC](rpc.md#lifecycle-events). Every public event is versioned and ordered
by a daemon sequence number. Internal diagnostics, such as unmatched provider
completions, are retained but not published.

Every accepted execution ends with exactly one retained result. Its `status`
is `completed`, `failed`, `canceled`, or `interrupted`, and a failed recovery
of `final_text` never turns a completed execution into a failure.

## Acceptance criteria

1. Two concurrent default sends to one idle Session yield one accepted
   execution and one side-effect-free busy rejection.
2. `new` plus an initial prompt succeeds atomically or reports one failure
   without a live half-created alias.
3. Every startup reaches ready or terminal failure within its deadline.
4. Every accepted execution receives exactly one `execution_seq`, one
   acceptance cursor, and one retained terminal result.
5. Provider death during execution produces a retained result and terminal
   Session state in bounded time.
6. A pending Request keeps the Session `blocked`, is never inferred as
   completion, and never resolves without `answer`, provider withdrawal,
   `cancel`, `stop`, or `restart`.
7. dlgt never sends an approval the caller, the provider reviewer, or the
   selected mode did not decide.
8. Cancel never returns the Session to `idle` before the provider reports the
   interrupted turn's result.
9. A steer never leaks into a later execution: it is delivered within its
   execution or reported expired. For Claude, an execution does not settle
   while a written message is unread.
10. A retried acceptance carrying the same request ID replays its original
    receipt instead of creating a second Session or execution.
11. Daemon exit stops owned provider processes and discards all runtime-local
    Session state; no work is recovered or dispatched after restart.
12. Provider silence and timeout are never accepted as lifecycle proof, and
    new messages alone never complete a long poll.
13. All control success and error paths emit schema-valid JSON, and every
    public event carries `schema_version`.
14. Every provider request dlgt does not map to a Request is refused
    explicitly rather than left unanswered.

## Design decisions

### Session remains the public address

Callers operate on one live Session identity. Internal execution and
delivery records exist for atomicity and observation, not as a second public
API.

### The PTY was dropped

v1 ran each provider's interactive TUI in an owned PTY, rendered it with a VT
emulator, and inferred Claude state from lifecycle hooks injected into the
child. Both providers now expose a structured channel: Codex app-server over
stdio and Claude `-p` with stream-json input and output. That channel carries
everything the PTY was used for, with stronger guarantees:

- acceptance is evidence, not a key write: a `turn/start` response or a
  `command_lifecycle` event instead of a paste followed by Enter;
- approvals and questions are data that the caller can answer by ID, instead
  of a dialog only an attached human could answer;
- Claude steering is a queued user message with a replay, instead of a hook
  bridge at tool boundaries;
- progress is a bounded message list, instead of a rendered screen that needed
  a VT emulator, screen epochs, and fragment paging;
- dlgt writes no hook configuration and never types into an editor, so there
  is no input to clear and no user hook to coexist with.

The cost is that a human can no longer attach to a worker. `watch` follows the
same messages read-only, and a human who wants to take a conversation over
stops the Session and runs its `resume_command`.

### Steering is not a queue

Steering changes the current execution and expires with it. dlgt does not hold
a prompt for later dispatch: a busy Session rejects new work without side
effects, so ordering, cancellation, and result ownership stay unambiguous.

### Replacement is not restart

Replacing running work is `cancel` followed by a plain `send` inside the same
provider conversation. Restart replaces the provider process. Keeping them
separate avoids using a process kill as routine flow control and preserves
provider context and diagnostics.

### Provider asymmetry is explicit

Codex has semantic app-server methods for turn start, steer, and interrupt.
Claude has a user-message queue, replays, and control requests. dlgt exposes
one intent model but documents that Claude steering is delivered at the next
model request and that one Claude execution can span several provider turns,
instead of claiming identical behavior.

### dlgt is never a reviewer

dlgt routes approvals and records who decided them; it does not decide. See
[Why dlgt never approves](#why-dlgt-never-approves).

### Submission is never fused with a long wait

A command that accepts work and then blocks for minutes loses its acceptance
receipt whenever the caller's harness kills, backgrounds, or swallows the
response, which produces duplicate Sessions and Session-ID rediscovery. `new`
and `send` therefore wait only a short bounded confirmation window before
returning a cursor. Waiting for progress or a result is a separate, replayable
`fetch`.

### Timeouts express bounded observation

Startup, cancellation, and restart waits are bounded, and a `fetch` deadline is
an observation bound. Timeout never manufactures completion, quiescence,
cancellation, or approval, and it is reported as a successful observation
rather than an error.

### Notifications are clients

Arbitrary completion commands in the daemon create reentrancy, secret, and
failure-policy problems. Consumers follow normalized events instead.

### Identifiers have distinct roles

Aliases are readable active-session conveniences. Provider-qualified Session
IDs remain unambiguous after alias reuse. `execution_seq` correlates accepted
work and results without creating a public Turn identity. Request IDs are
short per-Session ordinals such as `r3`, so they survive an agent's context.

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

## Known limitations

Claude stream-json input, its control requests, and `command_lifecycle` events
are not a documented stable interface. The behavior that was not measured
against a provider is listed in [CLI](cli.md#open-questions).

## Provider references

- [Codex app-server](https://developers.openai.com/codex/app-server/)

## Superseded concepts

The final design removes these earlier assumptions:

- a daemon-owned prompt queue can coexist with a one-execution Session;
- one overloaded queue operation can mean both steering and a future turn;
- replacement should kill and restart the provider process;
- an atomic `replace` send mode is needed; `cancel` then `send` is enough;
- provider quietness or prompt text can prove readiness or completion;
- public Turn, Queue, Operation, or Delivery resources are required;
- provider implementations must pretend to offer identical steering strength;
- optional or unbounded lifecycle waits are acceptable;
- acceptance and a long wait can share one command;
- a bounded observation timeout is an error rather than an observation;
- workers should be auto-approved by dlgt; the provider reviewer or the
  caller decides instead.

The v1 PTY runtime is superseded by the structured transports:

- an owned PTY per Session: replaced by the provider's stdio channel;
- the Codex remote TUI beside a private app-server connection: the app-server
  alone drives the Session;
- Claude lifecycle and steering hooks injected into the child: replaced by
  stream-json events, replays, and control requests;
- a VT-rendered screen, stable rows, and screen epochs in `fetch`: replaced by
  messages;
- `scrollback` and raw PTY reads through `logs --raw`: no terminal bytes
  exist;
- `attach` as an exclusive input lease and the `SESSION_ATTACHED` and
  `ALREADY_ATTACHED` errors: blocked input is answered with `answer`, and
  `watch` is read-only;
- `LAUNCH_BLOCKED` for a startup screen only a human could answer: a provider
  that cannot start without interaction is `LAUNCH_FAILED` with the provider
  command to run once by hand;
- bracketed-paste prompt delivery and input clearing before a Claude turn:
  prompts are `user` messages on stdin;
- Cursor, Grok, OpenCode, and Pi harnesses driven through their TUIs: only
  Codex and Claude remain.
