#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repo_root/target/debug/dlgt"
state_dir=$(mktemp -d "${TMPDIR:-/tmp}/dlgt-smoke.XXXXXX")
server_pid=
old_server_pid=

cleanup() {
  if [ -n "$server_pid" ]; then
    DLGT_HOME="$state_dir" "$binary" server stop >/dev/null 2>&1 || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [ -n "$old_server_pid" ]; then
    DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null 2>&1 || true
    wait "$old_server_pid" 2>/dev/null || true
  fi
  rm -rf "$state_dir"
}
trap cleanup EXIT INT TERM

export DLGT_HOME="$state_dir"
export HOME="$state_dir/home"
export DLGT_CLAUDE_BIN="$repo_root/tests/fixtures/fake-claude.sh"
export DLGT_CODEX_BIN="$repo_root/tests/fixtures/fake-codex.sh"
export DLGT_FAKE_ARGS_FILE="$state_dir/fake-args.log"
export DLGT_FAKE_CODEX_LOG="$state_dir/fake-codex.log"
export DLGT_FAKE_ROTATE_FILE="$state_dir/rotate"
unset ANTHROPIC_API_KEY DLGT_FAKE_NO_INIT 2>/dev/null || true
mkdir -p "$HOME"
touch "$DLGT_FAKE_ARGS_FILE" "$DLGT_FAKE_CODEX_LOG"
old_socket="$state_dir/run/old/dlgt.sock"

fail() {
  printf 'smoke: %s\n' "$*" >&2
  exit 1
}
# expect <text> <pattern>: the text contains a line matching the pattern.
expect() {
  printf '%s\n' "$1" | grep -q -- "$2" || fail "expected $2 in: $1"
}
refute() {
  if printf '%s\n' "$1" | grep -q -- "$2"; then fail "unexpected $2 in: $1"; fi
}
# Last value of a string field in a compact JSON line.
field() {
  sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p"
}
# rejected <status> <command...>: the command exits with the status and its
# output is left in $output.
rejected() {
  expected_status=$1
  shift
  set +e
  output=$("$@")
  actual_status=$?
  set -e
  test "$actual_status" -eq "$expected_status" \
    || fail "exit $actual_status instead of $expected_status from $*: $output"
}
wait_for_socket() {
  attempt=0
  while [ ! -S "$1" ]; do
    attempt=$((attempt + 1)); test "$attempt" -lt 200 || fail "no socket at $1"; sleep 0.02
  done
}
wait_until_gone() {
  attempt=0
  while "$binary" show "$1" >/dev/null 2>&1; do
    attempt=$((attempt + 1)); test "$attempt" -lt 200 || fail "$1 never stopped"; sleep 0.02
  done
}

version=$("$binary" --version | field version)
test -n "$version" || exit 1

"$binary" server --foreground >"$state_dir/server.log" 2>&1 &
server_pid=$!
wait_for_socket "$state_dir/run/$version/dlgt.sock"

# `new` launches the provider, binds its Session ID, and accepts the prompt.
printf '%s' 'smoke-initial' >"$state_dir/initial-prompt.md"
new_json=$("$binary" new --title smoke --alias @smoke --harness claude --cwd "$repo_root" \
  --request-id smoke-1 --harness-option chrome \
  --prompt-file "$state_dir/initial-prompt.md")
session_id=$(printf '%s\n' "$new_json" | field id)
case "$session_id" in claude:*) ;; *) fail "unexpected Session ID in $new_json" ;; esac
expect "$new_json" '"alias":"@smoke"'
expect "$new_json" '"submission":"confirmed"'
expect "$new_json" '"cursor":"[0-9]*"'
expect "$new_json" '"approvals":{"effective":"auto","requested":"auto"}'
refute "$new_json" '"provider_session_id"\|"resume_ref"\|"internal:\|auto_approve'
# One stream-json process that asks over stdio, in the provider's auto mode.
for argument in -p stream-json --replay-user-messages --permission-prompt-tool stdio \
  --chrome --permission-mode auto --session-id; do
  grep -q -- "^$argument\$" "$DLGT_FAKE_ARGS_FILE" || fail "claude was not given $argument"
done
if grep -q -- '^--dangerously-skip-permissions$\|^--settings$' "$DLGT_FAKE_ARGS_FILE"; then
  fail "claude was given a removed or dangerous argument"
fi
initial_json=$("$binary" fetch "$session_id" --wait 5s)
expect "$initial_json" '"reason":"result"'
expect "$initial_json" '"final_text":"fake:smoke-initial"'
expect "$initial_json" '"denied_requests":0'
show_json=$("$binary" show "$session_id")
expect "$show_json" '"marker":"dlgt:[0-9a-f]\{12\}"'
expect "$show_json" "\"resume_command\":\"claude --resume ${session_id#claude:}\""
expect "$show_json" '"auth":"subscription"'
expect "$show_json" '"resolved_model":"fake-model"'
expect "$show_json" '"requests":\[\]'

