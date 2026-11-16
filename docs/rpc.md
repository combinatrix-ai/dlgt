# dlgt local RPC v2

This document is the normative programmatic interface. Command syntax and exit
statuses belong in [CLI](cli.md); provider integration and security rationale
belong in [Design](design.md).

## Transport and framing

dlgt uses newline-delimited JSON over a mode-0600 Unix socket. `dlgt rpc
--stdio` proxies only the public methods in this document. Each request
produces exactly one response line.

Request:

```json
{"id":"req_1","method":"session.fetch","params":{"session":"codex:019f6307-341e-7e81-8a33-7ab61e804345","wait_ms":900000}}
```

Success:

```json
{"id":"req_1","result":{"schema_version":2,"reason":"result","cursor":"4","sessions":[]}}
```

A successful response may also carry an informational notice without changing
the result:

```json
{"id":"req_1","result":[],"info":{"code":"UPDATE_AVAILABLE","current_version":"0.1.4","latest_version":"0.2.0","command":"dlgt update"}}
```

`info` is advisory. Clients should present it separately from the result, and
must obtain user confirmation before acting on `UPDATE_AVAILABLE`.

Failure:

```json
{"id":"req_1","error":{"code":"CURSOR_EXPIRED","message":"this daemon no longer holds position 3"}}
```

Raw RPC responses do not use the CLI's `ok:true` or `ok:false` wrapper. Blank
input lines are ignored. Invalid JSON, framing failures, and a closed transport
terminate the stdio proxy. The proxy rejects a method outside the public list
with `INVALID_ARGUMENT`.

A request `id` is echoed in the response envelope, so it is bounded: an id
longer than 200 bytes is rejected with `INVALID_ARGUMENT`, and the rejection
echoes only a truncated prefix of it.

## Public methods

```text
session.create        Create a Session with its required initial prompt
session.restart       Replace a Session process and resume provider context
session.send          Accept work on an idle Session, steer a busy one, or resume
session.answer        Answer a pending approval or question
session.fetch         Read one Session since a cursor, optionally waiting
session.cancel        Interrupt active work, bounded by timeout_ms
session.list          List active or all Sessions
session.read          Read live Session state, pending Requests, and latest result
session.stop          Stop the Harness process group
model.list            Discover one Harness's models
profile.list          List client-side Profiles
```

`session` parameters accept a provider-qualified `codex:<thread-id>` or
`claude:<session-id>` Session ID, or the active human alias. `harness` is
`codex` or `claude`.

| Method | Parameters |
| --- | --- |
| `session.create` | `title`, optional `alias`, `harness`, optional `cwd`, optional `model`, optional `effort`, optional `approvals` (`auto`, `ask`, or `bypass`; default `auto`), optional `harness_options`, required non-empty `prompt`, required `request_id`, optional `startup_timeout_ms`, launch `environment` |
| `session.restart` | `session`, optional `startup_timeout_ms`, fresh launch `environment` |
| `session.send` | `session`, required non-empty `prompt`, required `request_id`, optional `steer` boolean, optional `resume` boolean; with `resume:true`, the launch parameters of `session.create` except `title`, `alias`, and `harness` |
| `session.answer` | `session`, `request` (a Request ID such as `r3`), `decision` (`allow`, `deny`, `text`, or `choice`), `message` with `deny`, `text` with `text`, `choice` (1-based) with `choice` |
| `session.fetch` | `session`, optional `cursor` (a position, as a number or a string of digits), optional `wait_ms` (max 86,400,000), optional `messages` (boolean or a positive count), optional `max_bytes`, optional `from_start` boolean |
| `session.cancel` | `session`, optional `timeout_ms` with a 30-second default |
| `session.list` | optional `all` boolean |
| `session.read` | `session` |
| `session.stop` | `session`, optional `force` boolean |
| `model.list` | `harness`, optional `include_hidden` |
| `profile.list` | no parameters |

`steer` and `resume` are mutually exclusive. Profiles are expanded by the
client. `profile.list` is implemented by the stdio proxy rather than delegated
to the daemon, so the daemon does not reread mutable client configuration.
The CLI scans live versioned daemon sockets before `send`, `fetch`, and
`answer`. Raw JSONL RPC is scoped to the selected socket; callers using RPC
directly must select the owning versioned socket themselves.

### Idempotent acceptance

`session.create` and `session.send`, including steer and resume, are
acceptances. Their `request_id` is a required caller-chosen idempotency key:
non-empty and at most 128 bytes. A missing, empty, or over-long key fails with
`INVALID_ARGUMENT`. The daemon retains the last 1,024 acceptance receipts for
its lifetime. Repeating an ID with the same payload returns the original
receipt with `replayed: true`; repeating it with a different payload fails with
`INVALID_ARGUMENT`.

