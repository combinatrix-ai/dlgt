---
name: dlgt
description: Create, address, observe, and control live Codex and Claude Sessions through one local runtime.
---

# dlgt

Use `dlgt` when a Codex or Claude subagent should remain alive in an owned PTY
and be addressable from later commands. The only public runtime object is a
Session. Retain `session.id`, `session.resume_ref`, and
`session.provider_session_id` from `new`. Use the immutable `session.id` while
its daemon is live. Use the provider-qualified `resume_ref` (`codex:<id>` or
`claude:<id>`) for provider-native lookup or explicit resume after dlgt exits.
Aliases are human conveniences and may be reused after a Session stops.

## Exact delegation routes

Use only the row matching the current leader. Pass the model exactly; never
silently substitute it. Pass `--effort` only when the user explicitly
requested an effort level; otherwise omit it so the harness default applies.

| Current leader | Requested work | Harness | Model |
| --- | --- | --- | --- |
| Codex Sol | implementation by Luna | `codex` | `gpt-5.6-luna` |
| Codex Sol | review by Fable | `claude` | `claude-fable-5` |
| Claude Fable | code-heavy implementation by Luna | `codex` | `gpt-5.6-luna` |
| Claude Fable | review by Sol | `codex` | `gpt-5.6-sol` |

If no row matches, follow an explicit user or standing routing contract. If
none exists, do not invent a model substitution or treat this table as a
default route.

Tell every delegated worker not to delegate again. Give it a self-contained
prompt with the project path, goal, deliverables, checks, edit/commit policy,
and required final response. The leader inspects the actual result or shared
filesystem diff and remains responsible for final verification. Do not run
both counterpart reviewers unless the user explicitly requests both.

## Command reference

Use `dlgt help <command>` for syntax matching the installed binary. For the
complete CLI contract, fetch the latest released reference:

```bash
curl -fsSL https://combinatrix.ai/dlgt/cli.md
```

The hosted reference may describe a newer release. When it conflicts with the
installed binary, follow that binary's help and tell the user an update may be
available.

Discover launch choices instead of guessing them:

```bash
dlgt harnesses
dlgt models --harness codex
dlgt models --harness claude
dlgt profiles list
```

## Choose the lifecycle operation

| Situation | Operation |
| --- | --- |
| Start new work with no provider conversation | `new` with the required first prompt |
| Send a follow-up to a live, idle Session | plain `send <ses_*|@alias>` |
| Replace a Session's provider process but preserve its `ses_*` ID and history | `restart <ses_*>` |
| Continue after the owning daemon or Session is gone | `send <resume_ref> --resume` |

`restart` interrupts active work. Plain `send` never launches, restarts,
resumes, or queues a Session. `send --resume` may create a new `ses_*` Session
and atomically submit the follow-up; retain the new identifiers it returns.

## Rules

- `new` requires its initial prompt and atomically starts the Harness and
  accepts that prompt.
- Plain `send` submits work only to a live, idle Session. Rejection is
  side-effect-free; accepted work changes the Session to busy.
- Resume a provider conversation with `dlgt send codex:<thread-id> --resume --
  "prompt"` or the equivalent `claude:<session-id>` selector. A successful
  resume returns a new `ses_*` ID and canonical `resume_ref`; an already-live
  matching Session is reused instead of duplicated.
- Keep one active execution per Session. `SESSION_BUSY` means retry after the
  current execution terminalizes; dlgt never queues prompts.
- Always give `new --wait`, `send --wait`, and `wait` an explicit `--timeout`.
  A timeout does not cancel work.
- Use `--stdin` for long or sensitive prompts so they do not appear in argv.
- Use provider lifecycle state and `wait`, not PTY silence, as completion proof.
- Use `scrollback` for bounded plain-text observation. Raw PTY bytes require the
  explicit diagnostic command `logs --raw`.
- If a Session remains `starting` or `busy` without the expected lifecycle
  event, inspect `events` and `scrollback`, then use `attach` when the screen
  shows a first-run, authentication, trust, theme, or permission-mode prompt.
  Complete the prompt, detach, and retry the delegated work in a fresh Session.
- `attach` is exclusive. Detach with `Ctrl-b d`; use `--steal` only when taking
  control from a known stale attach client.
- Treat results, rendered scrollback, and raw output as potentially sensitive.
- If a successful response contains `info.code: UPDATE_AVAILABLE`, tell the
  user the current and latest versions and ask whether to run `dlgt update`.
  Do not update dlgt or replace its binary and embedded Skills without explicit
  confirmation. If the user already explicitly requested the update, do not
  ask again.
- dlgt marks the Session cwd trusted in the Harness's local state and starts
  workers auto-approved. Workers can edit files and run commands in the cwd,
  so constrain them in the prompt, and pass `--no-auto-approve` when a
  delegation must keep the Harness's own permission prompts.

## Reliable delegation workflow

```bash
created=$(dlgt new --title "Fable review" --alias @fable-review \
  --harness claude --model claude-fable-5 --cwd . \
  --wait --timeout 15m \
  -- "Review only; do not edit or delegate again. Report findings and trade-offs.")

session_id=$(printf '%s\n' "$created" | jq -er '.session.id')
resume_ref=$(printf '%s\n' "$created" | jq -er '.session.resume_ref')
provider_session_id=$(printf '%s\n' "$created" | jq -er '.session.provider_session_id')

dlgt send "$session_id" --wait --timeout 15m -- "Address the findings"
dlgt show "$session_id"
dlgt stop "$session_id"
```

Use a real JSON parser rather than regex. If `jq` is unavailable, parse the
same three fields with another structured JSON tool. Do not rely on an Alias
after a Session stops.

## Recover from structured errors

| Error | Required action |
| --- | --- |
| `SESSION_BUSY` | Do not resend. Run `wait <ses_*> --timeout <duration>`, or explicitly `cancel` the active work. |
| `SESSION_BLOCKED` | Inspect `events` and `scrollback`, `attach`, answer the visible prompt, detach with `Ctrl-b d`, then `wait` again. |
| `SESSION_ATTACHED` / `ALREADY_ATTACHED` | Coordinate with the active controller. Use `--steal` only for a known stale attach client. |
| `SESSION_NOT_RUNNING` | Use the error's `resume_ref` or the saved one with `send <resume_ref> --resume -- <prompt>`. |
| `WAIT_TIMEOUT` | Work continues. Wait again, inspect it, or cancel explicitly. Do not report completion. |
| `CANCEL_TIMEOUT` | Cancellation continues. Inspect `events` and wait for a terminal result. |
| `LAUNCH_FAILED` / `PROVIDER_FAILED` | Inspect `events`, `scrollback`, `show`, and only then sensitive `logs --raw`. |

Useful observation and control commands:

```bash
dlgt wait "$session_id" --timeout 15m
dlgt cancel "$session_id"
dlgt restart "$session_id"
dlgt events "$session_id"
dlgt scrollback "$session_id" --lines 100
dlgt attach "$session_id"
dlgt list --all-versions
# Run `dlgt update` only after explicit user approval.
```

Control-plane commands return compact JSON with `ok:true` or a structured
`ok:false` error. `events --follow` is NDJSON; `attach` and `logs --raw` are raw
streams. `rpc --stdio` exposes only the public Session-based v1 methods.