# A cursorless fetch is the documented baseline recovery path.
baseline_json=$("$binary" fetch "$session_id")
expect "$baseline_json" '"reason":"snapshot"'
expect "$baseline_json" '"schema_version":2'
expect "$baseline_json" '"final_text":"fake:smoke-initial"'
expect "$baseline_json" '"final_text_source":"provider"'
expect "$baseline_json" '"kind":"assistant"'
expect "$baseline_json" '"requests":\[\]'
refute "$baseline_json" '"screen"'
baseline_cursor=$(printf '%s\n' "$baseline_json" | field cursor)
test "$baseline_cursor" -gt 0

# An exhausted cursor is a successful empty observation, not an error, and it
# keeps the caller's own position: nothing advanced, so there is nothing new to
# name and no retained position is spent.
empty_json=$("$binary" fetch "$session_id" --cursor "$baseline_cursor")
expect "$empty_json" '"reason":"timeout"'
expect "$empty_json" '"results":\[\]'
expect "$empty_json" '"messages":\[\]'
expect "$empty_json" '"has_more":false'
expect "$empty_json" "\"cursor\":\"$baseline_cursor\""
# A leading-zero spelling names the same position but is canonicalized before
# publication, so the no-mint echo cannot smuggle an arbitrarily long
# caller-chosen spelling past the measured byte bound.
expect "$("$binary" fetch "$session_id" --cursor "000000000$baseline_cursor")" \
  "\"cursor\":\"$baseline_cursor\""

# A current client routes provider-qualified selectors to a live daemon on a
# different versioned socket instead of launching a duplicate locally.
DLGT_SOCKET="$old_socket" "$binary" server --foreground >"$state_dir/old-server.log" 2>&1 &
old_server_pid=$!
wait_for_socket "$old_socket"
cross_version_id=$(DLGT_SOCKET="$old_socket" "$binary" new --title cross-version \
  --alias @cross-version --harness claude --cwd "$repo_root" --request-id cross-1 \
  -- cross-version-initial | field id)
DLGT_SOCKET="$old_socket" "$binary" fetch "$cross_version_id" --wait 5s >/dev/null
expect "$("$binary" send "$cross_version_id" --request-id cross-2 -- cross-version-follow-up)" \
  "\"id\":\"$cross_version_id\""
expect "$("$binary" fetch "$cross_version_id" --wait 5s)" \
  '"final_text":"fake:cross-version-follow-up"'
stale_cursor=$(DLGT_SOCKET="$old_socket" "$binary" fetch "$cross_version_id" | field cursor)
test -n "$stale_cursor"
DLGT_SOCKET="$old_socket" "$binary" stop "$cross_version_id" --force >/dev/null
DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null
wait "$old_server_pid"
old_server_pid=

# Restarting drops every Session as well as its cursor table. Selector lookup
# happens before cursor lookup, so the old Session must return NOT_FOUND even
# with a formerly valid cursor. Numeric reuse within a new cursor table is
# covered by cursor::tests::a_position_from_another_table_resolves_against_this_one.
DLGT_SOCKET="$old_socket" "$binary" server --foreground >"$state_dir/old-server-2.log" 2>&1 &
old_server_pid=$!
wait_for_socket "$old_socket"
rejected 1 env DLGT_SOCKET="$old_socket" "$binary" fetch "$cross_version_id" --cursor "$stale_cursor"
expect "$output" '"code":"NOT_FOUND"'
rejected 1 env DLGT_SOCKET="$old_socket" "$binary" fetch "$cross_version_id"
expect "$output" '"code":"NOT_FOUND"'
expect "$(DLGT_SOCKET="$old_socket" "$binary" list --all)" '"sessions":\[\]'
DLGT_SOCKET="$old_socket" "$binary" server stop >/dev/null
wait "$old_server_pid"
old_server_pid=

# Every acceptance must carry an idempotency key.
rejected 1 "$binary" send "$session_id" -- no-key
expect "$output" '"code":"INVALID_ARGUMENT"'
expect "$output" '--request-id'

# A provider that cannot start is a bounded launch failure that carries the
# provider's own stderr and stays addressable by its launch ID.
rejected 1 "$binary" new --title launch-failure --alias @launch-failure --harness claude \
  --cwd "$repo_root" --request-id launch-failure-1 --env DLGT_FAKE_NO_INIT=1 -- launch-failure
