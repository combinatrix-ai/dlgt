---
name: dlgt
description: Delegate work to the competing harness - run Claude from Codex, or Codex from Claude, as a live session that accepts follow-ups. Use when the other model should review, implement, or give a second opinion, or when a subagent must stay alive and addressable across commands.
---

# dlgt

`dlgt` runs a Codex or Claude subagent in a dlgt-owned PTY that stays alive
between commands. Reach for it whenever the work should cross the provider
boundary.

On the harness you are already running, its own built-in subagent call is the
cheaper default. Use dlgt there too when the delegation needs something that
call cannot express:

- a model or `--effort` pinned to that one worker
- a Session that stays alive and keeps its context across later follow-ups
- observation of the worker's screen, or a distinct `--cwd`, launch
  environment, or approval posture

Outside those, prefer the built-in subagent: dlgt costs a daemon, a process,
and a PTY.

Terms used below:

```text
Session   One Harness process and PTY, one controller, at most one active
          execution, no queue. The only public runtime object.
Harness   The provider adapter, codex or claude.
Title     A human description.
Alias     A short human address derived from the title.
Profile   A reusable client-side launch specification.
```

## The contract in one paragraph

Delegation is two phases. `new` and `send` SUBMIT work: after a short bounded
confirmation window they return a successful submission receipt —
`submission: "confirmed" | "pending"`, `session.id`, `execution_seq`, and a
small `cursor` number — never the answer. `pending` means local delivery
succeeded but the provider acknowledgement has not arrived; it is not a reason
to send again. Codex confirms with its app-server turn lifecycle; Claude
confirms with the matching `UserPromptSubmit` hook. `fetch` is the command for
forward cursor-based observation of later state, results, lifecycle events,
and screen. What you must
retain between commands: the provider-qualified `session.id`, the latest
`cursor`, and — until each submission receipt is safely in your visible output
— the `--request-id` you chose with its byte-identical prompt and options.
Within the same daemon lifetime, re-running that identical command with the
identical id replays the original receipt (`"replayed": true`) instead of
starting a second worker. Invent the request-id BEFORE the first attempt; a
key that first appears on a retry is too late to deduplicate anything. An
`--alias` is a human convenience and does not deduplicate.

## Standard workflow when running as Codex

The complete arc, in order. Placeholders such as `<SESSION_ID>` are literal
substitutions; `<WORKDIR>` must be written as an absolute path — `$PWD` does
not expand inside a JSON field.

### 1. Submit

Put a multi-line prompt in a file (see "Long prompts" below), then:

```bash
dlgt new --title "long refactor" --harness claude --cwd /abs/path \
  --request-id refactor-1 --stdin < prompt.md
```

Let the submission JSON print. Do NOT capture it into a shell variable and
stay silent — variables do not survive the call, and the receipt must land in
your visible output. Read `submission`, `session.id`, and `cursor` out of it. If
`submission` is `pending`, follow its `action` and never resend with a new
request ID. The `action` is the exact `fetch` command that observes whether
the submitted execution appears. If this response is lost or killed, re-run
the byte-identical command with the identical `--request-id`; within the same
daemon lifetime it replays the original receipt and may upgrade `pending` to
`confirmed` without submitting again. Never issue a second bare `new` because
you are unsure whether the first landed. See "Recover a lost response" for the
daemon-restart boundary.

### 2. Choose how completion returns

Read the `exec` tool's own documentation in this session before observing. If
its JavaScript environment explicitly names both `yield_control()` and
`notify()`, use the delayed-delivery path below. It lets the parent continue
useful work and delivers the completed fetch back into the SAME active turn.
If the documentation is unclear, a one-line probe cell —
`text(typeof yield_control + " " + typeof notify)` — settles whether both are
functions without calling a missing helper. This capability has been verified
in Codex Desktop Code Mode with the 0.146.x host; detect the helpers themselves
rather than inferring support from a product name, feature flag, or TTY
presence.

If either helper is unavailable, use the explicit-collection path instead.
Also prefer explicit collection when the delegation result is already the
parent's next dependency and there is no useful work to do concurrently.
Without Code Mode cells at all, skip both paths and use the short forward polls
under "Pacing and final fallback".