It is required rather than optional because a key that first appears on the
retry cannot deduplicate anything: the original attempt has already created a
Session. This is separate from the RPC envelope's `id`, which correlates one
request and is capped at 200 bytes.

Payload identity is the canonical form of the RPC parameters:

- every parameter except `environment`, `correlation_id`, and `request_id`,
  which legitimately differ between retries of the same acceptance;
- object keys in sorted order, so a client that emits parameters in a
  different order still matches;
- array values in the order given, so `harness_options` must be repeated in
  the same order;
- `prompt` compared byte for byte. A trailing newline is significant.

An acceptance reserves its `request_id` before it runs. A second call arriving
while the first is still launching waits until the winner settles and then
replays its receipt, so two concurrent calls can never create two Sessions.
A different payload is rejected against an in-flight reservation as well as a
stored receipt. A failed acceptance stores no receipt, so the same ID may be
retried.

`session.answer` is idempotent by Request instead: the same answer to the same
Request returns the original receipt with `replayed: true`.

### Acceptance receipts

`session.create` and `session.send` return:

```json
{"session":{"id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","state":"busy"},"execution_seq":2,"cursor":"4","submission":"confirmed"}
```

`submission` is `confirmed` or `pending`. The daemon waits up to five seconds
for the provider's acknowledgement: the `turn/start` response for Codex, and
Claude starting or replaying the prompt. `pending` is still success: local
delivery occurred, so the caller must observe or replay the identical request
ID rather than submit with a new one. A replay refreshes this field from the
live execution and can change `pending` to `confirmed` without executing the
prompt twice. Pending receipts also include a human-readable `hint` and an
`action` containing a bounded `fetch`.

The `cursor` is the Session's observation position, taken under the store lock
immediately before the acceptance is recorded, so the first `session.fetch`
from it cannot miss output produced between acceptance and the caller's next
request.

A steer returns the running execution instead of a new one:

```json
{"session":{"id":"claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629","state":"busy"},"execution_seq":2,"steer":"accepted","cursor":"9"}
```