expect "$output" '"code":"LAUNCH_FAILED"'
expect "$output" '"launch_id":"internal:[0-9A-Z]\{8\}"'
expect "$output" '"detail":"Not logged in'
expect "$output" '"action":"claude"'
refute "$output" 'LAUNCH_BLOCKED\|"screen"'
launch_failure_id=$(printf '%s\n' "$output" | field launch_id)
expect "$("$binary" show "$launch_failure_id")" '"launch_failure":{'

# A busy Session rejects a second prompt without side effects, and a running
# execution must not hide the answer to the previous one.
send_json=$("$binary" send "$session_id" --request-id smoke-2 -- hold-busy)
expect "$send_json" '"execution_seq":2'
busy_baseline=$("$binary" fetch "$session_id" --no-messages)
expect "$busy_baseline" '"state":"busy"'
expect "$busy_baseline" '"quiet_for_ms":[0-9]'
expect "$busy_baseline" '"final_text":"fake:smoke-initial"'
refute "$busy_baseline" '"messages"'
expect "$("$binary" show "$session_id")" '"final_text":"fake:smoke-initial"'
rejected 5 "$binary" send "$session_id" --request-id smoke-busy -- second
expect "$output" '"code":"SESSION_BUSY"'

# A steer joins the running execution: it has no result of its own, and the
# execution still ends with exactly one.
steer_json=$("$binary" send "$session_id" --steer --request-id steer-1 -- steered-finish)
expect "$steer_json" '"steer":"accepted"'
expect "$steer_json" '"execution_seq":2'
steer_cursor=$(printf '%s\n' "$steer_json" | field cursor)
steered_json=$("$binary" fetch "$session_id" --cursor "$steer_cursor" --wait 5s)
expect "$steered_json" '"final_text":"fake:steered-finish"'
expect "$steered_json" '"delivered":false,"execution_seq":2,"kind":"steer"'
expect "$steered_json" '"delivered":true,"execution_seq":2,"kind":"steer"'
refute "$steered_json" '"execution_seq":3'
refute "$steered_json" 'turn_'
expect "$("$binary" send "$session_id" --steer --request-id steer-1 -- steered-finish)" \
  '"replayed":true'
# An idle Session has nothing to steer.
rejected 1 "$binary" send "$session_id" --steer --request-id steer-idle -- nothing-runs
expect "$output" '"code":"NO_ACTIVE_EXECUTION"'

# Public lifecycle events arrive through fetch deltas. The acceptance cursor
# precedes the first execution; pre-bind internal launch IDs must not leak.
initial_cursor=$(printf '%s\n' "$new_json" | field cursor)
events_json=$("$binary" fetch "$session_id" --cursor "$initial_cursor" --no-messages)
expect "$events_json" '"type":"session.busy"'
expect "$events_json" '"type":"session.idle"'
refute "$events_json" 'internal:'

# The response envelope echoes the request id, so the id is bounded.
long_rpc_id=$(awk 'BEGIN { for (i = 0; i < 300; i++) printf "i" }')
printf '{"id":"%s","method":"session.list","params":{}}\n' "$long_rpc_id" \
  | "$binary" rpc --stdio >"$state_dir/rpc-long-id.json"
grep -q '"code":"INVALID_ARGUMENT"' "$state_dir/rpc-long-id.json"
test "$(wc -c <"$state_dir/rpc-long-id.json" | tr -d ' ')" -lt 300

# A fetch through the stdio proxy still respects the result-document bound;
# only the JSONL envelope and its short id sit outside it.
printf '{"id":"r1","method":"session.fetch","params":{"session":"%s","max_bytes":2048,"messages":false}}\n' \
  "$session_id" | "$binary" rpc --stdio >"$state_dir/rpc-fetch.json"
grep -q '"cursor":"[0-9]*"' "$state_dir/rpc-fetch.json"
rpc_bytes=$(wc -c <"$state_dir/rpc-fetch.json" | tr -d ' ')
test "$rpc_bytes" -le 2148 || fail "rpc fetch line was $rpc_bytes bytes, over 2048 plus its envelope"

# A position is a number, so raw RPC accepts it spelled either way. Absent and
# null mean "baseline"; anything that is not a position is a malformed request
# rather than a silent baseline.
rpc_fetch() {
  printf '{"id":"c1","method":"session.fetch","params":{"session":"%s","messages":false,%s}}\n' \
    "$session_id" "$1" | "$binary" rpc --stdio
}
expect "$(rpc_fetch '"cursor":"1"')" '"cursor"'
expect "$(rpc_fetch '"cursor":1')" '"cursor"'
expect "$(rpc_fetch '"cursor":null')" '"reason":"snapshot"'
expect "$(rpc_fetch '"max_bytes":32768')" '"reason":"snapshot"'
for shape in 'true' '{}' '[]' '"not-a-number"'; do
  expect "$(rpc_fetch "\"cursor\":$shape")" '"code":"CURSOR_INVALID"'