Never send the parent's final response while a delayed listener is outstanding.
`notify()` can re-enter an active turn; it does not resurrect a finalized turn.
This JavaScript helper is also unrelated to Codex's `config.toml` `notify`
command, which only sends an external turn-complete notification.

### 3A. Delayed delivery when `yield_control` and `notify` exist

A bare `dlgt fetch --wait 30m` under Code Mode's nested executor is clamped
to 30 seconds and gets orphaned: the cell reports an empty completion while
the fetch runs on, unseen. Hold the process handle, yield the parent turn, and
await every continuation inside the same cell — substitute the three
placeholders and run this as one `exec` call:

```js
// @exec: {"yield_time_ms": 5000, "max_output_tokens": 30000}
const chunks = [];
let r = await tools.exec_command({
  cmd: "dlgt fetch '<SESSION_ID>' --cursor '<CURSOR>' --wait 30m --max-bytes 24000",
  workdir: "<WORKDIR>", yield_time_ms: 1000, max_output_tokens: 30000
});

if (r.session_id == null) {
  const output = (r.output ?? "").trim();
  if (r.exit_code !== 0) throw new Error("dlgt fetch exited " + r.exit_code + "\n" + output);
  JSON.parse(output);
  text(output);
} else {
  text("dlgt listener armed for <SESSION_ID> from cursor <CURSOR>; continuing in the parent turn.");
  yield_control();
  try {
    for (;;) {
      chunks.push(r.output ?? "");
      if (r.session_id == null) break;
      r = await tools.write_stdin({
        session_id: r.session_id, chars: "",
        yield_time_ms: 300000, max_output_tokens: 30000
      });
    }
    const output = chunks.join("").trim();
    if (r.exit_code !== 0) throw new Error("dlgt fetch exited " + r.exit_code + "\n" + chunks.join(""));
    JSON.parse(output);
    notify("dlgt fetch returned:\n" + output);
  } catch (error) {
    notify("dlgt listener failed for <SESSION_ID> from cursor <CURSOR>: " + error + "\n" + chunks.join("").trim());
  }
}
```

The initial `text(...)` makes the armed listener visible before control
returns. `yield_control()` does not end the cell: execution continues after it,
and the awaited `write_stdin` calls keep the isolate alive until `notify(...)`
injects the result. Never start the listener in an unawaited promise; unawaited
work is discarded when the cell finishes. Keep the entire post-yield
continuation inside the `try`: an uncaught error after `yield_control()` has no
collector, and a listener that dies without calling `notify()` leaves the
parent honoring a wait that can never end.

The delayed tool result is still only a fetch response. Parse it using step 4.
In particular, `timeout`, `blocked`, and `page_full` are wakeups that require
another decision or observation; none of them means completion.

The outer `exec` tool may return `Script running with cell ID N` after the
listener is armed. Retain that decimal cell ID. If parent work runs out before
the notification arrives, do not idle and do not finalize: issue the explicit
top-level `wait` from 3B on that cell ID. It blocks until the listener ends.
Never start a second `fetch` on the same Session while the listener holds it;
a concurrent observer duplicates the window and forks the cursor you must
adopt. The top-level `wait` uses `max_tokens`; nested `exec_command` and
`write_stdin` use `max_output_tokens`.

### 3B. Explicit collection when delayed delivery is unavailable

When the exec environment lacks either `yield_control()` or `notify()`, hold
the process handle with this wrapped cell:

```js
// @exec: {"yield_time_ms": 1000, "max_output_tokens": 30000}
const chunks = [];
let r = await tools.exec_command({
  cmd: "dlgt fetch '<SESSION_ID>' --cursor '<CURSOR>' --wait 30m --max-bytes 24000",
  workdir: "<WORKDIR>", yield_time_ms: 30000, max_output_tokens: 30000
});
for (;;) {
  chunks.push(r.output ?? "");
  if (r.session_id == null) break;
  r = await tools.write_stdin({
    session_id: r.session_id, chars: "",
    yield_time_ms: 300000, max_output_tokens: 30000
  });
}
const output = chunks.join("").trim();
if (r.exit_code !== 0) throw new Error("dlgt fetch exited " + r.exit_code + "\n" + output);
JSON.parse(output); // throws on empty or truncated output instead of publishing it
text(output);
```