Steering requires a `busy` Session. Idle is `NO_ACTIVE_EXECUTION`, blocked is
`SESSION_BLOCKED`, and canceling is `SESSION_UNAVAILABLE`. Delivery is
reported later through `steer` messages. See
[Design](design.md#steering) for how each provider delivers it.

### Answers

```json
{"id":"req_7","method":"session.answer","params":{"session":"claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629","request":"r3","decision":"deny","message":"Do not force-push. Open a new branch instead."}}
```

```json
{"session":{"id":"claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629","state":"busy"},"request":{"id":"r3","resolution":"denied"},"cursor":"11"}
```

`resolution` is `allowed`, `denied`, or `answered`. `allow` and `deny` answer
an approval; `text` and `choice` answer a question. The Session must be live:
otherwise the call fails with `SESSION_NOT_RUNNING`. An unknown Request is
`REQUEST_NOT_FOUND`, and a different answer to a resolved Request is
`REQUEST_RESOLVED`. The answer returns as soon as the provider accepted it.

### Other results

| Method | Result |
| --- | --- |
| `session.restart` | `{"session": Session}` |
| `session.list` | an array of Sessions |
| `session.read` | `{"session", "requests", "result", "execution_seq", "resume_command"}`, plus `launch_failure` for a Session that failed before it became ready |
| `session.cancel` | `{"session", "canceled": true, "result"}`, or `{"session_id", "canceled": false, "reason": "NO_ACTIVE_WORK"}` for an idle Session |
| `session.stop` | `{"stopping": true, "force", "session_id"}` |
| `model.list` | the Harness's model catalog; see [CLI](cli.md#model-discovery) |

`resume_command` is the provider's own command for opening the conversation
in its interactive UI, such as `claude --resume <id>` or `codex resume <id>`.

## Session and result schemas

A public Session contains:

```json
{
  "id": "claude:8bc7859c-4c82-4b9a-a00d-2f3c483a9629",
  "alias": "@run-review-361csx",
  "title": "run review",
  "harness": "claude",
  "cwd": "/work/repo",
  "state": "busy",
  "model": "best",
  "effort": null,
  "resolved_model": "claude-fable-5",
  "approvals": {"requested": "auto", "effective": "auto"},
  "auth": "subscription",
  "marker": "dlgt:8bc7859c4c82",
  "created_at_ms": 1784024104395,
  "updated_at_ms": 1784024252019,
  "busy_for_ms": 183000,
  "quiet_for_ms": 72000
}
```

The suffix of `id` is the Codex thread ID or Claude session ID, so the same
value correlates provider-native logs, addresses a live Session, and resumes
it after daemon exit. Provider turn IDs, internal launch IDs, and internal
execution row IDs are excluded.

- `resolved_model` is the model the provider reported, present once known.
- `approvals.effective` is the mode the provider reports it runs. When it
  differs from `requested`, `approvals.reason` says why.
- `auth` is `subscription` or `api_key`, present only for Claude and only after
  the first turn of a process. See
  [CLI](cli.md#profiles-and-launch-environment).
- `marker` is present only for Claude. It is part of the name dlgt gives the
  provider conversation, so the conversation can be found in Claude's own
  session list.
- `busy_for_ms` and `quiet_for_ms` are present only when `state` is exactly
  `busy`. `busy_for_ms` is elapsed time since the current execution was
  reserved. `quiet_for_ms` is elapsed time since the provider last emitted any
  event, clamped to the busy interval; if no event has occurred it equals
  `busy_for_ms`. They are diagnostic only and never transition a Session or
  permit another send.

The public state set is:

```text
starting  idle  busy  blocked  canceling  stopping  restarting  stopped  failed
```

Every accepted execution receives a per-Session monotonic `execution_seq`.
This is correlation data, never an RPC selector or public resource ID.
The returned Session state is a snapshot taken when the response is built.

A retained result has this shape:

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

`status` is `completed`, `failed`, `canceled`, or `interrupted`.
`final_text` is always a string, although it may be empty. `error` is `null`
or a string describing the failure; a provider failure carries a JSON-encoded
object with `code` and `message` in that string. `usage` is Claude's token
counts, or `null` for Codex. `denied_requests` counts approvals denied or timed
out during the execution, by the provider's reviewer or by the caller.

`final_text_source` is `provider` when the Harness turn result reported the
text, `messages` when the turn result carried no text and dlgt used the last
assistant message streamed during this execution, and `missing` when no text
was recovered. A failed recovery never changes the execution status.

## Requests

A pending Request has this shape:

```json
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
```

`kind` is `approval` or `question`. An approval carries `tool`, `summary`,
`input`, and, when the provider supplied them, `cwd` and `reason`. An `input`
larger than 2 KiB is replaced by `{"truncated": true, "preview": "..."}`. A
question carries `text` and, when the provider supplied them, `choices`. When
a provider asks several questions in one request, `text` lists them one per
line and there are no `choices`.

`id` is a short per-Session ordinal. At most 16 pending Requests are published
per response; when more are pending, the bucket carries
`"requests_truncated": true`.

## Composite reads

`session.fetch` returns one document per request. There is no streaming
variant and no partial output.

```json
{
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
        {"execution_seq":7,"status":"completed","final_text":"Review result...","final_text_source":"provider","final_text_offset":0,"final_text_complete":true,"denied_requests":0,"error":null,"started_at_ms":1784024104395,"completed_at_ms":1784024252019,"usage":null}
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

`reason` is `snapshot`, `change`, `result`, `blocked`, `page_full`, `gap`, or
`timeout`. Every one of them is a successful response; `results[].status`
remains the authority on whether an execution succeeded.

Rules:

- Without `cursor`, the response is a bounded baseline: current state,
  pending Requests, the latest retained result, the last 64 messages, and a
  fresh cursor.
- `from_start: true` without a cursor starts at the oldest retained message,
  event, and result instead of the baseline.
- A positive `wait_ms` binds to the execution active, or latest, at the first
  evaluation. A later execution never extends the bind. A pending Request, a
  page-full response, a gap, or the deadline returns early.
- New messages alone never complete a wait.
- Replaying a cursor replays the same immutable events, results, and messages.
  The Session snapshot and pending Requests are current and may be newer on
  replay. Nothing is advanced or consumed server-side.

### Messages

```json
{"seq":57,"execution_seq":7,"kind":"approval","decision":"approved","by":"provider_reviewer","risk":"low","tool":"exec","summary":"sudo -n true"}
{"seq":58,"execution_seq":7,"kind":"approval","decision":"denied","by":"caller","request_id":"r3","tool":"Bash","summary":"git push --force origin main"}
{"seq":61,"execution_seq":7,"kind":"steer","delivered":true,"text":"Skip the integration tests; unit tests are enough"}
{"seq":62,"execution_seq":7,"kind":"notice","text":"approvals run as ask: provider reported permission mode default"}
```

| `kind` | Fields |
| --- | --- |
| `assistant` | `text` |
| `tool_call` | `tool`, `summary` |
| `tool_result` | `tool`, `ok`, `summary` |
| `approval` | `decision`, `by`, optional `risk`, optional `request_id`, `tool`, `summary` |
| `steer` | `delivered`, optional `expired: true`, `text` |
| `notice` | `text` |

Every message also carries `seq` and, when it belongs to an execution,
`execution_seq`. `decision` is `approved`, `denied`, `timed_out`, or
`aborted`. `by` is `provider_reviewer`, `caller`, or `provider_policy`; dlgt
is never a value of `by`. `text` and `summary` are cut at 4 KiB with
`"truncated": true`. `messages: false` omits the delta, and a count caps it at
up to 512 per response (default 64).

### Bounds and pagination

Bounds are part of the contract: 32 KiB serialized by default and 256 KiB
hard, 64 events, 4 results, 64 messages (512 on request), and 16 pending
Requests per response. `has_more: true` means data already exists beyond the
returned cursor, that response uses `reason: "page_full"`, and the next
request returns immediately even with `wait_ms`.

`max_bytes` bounds two things: the `result` document itself, and the complete
compact response line the CLI prints, whose `{"ok":true,...}` wrapper and
newline the daemon reserves before committing content. When the budget
squeezes a response, the daemon keeps state and gaps, then pending Requests,
then terminal results, then events, and drops messages first.

It does not bound the raw JSONL envelope. That envelope carries a
caller-supplied request `id`, so its size is the caller's to control; the id
is capped at 200 bytes before JSON escaping. An optional `info` notice and
pretty-printing are also outside the bound. The cursor is derived from what
the document actually carries, so it can never advance past omitted content.
Progress under a tight budget comes from chunking: a long `final_text` is
chunked at a UTF-8 boundary and continued with `final_text_offset` and
`final_text_complete`, and every `has_more` response advances at least one
watermark. A `max_bytes` too small for the envelope plus one chunk fails with
`INVALID_ARGUMENT` naming a verified workable value instead of emitting an
oversized document. That value is verified by actually rendering a response
at it, so retrying at exactly it succeeds; it is a verified candidate, not a
guaranteed minimum, because progress is not monotonic in the budget.

Lifecycle events and messages are each committed as a strictly ascending
prefix, so a response never skips one to deliver a later one.

### Cursors

A cursor is an ordinal position. Responses serialize it as a string of digits
for field stability; requests accept either spelling, a JSON number or a string
of digits. Absent or `null` requests a baseline. Any other type, and any string
that is not a position, is `CURSOR_INVALID` rather than a silent baseline.
Leading zeros are accepted and canonicalized. Every acceptance, and every
fetch response whose watermark vector advanced, mints the Session's next
position; a fetch whose vector did not move returns the caller's own position,
though an empty public delta can still mint when internal bookkeeping advanced
beneath it. The daemon holds the watermarks behind each position and never
mutates them, which is what makes replaying a position return an identical
window.

The position is an ordinal within the Session addressed, not a capability
token: using one Session's number against another resolves that other
Session's own position of the same number, which is a legitimate replay.
Idempotency belongs to `request_id`. Positions bind to an internal Session
identity, so a Claude provider-ID rotation keeps them valid.

A position is meaningful only within one daemon lifetime. A number from a
previous daemon is not rejected: it names this daemon's window of that
position, or fails `CURSOR_EXPIRED` if this daemon has not minted that far.
After `RPC_UNAVAILABLE`, `SESSION_NOT_RUNNING`, or any daemon restart,
discard remembered positions and re-enter through a resume acceptance cursor
or a cursorless baseline.

The daemon retains the most recent 64 positions per Session, within a global
cap of 4,096; beyond either bound the oldest are dropped.

Retention is bounded to 10,000 messages per Session, 50,000 lifecycle events
per daemon, and 128 results or 16 MiB of result bodies per Session. A cursor
that predates an eviction returns `reason:"gap"`, a `gaps` entry of
`{"component":"messages"|"events"|"results","reason":"retention_overrun"}`, a
bounded baseline, and a fresh cursor. Per-Session components (`messages`,
`results`) are reported on the Session bucket and scope-wide components
(`events`) in the top-level `gaps` array. dlgt never silently resets a cursor.

Lifecycle events are materialized when they are recorded and are scoped by an
internal Session identity, so replaying a cursor returns byte-identical events
even after the execution they describe has been evicted or the public Session
ID has rotated. `session_id` on an event is the ID that was published when the
event happened.

## Lifecycle events

`session.fetch` returns normalized lifecycle events for one Session in its
`events` array. There is no cross-Session event read or stream.

```jsonl
{"schema_version":2,"seq":101,"type":"session.created","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":2,"seq":102,"type":"session.ready","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":2,"seq":103,"type":"session.busy","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1}
{"schema_version":2,"seq":104,"type":"provider.retrying","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1,"attempt":1}
{"schema_version":2,"seq":105,"type":"session.blocked","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1,"reason":"user_input"}
{"schema_version":2,"seq":106,"type":"session.resumed","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1}
{"schema_version":2,"seq":107,"type":"session.idle","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345","execution_seq":1,"result_status":"completed"}
{"schema_version":2,"seq":108,"type":"session.restarting","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":2,"seq":109,"type":"session.ready","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":2,"seq":110,"type":"session.stopping","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
{"schema_version":2,"seq":111,"type":"session.stopped","session_id":"codex:019f6307-341e-7e81-8a33-7ab61e804345"}
```

The complete v2 event type set is:

```text
session.created      the Session was created
session.restarting   a restart began
session.ready        the provider is ready
session.busy         the provider started the execution
session.blocked      a Request is pending
session.resumed      the last pending Request closed
session.canceling    a cancel was requested
session.idle         the execution ended; carries result_status
session.stopping     a stop was requested
session.stopped      the provider exited after a stop
session.failed       startup failed, or the provider exited unexpectedly
provider.retrying    the provider is retrying a request; carries attempt
```

Every event contains `schema_version`, a global monotonic `seq`, `type`, and
`session_id`, plus `execution_seq` when it belongs to an execution.
`session.blocked` carries `reason: "user_input"`.

Events carry lifecycle and actionable state. Provider output, steering, and
approval decisions are messages.

## Error contract

RPC failures contain a stable `code` and a human-readable `message`. The v2
code families are:

```text
INVALID_ARGUMENT       Request cannot be retried unchanged
NOT_FOUND              Session or configuration object does not exist
NO_RESULT              Session has never accepted work
NO_ACTIVE_EXECUTION    A steer found no running execution
SESSION_NOT_RUNNING    No live Session matches; retry with resume
ALIAS_IN_USE           Exact alias belongs to a non-terminal Session
SESSION_BUSY           Active execution; retry after it terminalizes
SESSION_BLOCKED        A Request is pending; answer it
SESSION_UNAVAILABLE    Session state cannot accept the operation
REQUEST_NOT_FOUND      No such Request on this Session
REQUEST_RESOLVED       The Request already has a different answer
CURSOR_EXPIRED         Cursor position was never minted or is no longer held
CURSOR_INVALID         Cursor value is not a position number
CANCEL_TIMEOUT         Cancel wait expired; cancellation continues
LAUNCH_FAILED          Harness startup or initial prompt acceptance failed
RPC_UNAVAILABLE        Daemon transport is unavailable; retry may succeed
INTERNAL               dlgt runtime invariant failure
```

Methods may add contextual error fields but must not overload a code with a
different retry or human-action policy. The contextual fields are:

| Field | Meaning |
| --- | --- |
| `session_id` | The canonical Session the error concerns. |
| `launch_id` | The `internal:<short-id>` of a launch that failed before provider binding. It is never a Session ID. |
| `session_state` | The Session's public state when the error was built. |
| `request_ids` | Pending Request IDs, with `SESSION_BLOCKED`. |
| `action` | One command that resolves the error, such as `dlgt answer <SESSION_ID> r3 --allow\|--deny`. |
| `hint` | Human-readable recovery guidance. |
| `detail` | The provider's stderr tail, with `LAUNCH_FAILED`. |
| `correlation_id` | The caller correlation ID of the failed acceptance. |

`SESSION_NOT_RUNNING` always carries the hint to retry with `resume` and the
same Session ID.

A launch failure is `LAUNCH_FAILED` with `detail` and a `hint`. Without a
terminal there is no startup screen for a human to answer, so a provider that
cannot start without interaction fails, and `action` names the provider
command, `claude` or `codex`, that the user runs once by hand in the Session's
working directory. `session.read` accepts the `launch_id` and returns the same
diagnostics as `launch_failure`. CLI exit-status mapping is defined in
[CLI](cli.md#exit-statuses).

## Security boundary

`rpc --stdio` uses a fixed allowlist. Internal methods such as `server.ping`
and `server.stop` are unavailable even if a caller names them directly.

Provider turn IDs, provider request IDs, internal Session UIDs, and internal
execution row IDs never appear in public responses or normalized events; a
cursor is a bare position number and carries no identity data. Launch
environment values travel in RPC memory but are not serialized into Session
metadata, errors, Profiles, or events. Results, messages, and Request inputs
remain untrusted and potentially sensitive because a provider can echo its
environment.