done

# Acceptance over raw RPC needs the same idempotency key the CLI requires.
rpc_accept() {
  printf '{"id":"a1","method":"session.send","params":{"session":"%s","prompt":"x"%s}}\n' \
    "$session_id" "$1" | "$binary" rpc --stdio
}
expect "$(rpc_accept '')" '"code":"INVALID_ARGUMENT"'
expect "$(rpc_accept ',"request_id":""')" '"code":"INVALID_ARGUMENT"'
long_key=$(awk 'BEGIN { for (i = 0; i < 129; i++) printf "k" }')
expect "$(rpc_accept ",\"request_id\":\"$long_key\"")" '"code":"INVALID_ARGUMENT"'
expect "$(printf '{"id":"a2","method":"session.create","params":{"title":"t","harness":"claude","prompt":"x","cwd":"%s","environment":{}}}\n' \
  "$repo_root" | "$binary" rpc --stdio)" '"code":"INVALID_ARGUMENT"'
# The terminal-era methods are not part of the public RPC surface.
for method in scrollback.read transcript.read_raw harness.list session.input hook.event; do
  expect "$(printf '{"id":"m1","method":"%s","params":{}}\n' "$method" | "$binary" rpc --stdio)" \
    '"code":"INVALID_ARGUMENT"'
done

expect "$("$binary" models --harness claude)" '"id":"default"'
expect "$("$binary" models --harness codex)" '"id":"fake-codex"'
expect "$("$binary" profiles)" '"profiles"'
"$binary" skill | grep -q '^name: dlgt$'
for removed in attach scrollback logs harnesses; do
  rejected 1 "$binary" "$removed" "$session_id"
  expect "$output" 'unknown command'
done

# In `ask` mode every approval becomes a Request that only the caller answers.
ask_json=$("$binary" new --title asker --alias @asker --harness claude --cwd "$repo_root" \
  --approvals ask --request-id ask-1 -- ask-first)
ask_id=$(printf '%s\n' "$ask_json" | field id)
ask_cursor=$(printf '%s\n' "$ask_json" | field cursor)
grep -q -- '^manual$' "$DLGT_FAKE_ARGS_FILE" || fail "ask mode did not use the manual permission mode"
blocked_json=$("$binary" fetch "$ask_id" --cursor "$ask_cursor" --wait 5s)
expect "$blocked_json" '"reason":"blocked"'
expect "$blocked_json" '"state":"blocked"'
expect "$blocked_json" '"type":"session.blocked"'
expect "$blocked_json" '"id":"r1","input":{"command":"touch probe.txt"},"kind":"approval"'
expect "$blocked_json" '"summary":"touch probe.txt","tool":"Bash"'
expect "$("$binary" show "$ask_id")" '"id":"r1"'
# While blocked, neither a prompt nor a steer is accepted, and the error names
# the Request and the command that answers it.
rejected 4 "$binary" send "$ask_id" --request-id ask-blocked -- more
expect "$output" '"code":"SESSION_BLOCKED"'
expect "$output" '"request_ids":\["r1"\]'
expect "$output" "\"action\":\"dlgt answer $ask_id r1 --allow|--deny\""
rejected 4 "$binary" send "$ask_id" --steer --request-id ask-steer -- more
expect "$output" '"code":"SESSION_BLOCKED"'
rejected 1 "$binary" answer "$ask_id" r1 --text nope
expect "$output" '"code":"INVALID_ARGUMENT"'
rejected 1 "$binary" answer "$ask_id" r9 --allow
expect "$output" '"code":"REQUEST_NOT_FOUND"'
expect "$("$binary" fetch "$ask_id" --no-messages)" '"state":"blocked"'
answer_json=$("$binary" answer "$ask_id" r1 --allow)
expect "$answer_json" '"request":{"id":"r1","resolution":"allowed"}'
expect "$answer_json" '"cursor":"[0-9]*"'
# Answering is idempotent by Request, and a different answer is rejected.
expect "$("$binary" answer "$ask_id" r1 --allow)" '"replayed":true'
rejected 1 "$binary" answer "$ask_id" r1 --deny
expect "$output" '"code":"REQUEST_RESOLVED"'
allowed_json=$("$binary" fetch "$ask_id" --cursor "$ask_cursor" --wait 5s)
expect "$allowed_json" '"final_text":"allowed"'
expect "$allowed_json" '"by":"caller","decision":"approved"'
expect "$allowed_json" '"request_id":"r1"'
expect "$allowed_json" '"type":"session.resumed"'
expect "$allowed_json" '"denied_requests":0'