The yield values intentionally differ. The `@exec` value governs when the
outer Code Mode cell yields; the nested `exec_command` value governs when it
returns the child-process handle. In 3A the child returns quickly so the
listener can arm before the outer cell yields. In 3B the child gets up to 30
seconds to finish directly while the outer cell publishes its cell ID early.

`--max-bytes 24000` keeps the document comfortably inside the 30,000-token
output limits; `has_more` pagination covers anything beyond it.

Two possible responses to that cell:

- `Script completed` with the JSON already in the output: the fetch finished
  within the first yield. Consume it; there is no cell to wait on.
- `Script running with cell ID N`: make a SEPARATE top-level `wait` tool
  call — never a `tools.wait(...)` inside the JavaScript:

  ```text
  wait {"cell_id":"N","yield_time_ms":1860000,"max_tokens":30000}
  ```

  `N` is the decimal cell ID from that banner. A `chunk_id` or a nested
  `session_id` is never a cell ID; waiting on one fails with "cell not
  found". 1,860,000 ms gives a 30-minute fetch a one-minute margin. If the
  wait yields early, wait again on the SAME cell; never start a replacement
  fetch because a cell yielded.

Choose when to issue that wait by whether you can afford to block:

- **Blocking until the answer is fine** (it is your next step anyway): wait
  immediately.
- **You have other work first**: keep the cell ID, do that work in the SAME
  turn, and issue the same wait when ready — a cell that already finished
  returns its buffered result instantly. In this fallback path completion does
  not interrupt you, so you must eventually spend the wait to learn of it; and
  the cell does not survive the turn. If you cannot guarantee this turn stays
  open, block now — sending your final answer destroys the cell.

### 4. Parse reason first, then carry the cursor

Parse the JSON document already printed by the wrapper. Read `reason` before
indexing `sessions[0].results`: on `timeout` the array is empty and the work is
still running — that is never completion. `result` means the bound execution
terminalized; only `results[].status == "completed"` makes `final_text` a
finished answer. `page_full` corresponds to `has_more: true`: more is already
waiting, so call again immediately with the returned cursor. Every valid
observation exits 0, including `timeout` and `blocked`; a non-zero exit returns
an error document rather than an observation document. From EVERY valid
response, adopt the returned `cursor` for the next call, even when the visible
delta is empty.

### 5. Follow up, then release

```bash
dlgt send '<SESSION_ID>' --request-id refactor-2 -- "Rank the findings by severity."
```

The send receipt prints a fresh cursor; observe with the same wrapped cell.
When the Session is no longer needed: `dlgt stop '<SESSION_ID>'`. Stopping
ends the process, not the provider conversation — the same `session.id`
resumes later via `send --resume`.

### Pacing and final fallback

To pace periodic observations instead of holding one wait, delay with an
awaited Code Mode timer — `await new Promise(r => setTimeout(r, 300000));`
before the `fetch` in the same cell — never with a shell `sleep`, which burns
a unified-exec process and hits the same 30-second clamp.

Where Code Mode cells are unavailable, fall back to short forward polls that
stay UNDER the 30-second nested clamp, repeated:

```bash
dlgt fetch '<SESSION_ID>' --cursor '<CURSOR>' --wait 20s
```

Code Mode internals are version-sensitive; if either wrapper misbehaves after
a Codex upgrade, drop to this fallback.

## Standard workflow when running as Claude Code

```bash
# 1. Submit in the foreground; it returns after a short confirmation window.
dlgt new --title "long refactor" --harness codex --cwd /abs/path \
  --request-id refactor-1 --stdin < prompt.md

# 2. Observe with ONE long fetch, run through the Bash tool's background
#    mechanism (run_in_background), substituting the printed values:
dlgt fetch '<SESSION_ID>' --cursor '<CURSOR>' --wait 30m
```

The background task exits when the worker finishes and the harness notifies
you — zero polling calls. The receipt is already safe in your context, so a
killed or backgrounded read costs nothing. For a short task (a minute or
less), a foreground `--wait 60s` fits the default tool budget and needs no
background step. Never fuse submission into the long fetch: that puts the
receipt and a 30-minute wait in the same killable command, which is exactly
the failure the two-phase split removes. Parse responses exactly as in the
Codex arc: `reason` first, then `results[].status`, and always carry the
returned cursor. If the background fetch returns `reason: "timeout"`, launch
another fetch from that returned cursor; do not resend the prompt.

