---
name: dlgt
description: Create, address, observe, and control live Codex and Claude Sessions through one local runtime.
---

# dlgt

Use `dlgt` when a Codex or Claude subagent should remain alive in an owned PTY
and be addressable from later commands. The only public runtime object is a
Session. Retain both `session.id` and `provider_session_id` from `new`:
`session.id` addresses the live dlgt runtime while that version's daemon runs,
and `provider_session_id` identifies the underlying Codex or Claude
conversation for provider-native lookup or resume after dlgt exits. Aliases are
human conveniences and may be reused after a Session stops.

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

Tell every delegated worker not to delegate again. Give it a self-contained
prompt with the project path, goal, deliverables, checks, edit/commit policy,
and required final response. The leader inspects the actual result or shared
filesystem diff and remains responsible for final verification. Do not run
both counterpart reviewers unless the user explicitly requests both.

## Rules

- Create only with `new`; `send` never creates or selects another Session.
- Restart any stable Session with `restart`. Active work becomes interrupted;
  retain the immutable Session ID because terminal aliases may have been reused.
- Keep one active execution per Session. `SESSION_BUSY` means retry after the
  current execution terminalizes; dlgt never queues prompts.
- Always give `wait` an explicit `--timeout`. A timeout does not cancel work.
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

## Common commands

```bash
created=$(dlgt new --title "Fable review" --alias @fable-review \
  --harness claude --model claude-fable-5 --cwd . \
  -- "Review only; do not edit or delegate again. Report findings and trade-offs.")
# Parse and retain .session.id from the JSON response. Use that immutable ID
# for later commands rather than relying on the alias.

dlgt send ses_7K3M9Q2X --wait --timeout 15m -- "Address the findings"
dlgt wait ses_7K3M9Q2X --timeout 15m
dlgt cancel ses_7K3M9Q2X
dlgt restart ses_7K3M9Q2X
dlgt show ses_7K3M9Q2X
dlgt events ses_7K3M9Q2X --follow
dlgt scrollback ses_7K3M9Q2X --lines 100
dlgt attach ses_7K3M9Q2X
dlgt stop ses_7K3M9Q2X
dlgt list --all-versions
dlgt update
```

Control-plane commands return compact JSON with `ok:true` or a structured
`ok:false` error. `events --follow` is NDJSON; `attach` and `logs --raw` are raw
streams. `rpc --stdio` exposes only the public Session-based v1 methods.