# A denial reaches the worker with its reason, and a completed result still
# reports that something was denied.
deny_cursor=$("$binary" send "$ask_id" --request-id ask-2 -- ask-second | field cursor)
expect "$("$binary" fetch "$ask_id" --cursor "$deny_cursor" --wait 5s)" '"id":"r2"'
expect "$("$binary" answer "$ask_id" r2 --deny --message use-a-branch)" '"resolution":"denied"'
denied_json=$("$binary" fetch "$ask_id" --cursor "$deny_cursor" --wait 5s)
expect "$denied_json" '"final_text":"denied:use-a-branch"'
expect "$denied_json" '"status":"completed"'
expect "$denied_json" '"denied_requests":1'
expect "$denied_json" '"by":"caller","decision":"denied"'

# A question is answered with text or one of its choices.
question_cursor=$("$binary" send "$ask_id" --request-id ask-3 -- question-color | field cursor)
question_json=$("$binary" fetch "$ask_id" --cursor "$question_cursor" --wait 5s)
expect "$question_json" '"choices":\["Red","Blue"\]'
expect "$question_json" '"kind":"question","text":"Which color?"'
rejected 1 "$binary" answer "$ask_id" r3 --allow
expect "$output" '"code":"INVALID_ARGUMENT"'
expect "$("$binary" answer "$ask_id" r3 --choice 2)" '"resolution":"answered"'
expect "$("$binary" fetch "$ask_id" --cursor "$question_cursor" --wait 5s)" \
  '"final_text":"answered:Blue"'

# Cancel interrupts the execution, not the Session. A steer the provider had
# not read is discarded with it, and a pending Request is closed.
cancel_cursor=$("$binary" send "$ask_id" --request-id cancel-1 -- hold-cancel | field cursor)
"$binary" send "$ask_id" --steer --request-id cancel-steer -- queued-steer >/dev/null
cancel_json=$("$binary" cancel "$ask_id")
expect "$cancel_json" '"canceled":true'
expect "$cancel_json" '"status":"canceled"'
expect "$cancel_json" '"state":"idle"'
canceled_json=$("$binary" fetch "$ask_id" --cursor "$cancel_cursor")
expect "$canceled_json" '"type":"session.canceling"'
expect "$canceled_json" '"expired":true'
refute "$canceled_json" 'fake:queued-steer'
idle_cancel=$("$binary" cancel "$ask_id")
expect "$idle_cancel" '"canceled":false'
expect "$idle_cancel" '"reason":"NO_ACTIVE_WORK"'
"$binary" send "$ask_id" --request-id cancel-2 -- ask-then-cancel >/dev/null
expect "$("$binary" fetch "$ask_id" --wait 5s)" '"reason":"blocked"'
expect "$("$binary" cancel "$ask_id")" '"status":"canceled"'
expect "$("$binary" show "$ask_id")" '"requests":\[\]'
rejected 1 "$binary" answer "$ask_id" r4 --allow
expect "$output" '"code":"REQUEST_RESOLVED"'
# The Session is usable again, and the discarded steer never ran.
after_cancel=$("$binary" send "$ask_id" --request-id cancel-3 -- after-cancel | field cursor)
expect "$("$binary" fetch "$ask_id" --cursor "$after_cancel" --wait 5s)" \
  '"final_text":"fake:after-cancel"'

# `watch` is the read-only human view of the same messages.
watch_text=$("$binary" watch "$ask_id" --from-start --until-idle)
expect "$watch_text" '^\[exec 1\] allow  r1  by caller$'
expect "$watch_text" '^\[exec 2\] deny   r2  by caller$'
expect "$watch_text" '^\[exec 1\] claude allowed$'
expect "$watch_text" '^\[exec 1\] done   completed$'
expect "$watch_text" '^\[exec 4\] done   canceled$'
"$binary" stop "$ask_id" --force >/dev/null
wait_until_gone @asker

# A provider that does not honor the requested mode is reported, not hidden,
# and so is a login that bills an API key.
fallback_json=$("$binary" new --title fallback --harness claude --model no-auto-model \
  --cwd "$repo_root" --env ANTHROPIC_API_KEY=fake-key --request-id fallback-1 -- fallback-initial)
fallback_id=$(printf '%s\n' "$fallback_json" | field id)
expect "$fallback_json" '"approvals":{"effective":"ask","reason":"provider reported permission mode default","requested":"auto"}'
fallback_fetch=$("$binary" fetch "$fallback_id" --wait 5s)
expect "$fallback_fetch" '"auth":"api_key"'
expect "$fallback_fetch" '"kind":"notice"'
refute "$fallback_fetch" 'fake-key'
"$binary" stop "$fallback_id" --force >/dev/null