## Long prompts

A self-contained delegation prompt is usually multi-line. Prefer a prompt
file with `--stdin` (as in both workflows above); it keeps the prompt out of
argv and away from shell quoting entirely. A heredoc also works from a plain
shell:

```bash
dlgt new --title "counterpart review" --harness claude --cwd /abs/path \
  --request-id review-1 --stdin <<'PROMPT'
Review the uncommitted changes in this repository.
Do not edit files and do not delegate again.
Report findings and trade-offs only.
PROMPT
```

Quote the heredoc delimiter as `<<'PROMPT'`, or the shell expands `$` and
backticks inside the prompt first. Never embed a heredoc inside a JavaScript
template literal: the quoted delimiter does not protect against backtick and
`${...}` interpolation at the JavaScript layer — from Code Mode, use a prompt
file.

## What is required, what is optional

| Command | Required | Optional, with its default |
| --- | --- | --- |
| `new` | `--title`, `--request-id`, a Harness (`--harness` or `--profile <PROFILE>`), and the prompt (`--stdin` or `-- <PROMPT>`) | `--model` (provider default), `--effort` (harness default), `--cwd` (current directory), `--alias` (generated from the title), `--no-auto-approve` (default is auto-approved), `--startup-timeout` (60s), env options |
| `send` | the Session address, `--request-id`, and the prompt | `--resume`; launch options are accepted only with it |
| `fetch` | the Session address | `--cursor` (omit = bounded baseline snapshot), `--wait <DURATION>` (omit = return immediately; binds to the active/latest execution and waits for its terminal result; up to 24h), `--screen[=N]`/`--no-screen`, `--max-bytes` (32 KiB) |
| `stop`, `cancel`, `show`, `restart` | the Session address | `cancel --timeout` (30s), `stop --force` |

## Recover a lost response

| Lost | Recovery |
| --- | --- |
| The `new` or `send` response, while the same daemon is alive | Re-run the byte-identical command with the same `--request-id`. It replays the original submitted receipt with `"replayed": true` and never creates a second Session or execution; a formerly `pending` receipt may become `confirmed`. |
| A `fetch` response, or the cursor | Apply the three-way cursor rule below. |
| The Session ID itself | `dlgt list` finds it, but prefer a stable `--alias` on `new` so you never need to search. |

The cursor rule — after any observation attempt, exactly one of three cases
applies:

- **No dlgt JSON received** (killed, empty, truncated): the observation
  delivered nothing and advanced nothing. Retry the SAME cursor; positions
  replay the identical window safely, any number of times.
- **Valid dlgt JSON received**: ALWAYS adopt its returned `cursor`, even when
  the visible delta was empty — internal bookkeeping can advance beneath an
  empty page.
- **The cursor itself is lost, or the daemon answered `CURSOR_EXPIRED` or
  `CURSOR_INVALID`**: run one cursorless `fetch <session.id>` for a bounded
  baseline — current state, the latest retained result, a screen tail, and a
  fresh cursor — and continue from there.

Positions and the request-id replay ledger are memory-only and meaningful only
within one daemon lifetime. Handle the boundaries separately:

- On `RPC_UNAVAILABLE`, run `dlgt list --all-versions` to locate the owning
  versioned daemon. Do not assume the daemon restarted.
- After a confirmed daemon restart, its cursor positions and request-id ledger
  are gone. Do not blindly replay an uncertain `new` or `send`: first inspect
  provider history and possible external effects. Resume a known provider
  conversation with `send --resume` only when a new follow-up is appropriate.
- On `SESSION_NOT_RUNNING`, use `send --resume` for a new follow-up; a
  cursorless baseline cannot recreate a missing live Session.
- Use a cursorless baseline only when the Session still exists and only the
  cursor was lost, expired, or invalid.

## Optimization: one call for a short task

From a PLAIN shell (Claude foreground, or a real terminal), a short task can
fuse the two phases into one command:

```bash
dlgt new --title "quick check" --harness claude --cwd /abs/path \
  --alias @quick --request-id quick-1 -- "..." \
  && dlgt fetch @quick --wait 60s
```