# Restart interrupts active work while preserving identity, provider binding, and history.
"$binary" send "$session_id" --request-id smoke-3 -- hold-until-restart >/dev/null
restart_cursor=$("$binary" fetch "$session_id" --no-messages | field cursor)
restart_json=$("$binary" restart "$session_id")
expect "$restart_json" "\"id\":\"$session_id\""
expect "$restart_json" '"state":"idle"'
grep -q -- "^${session_id#claude:}\$" "$DLGT_FAKE_ARGS_FILE"
grep -q -- '^--resume$' "$DLGT_FAKE_ARGS_FILE" || fail "restart did not resume the conversation"
show_json=$("$binary" show "$session_id")
expect "$show_json" '"execution_seq":3'
expect "$show_json" '"status":"interrupted"'
"$binary" send "$session_id" --request-id smoke-4 -- after-restart >/dev/null
after_restart=$("$binary" fetch "$session_id" --wait 5s)
expect "$after_restart" '"execution_seq":4'
expect "$after_restart" '"final_text":"fake:after-restart"'
expect "$("$binary" fetch "$session_id" --cursor "$restart_cursor" --no-messages)" \
  '"type":"session.restarting"'
"$binary" stop "$session_id" --force >/dev/null
wait_until_gone @smoke

# Exact aliases are reusable after terminal stop, while the old Session ID remains readable.
reused_id=$("$binary" new --title reused --alias @smoke --harness claude --cwd "$repo_root" \
  --request-id reused-1 -- reused-initial | field id)
expect "$("$binary" fetch "$reused_id" --wait 5s)" '"execution_seq":1'
expect "$("$binary" show "$session_id")" '"state":"stopped"'

# A long poll completes on the provider's result, and the delta carries the
# hydrated result exactly once. New messages alone do not complete it.
poll_cursor=$("$binary" send "$reused_id" --request-id reused-2 -- hold-poll | field cursor)
"$binary" fetch "$reused_id" --cursor "$poll_cursor" --wait 10s >"$state_dir/poll.json" &
poll_pid=$!
sleep 0.3
"$binary" send "$reused_id" --steer --request-id reused-2-steer -- poll-done >/dev/null
wait "$poll_pid"
grep -q '"reason":"result"' "$state_dir/poll.json"
grep -q '"final_text":"fake:poll-done"' "$state_dir/poll.json"
grep -q '"execution_seq":2' "$state_dir/poll.json"
advanced_cursor=$(field cursor <"$state_dir/poll.json")

# Replaying the acceptance cursor replays the same immutable window, while the
# advanced cursor no longer redelivers it.
expect "$("$binary" fetch "$reused_id" --cursor "$poll_cursor")" '"final_text":"fake:poll-done"'
advanced_json=$("$binary" fetch "$reused_id" --cursor "$advanced_cursor")
expect "$advanced_json" '"results":\[\]'
expect "$advanced_json" '"messages":\[\]'

# A retried acceptance replays its receipt instead of creating a second
# execution, and the same request ID with a different payload is rejected. A
# receipt that was still pending follows the live turn to confirmed.
retry_first=$("$binary" send "$reused_id" --request-id retry-1 -- late-payload)
expect "$retry_first" '"execution_seq":3'
expect "$retry_first" '"submission":"pending"'
expect "$retry_first" '"action":"dlgt fetch '
expect "$("$binary" fetch "$reused_id" --wait 10s)" '"final_text":"fake:late-payload"'
retry_replay=$("$binary" send "$reused_id" --request-id retry-1 -- late-payload)
expect "$retry_replay" '"replayed":true'
expect "$retry_replay" '"execution_seq":3'
expect "$retry_replay" '"submission":"confirmed"'
rejected 1 "$binary" send "$reused_id" --request-id retry-1 -- other-payload
expect "$output" '"code":"INVALID_ARGUMENT"'

# Retention overrun is a structured gap with a recoverable baseline, never a
# silent reset.
flood_cursor=$("$binary" send "$reused_id" --request-id reused-4 -- flood-the-log | field cursor)
"$binary" fetch "$reused_id" --wait 20s >/dev/null
gap_json=$("$binary" fetch "$reused_id" --cursor "$flood_cursor")
expect "$gap_json" '"reason":"gap"'
expect "$gap_json" '"component":"messages","reason":"retention_overrun"'
expect "$gap_json" '"text":"flood-'
expect "$("$binary" fetch "$reused_id" --cursor "$flood_cursor" --messages=5)" '"text":"flood-'
refute "$("$binary" fetch "$reused_id" --cursor "$flood_cursor" --no-messages)" '"messages"'
# A page of messages is bounded, and what is left is reported.
page_json=$("$binary" fetch "$reused_id" --cursor "$flood_cursor" --messages=3)
expect "$page_json" '"has_more":true'
test "$(printf '%s\n' "$page_json" | grep -o '"kind":"assistant"' | wc -l | tr -d ' ')" -eq 3

# A long final_text is chunked at a UTF-8 boundary and continued by cursor.
long_cursor=$("$binary" send "$reused_id" --request-id reused-5 -- long-answer | field cursor)
"$binary" fetch "$reused_id" --wait 10s >/dev/null
chunk_json=$("$binary" fetch "$reused_id" --cursor "$long_cursor" --no-messages --max-bytes 2048)
expect "$chunk_json" '"final_text_complete":false'
expect "$chunk_json" '"final_text_offset":0'
expect "$chunk_json" '"has_more":true'
expect "$chunk_json" '"reason":"page_full"'
# The budget went mostly to the result body, so the terminal event is left for
# the next page and the watermark must not have advanced past it.
refute "$chunk_json" '"type":"session.idle"'
# The bound covers the complete compact response line the client prints,
# wrapper and trailing newline included.
chunk_bytes=$(printf '%s\n' "$chunk_json" | wc -c | tr -d ' ')
test "$chunk_bytes" -le 2048 || fail "squeezed fetch line was $chunk_bytes bytes, over its 2048 budget"
chunk_cursor=$(printf '%s\n' "$chunk_json" | field cursor)
rest_json=$("$binary" fetch "$reused_id" --cursor "$chunk_cursor" --no-messages)
expect "$rest_json" '"final_text_complete":true'
refute "$rest_json" '"final_text_offset":0'
expect "$rest_json" '"type":"session.idle"'

# Positions are retained in bounded number: an evicted one expires, and the
# documented recovery is one cursorless baseline.
evicted_cursor=$("$binary" fetch "$reused_id" --no-messages | field cursor)
attempt=0
while [ "$attempt" -lt 80 ]; do
  "$binary" fetch "$reused_id" --no-messages >/dev/null
  attempt=$((attempt + 1))
done
rejected 1 "$binary" fetch "$reused_id" --cursor "$evicted_cursor" --no-messages
expect "$output" '"code":"CURSOR_EXPIRED"'
expect "$("$binary" fetch "$reused_id" --no-messages)" '"reason":"snapshot"'
# A non-numeric position is malformed rather than expired.
rejected 1 "$binary" fetch "$reused_id" --cursor not-a-number
expect "$output" '"code":"CURSOR_INVALID"'

# Restart never steals an alias that a newer active Session owns.
rejected 1 "$binary" restart "$session_id"
expect "$output" '"code":"ALIAS_IN_USE"'
expect "$("$binary" show "$session_id")" '"state":"stopped"'

# Unexpected provider death creates a durable failed result in bounded time,
# and the provider's last words stay readable.
"$binary" send "$reused_id" --request-id reused-6 -- crash >/dev/null
crash_json=$("$binary" fetch "$reused_id" --wait 8s)
expect "$crash_json" '"status":"failed"'
expect "$crash_json" '"state":"failed"'
expect "$crash_json" 'stderr: fake provider crashed'
rejected 1 "$binary" send "$reused_id" --request-id reused-7 -- after-crash
expect "$output" '"code":"SESSION_NOT_RUNNING"'

# A rotated provider Session ID keeps the pre-rekey address and its cursor
# usable, and resuming the conversation keeps the retained message history.
rekey_id=$("$binary" new --title rekey --alias @rekey --harness claude --cwd "$repo_root" \
  --request-id rekey-1 -- rekey-initial | field id)
rekey_cursor=$("$binary" fetch "$rekey_id" --wait 5s | field cursor)
test -n "$rekey_cursor"
printf '%s' "${rekey_id#claude:}" >"$DLGT_FAKE_ROTATE_FILE"
"$binary" restart "$rekey_id" >/dev/null
"$binary" send @rekey --request-id rekey-2 -- rekey-rotated >/dev/null
rotated_id="$rekey_id-rotated"
attempt=0
until "$binary" show "$rotated_id" >/dev/null 2>&1; do
  attempt=$((attempt + 1)); test "$attempt" -lt 200 || fail "the Session was never rekeyed"; sleep 0.02
done
rm -f "$DLGT_FAKE_ROTATE_FILE"
# The pre-rekey address and a cursor taken before the rotation both still
# resolve to the same logical Session.
rotated_json=$("$binary" fetch "$rekey_id" --cursor "$rekey_cursor" --wait 5s)
expect "$rotated_json" "\"id\":\"$rotated_id\""
expect "$rotated_json" '"final_text":"fake:rekey-rotated"'
"$binary" stop "$rotated_id" --force >/dev/null
wait_until_gone @rekey
resume_json=$("$binary" send "$rotated_id" --resume --request-id rekey-3 -- resumed-prompt)
expect "$resume_json" "\"id\":\"$rotated_id\""
"$binary" fetch "$rotated_id" --wait 5s >/dev/null
# Resuming the same provider conversation continues one logical Session, so
# what it said before the resume is still readable.
history_json=$("$binary" fetch "$rotated_id")
expect "$history_json" '"text":"fake:rekey-initial"'
expect "$history_json" '"text":"fake:resumed-prompt"'
expect "$("$binary" fetch "$rekey_id" --cursor "$rekey_cursor")" "\"id\":\"$rotated_id\""
"$binary" stop "$rotated_id" --force >/dev/null