The output is two JSON documents, one per line; line 1 is the submission
receipt and is authoritative. If the second document is missing or truncated,
the work is still running and the receipt still names the Session — recover
by fetching, or if line 1 itself never reached you, by replaying the same
`--request-id`. Note the fetch here reads from a cursorless baseline rather
than the submission cursor — a slightly less exact convenience. Do NOT use
this form from bare Codex Code Mode: the 60-second wait exceeds the
30-second nested clamp; use the standard wrapped arc instead. Never use it
for long tasks on any harness.

## Choose the harness and model

- When the user explicitly asks to use dlgt, a Harness, or a model by name for
  work on a project, treat that request as authorization for the delegated
  worker to read the related source code and other non-secret repository
  context needed for the task. Do not stop merely to ask whether that source
  may be shown to the worker. This authorization is limited to the explicit or
  current project and task; it does not include secrets or credentials,
  unrelated paths, external effects, or edits and commits beyond the user's
  request.
- To cross the provider boundary, select the other harness: from Codex use
  `--harness claude`, from Claude use `--harness codex`. When you are using
  dlgt for the control it gives rather than to change providers, stay on the
  harness you are already running.
- Pass exactly the model the user named. Never silently substitute another
  model; if the named one is unavailable, say so instead.
- If the user named no model, omit `--model` and let the provider default
  apply. Discover valid values with `dlgt models`.
- Pass `--effort` only when the user explicitly requested an effort level.
- If the user, the repository, or a standing instruction file defines an
  explicit routing contract for which counterpart handles which work, follow
  that contract instead of this default.
- Give the worker a self-contained prompt: project path, goal, deliverables,
  checks, edit and commit policy, and the required final response. Tell it
  not to delegate again.
- Run one counterpart unless the user explicitly asks for more. The leader
  inspects the returned `final_text` and the actual filesystem diff, and
  remains responsible for final verification.

## Workers start auto-approved

By default dlgt marks the Session cwd trusted in the Harness's local state and
launches the worker with its approval prompts bypassed. The worker can edit
files and run commands under that cwd without asking. Choose `--cwd`
deliberately, constrain the worker in the prompt, and pass `--no-auto-approve`
when a delegation must keep the Harness's own permission prompts.

## Choose the lifecycle operation

| Situation | Operation |
| --- | --- |
| Start work with no existing provider conversation | `new` with its required first prompt |
| Follow up on a live, idle Session | `send <session.id\|@alias>` |
| Replace the provider process but keep history and conversation | `restart <session.id>` |
| Continue after the owning daemon or Session is gone | `send <session.id> --resume --request-id <ID> -- "<PROMPT>"` |

- Plain `send` never launches, restarts, resumes, or queues. It submits work
  only to a live, idle Session; rejection is side-effect-free.
- `restart` interrupts active work and preserves the alias, retained history,
  and provider conversation.
- `send --resume` relaunches the saved provider conversation and atomically
  submits the follow-up. An already-live matching Session is reused rather
  than duplicated. Claude may return a new canonical `session.id`; retain it.
- Keep one active execution per Session. `SESSION_BUSY` means observe until
  the current execution terminalizes. dlgt never queues prompts.

## Rules

- `--cwd` accepts absolute and relative paths, resolved client-side; omitting
  it uses the client's current directory.
- `fetch --wait` requires a duration and accepts up to 24h. A timeout
  never cancels the work; it only ends that observation.
- Use provider lifecycle state and the retained result, not PTY silence, as
  completion proof. `reason: "timeout"` is not completion.
- Never pass `--pretty`. It only costs tokens; the compact document is the
  contract.
- Never run `dlgt server stop` as cleanup. Session state, results, events,
  and screen history live in that daemon's memory and die with it, breaking
  every cursor and live Session on the machine. `dlgt stop <session.id>`
  releases one worker; the daemon exits on its own once idle and empty.
- Use `fetch` for observation. When its live screen is not enough — the
  interesting output already scrolled off, or `fetch` reports a retention gap
  — read paginated rendered history with `scrollback` (`--lines`, `--before`).
  It needs no terminal and no capability flag. Raw PTY bytes require the
  explicit diagnostic command `logs --raw`.