# Codex runs on the app-server alone: no terminal, and approvals in thread
# parameters rather than a bypass flag.
codex_json=$("$binary" new --title codex --alias @codex --harness codex --cwd "$repo_root" \
  --approvals ask --effort high --request-id codex-1 -- codex-initial)
codex_id=$(printf '%s\n' "$codex_json" | field id)
case "$codex_id" in codex:thread-*) ;; *) fail "unexpected Codex Session ID in $codex_json" ;; esac
expect "$codex_json" '"submission":"confirmed"'
expect "$codex_json" '"resolved_model":"fake-codex"'
expect "$("$binary" fetch "$codex_id" --wait 5s)" '"final_text":"fake:codex-initial"'
expect "$("$binary" show "$codex_id")" "\"resume_command\":\"codex resume ${codex_id#codex:}\""
grep -q '"approvalPolicy":"on-request","approvalsReviewer":"user"' "$DLGT_FAKE_CODEX_LOG"
grep -q '"sandbox":"workspace-write"' "$DLGT_FAKE_CODEX_LOG"
grep -q 'model_reasoning_effort="high" app-server$' "$DLGT_FAKE_CODEX_LOG"
if grep -q -- '--remote\|bypass' "$DLGT_FAKE_CODEX_LOG"; then fail "codex was given a terminal or bypass argument"; fi

codex_cursor=$("$binary" send "$codex_id" --request-id codex-2 -- ask-codex | field cursor)
codex_blocked=$("$binary" fetch "$codex_id" --cursor "$codex_cursor" --wait 5s)
expect "$codex_blocked" '"reason":"blocked"'
expect "$codex_blocked" '"cwd":"/work"'
expect "$codex_blocked" '"reason":"writes outside the workspace","summary":"touch outside.txt","tool":"exec"'
expect "$codex_blocked" '"kind":"tool_call","seq":[0-9]*,"summary":"touch outside.txt","tool":"exec"'
expect "$("$binary" answer "$codex_id" r1 --allow)" '"resolution":"allowed"'
codex_allowed=$("$binary" fetch "$codex_id" --cursor "$codex_cursor" --wait 5s)
expect "$codex_allowed" '"final_text":"allowed"'
expect "$codex_allowed" '"by":"caller","decision":"approved"'
"$binary" send "$codex_id" --request-id codex-3 -- ask-again >/dev/null
expect "$("$binary" fetch "$codex_id" --wait 5s)" '"id":"r2"'
"$binary" answer "$codex_id" r2 --deny >/dev/null
codex_denied=$("$binary" fetch "$codex_id" --wait 5s)
expect "$codex_denied" '"final_text":"denied"'
expect "$codex_denied" '"denied_requests":1'

"$binary" send "$codex_id" --request-id codex-4 -- hold-codex >/dev/null
expect "$("$binary" send "$codex_id" --steer --request-id codex-4-steer -- codex-steered)" \
  '"steer":"accepted"'
codex_steered=$("$binary" fetch "$codex_id" --wait 5s)
expect "$codex_steered" '"final_text":"fake:codex-steered"'
expect "$codex_steered" '"delivered":true'
"$binary" send "$codex_id" --request-id codex-5 -- hold-codex-cancel >/dev/null
expect "$("$binary" cancel "$codex_id")" '"status":"canceled"'
expect "$("$binary" restart "$codex_id")" "\"id\":\"$codex_id\""
grep -q '"method":"thread/resume"' "$DLGT_FAKE_CODEX_LOG"
"$binary" send "$codex_id" --request-id codex-6 -- after-restart >/dev/null
expect "$("$binary" fetch "$codex_id" --wait 5s)" '"final_text":"fake:after-restart"'
"$binary" stop "$codex_id" >/dev/null
wait_until_gone @codex
expect "$("$binary" show "$codex_id")" '"state":"stopped"'

# Stopping the daemon takes every provider process with it.
"$binary" server stop >/dev/null
wait "$server_pid"
server_pid=
attempt=0
while pgrep -f "$repo_root/tests/fixtures/fake-" >/dev/null 2>&1; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || fail "a provider process outlived the daemon"
  sleep 0.02
done
echo "dlgt smoke test passed"