- If a Session stays `starting` or `busy` without the expected lifecycle
  event, run `fetch <session.id>` and look at `screen.live`; read `scrollback`
  as well when the prompt refers to output above the live screen. A first-run,
  authentication, trust, or permission-mode prompt needs a human (`attach`
  from a real terminal), then retry the delegated work.
- `attach` is an interactive full-screen takeover: it needs a TTY on both ends
  and holds an exclusive lease, so it is not available from a tool call. Read
  the screen with `fetch` and `scrollback` instead. For a human at a terminal:
  detach with `Ctrl-b d`; `--steal` only takes over a known stale attach
  client.
- Treat results, rendered scrollback, and raw output as potentially
  sensitive.
- When installation, runtime, provider, or Skill wiring looks wrong, run
  `dlgt doctor`. Its default checks are offline and read-only. Use
  `dlgt doctor --probe` only when starting a short Codex app-server probe and
  checking the published release are appropriate; it never starts a model
  turn, though the versioned daemon may remain until its normal idle timeout.
- If a successful response contains `info.code: UPDATE_AVAILABLE`, tell the
  user the current and latest versions and ask whether to run `dlgt update`.
  Do not replace the binary and its embedded Skills without explicit
  confirmation.

## Recover from structured errors

| Error | Required action |
| --- | --- |
| `SESSION_BUSY` | Do not resend. `fetch <session.id> --wait <duration>`, or explicitly `cancel`. |
| `SESSION_BLOCKED` | `fetch` reports this as `reason: "blocked"` with the live screen. Read the question; answering needs a human on a real terminal (`attach`), then `fetch` again. |
| `SESSION_NOT_RUNNING` | `send <session.id> --resume --request-id <ID> -- "<PROMPT>"`. |
| `SESSION_ATTACHED` / `ALREADY_ATTACHED` | Coordinate with the active controller. `--steal` only for a known stale attach client. |
| `ATTACH_REQUIRES_TTY` | You have no terminal. Read `screen.live` from `fetch`, and `scrollback` for the history above it. |
| `CURSOR_EXPIRED` / `CURSOR_INVALID` | Third case of the cursor rule: one cursorless `fetch` to rebase. |
| `CANCEL_TIMEOUT` | Cancellation continues. Keep fetching until a terminal result appears. |
| `ALIAS_IN_USE` | The exact alias belongs to a non-terminal Session. Choose another alias or address by ID. |
| `LAUNCH_FAILED` | The provider process could not be launched. Read `fetch` and `show`, and only then the sensitive `logs --raw`. This differs from a process that launched and later failed: that execution failure appears in `results[].status`. |

## Observation and control reference

```bash
dlgt fetch '<SESSION_ID>' --wait 30m
dlgt fetch '<SESSION_ID>' --cursor '<CURSOR>'
dlgt fetch '<SESSION_ID>' --no-screen
dlgt fetch '<SESSION_ID>'
dlgt scrollback '<SESSION_ID>' --lines 200
dlgt cancel '<SESSION_ID>'
dlgt restart '<SESSION_ID>'
dlgt show '<SESSION_ID>'
dlgt list --all-versions
dlgt doctor
```

`fetch` reports one Session's lifecycle events, results, and screen. A response
is bounded to about 32 KiB by default; `has_more: true` means more is already
waiting — call again with the returned cursor and it returns immediately.

`dlgt help <command>` is authoritative for the installed binary. Use it
whenever a flag is unclear, and use discovery (`dlgt harnesses`,
`dlgt models`, `dlgt profiles list`) rather than guessing. Fetch the hosted
reference only when help does not answer:
`curl -fsSL https://combinatrix.ai/dlgt/cli.md` — and when it conflicts with
the installed binary, the binary's help wins.

## Clean up provider history

A dlgt Session is also a real provider conversation (titled `[dlgt] ...` in
Codex or Claude history). `dlgt stop` releases the process but deliberately
preserves that conversation for `send --resume`. Treat provider-history
cleanup as a separate, explicit user action — never archive automatically
when ordinary work finishes. For a Codex Session: `codex archive
<thread-id>` (the `session.id` without its `codex:` prefix). For a Claude
Session there is no public archive command; archiving means operating Claude
Desktop's own UI, identifying the conversation unambiguously first — if it
cannot be identified, or UI control is unavailable, report that instead of
guessing, and never edit provider storage or transcript files directly.
