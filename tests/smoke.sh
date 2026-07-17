#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$repo_root/target/debug/dlgt"
state_dir=$(mktemp -d "${TMPDIR:-/tmp}/dlgt-smoke.XXXXXX")
server_pid=

cleanup() {
  if [ -n "$server_pid" ]; then
    DLGT_HOME="$state_dir" "$binary" server stop >/dev/null 2>&1 || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$state_dir"
}
trap cleanup EXIT INT TERM

export DLGT_HOME="$state_dir"
export HOME="$state_dir/home"
export DLGT_CLAUDE_BIN="$repo_root/tests/fixtures/fake-agent.sh"
export DLGT_FAKE_ARGS_FILE="$state_dir/fake-args.log"
mkdir -p "$HOME"

"$binary" server --foreground >"$state_dir/server.log" 2>&1 &
server_pid=$!
attempt=0
while [ ! -S "$state_dir/dlgt.sock" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done

# `new` is readiness-bounded. Start it while the fixture emits the authoritative hook.
"$binary" new --title smoke --alias @smoke --harness claude --cwd "$repo_root" \
  --harness-option permission-mode=auto >"$state_dir/new.json" &
new_pid=$!
attempt=0
session_id=
while [ -z "$session_id" ]; do
  sessions=$("$binary" list --all)
  session_id=$(printf '%s\n' "$sessions" | sed -n 's/.*"id":"\(ses_[0-9A-Z]*\)".*/\1/p')
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-session"}' \
  | "$binary" hook emit "$session_id" claude
wait "$new_pid"
grep -Eq '"id":"ses_[0-9A-Z]{8}"' "$state_dir/new.json"
grep -q '"alias":"@smoke"' "$state_dir/new.json"
grep -q '"provider_session_id":"provider-session"' "$state_dir/new.json"
grep -q -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE"
if grep -q -- '^--dangerously-skip-permissions$' "$DLGT_FAKE_ARGS_FILE"; then exit 1; fi

# Bounded launch failures retain the failed audit Session ID for diagnostics.
set +e
launch_failure_json=$("$binary" new --title launch-failure --alias @launch-failure \
  --harness claude --cwd "$repo_root" --startup-timeout 50ms)
launch_failure_status=$?
set -e
test "$launch_failure_status" -eq 1
printf '%s\n' "$launch_failure_json" | grep -q '"code":"LAUNCH_FAILED"'
printf '%s\n' "$launch_failure_json" | grep -Eq '"session_id":"ses_[0-9A-Z]{8}"'

long_message=$(awk 'BEGIN { for (i = 0; i < 12000; i++) printf "x" }')
send_json=$("$binary" send "$session_id" -- "$long_message")
printf '%s\n' "$send_json" | grep -q '"execution_seq":1'

set +e
busy_json=$("$binary" send "$session_id" -- second)
busy_status=$?
set -e
test "$busy_status" -eq 5
printf '%s\n' "$busy_json" | grep -q '"code":"SESSION_BUSY"'

printf '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session","turn_id":"provider-turn","user_prompt":"%s"}\n' "$long_message" \
  | "$binary" hook emit "$session_id" claude
printf '{"hook_event_name":"Stop","session_id":"provider-session","turn_id":"provider-turn","last_assistant_message":"done"}\n' \
  | "$binary" hook emit "$session_id" claude
wait_json=$("$binary" wait "$session_id" --timeout 2s)
printf '%s\n' "$wait_json" | grep -q '"status":"completed"'
printf '%s\n' "$wait_json" | grep -q '"final_text":"done"'
printf '%s\n' "$wait_json" | grep -q '"execution_seq":1'
if printf '%s\n' "$wait_json" | grep -q 'turn_'; then exit 1; fi

set +e
plain_logs=$("$binary" logs "$session_id")
plain_status=$?
set -e
test "$plain_status" -eq 1
printf '%s\n' "$plain_logs" | grep -q '"code":"INVALID_ARGUMENT"'
"$binary" logs "$session_id" --raw --json | grep -q '"data_base64"'
"$binary" scrollback "$session_id" --lines 10 | grep -q '"lines"'
"$binary" events "$session_id" | grep -q '"schema_version":1'
"$binary" events "$session_id" --follow >"$state_dir/follow.jsonl" &
follow_pid=$!
attempt=0
while [ ! -s "$state_dir/follow.jsonl" ]; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
kill "$follow_pid"
wait "$follow_pid" 2>/dev/null || true
grep -q '"schema_version":1' "$state_dir/follow.jsonl"
"$binary" models --harness claude | grep -q '"discovery":"partial"'
"$binary" harnesses | grep -q '"codex"'
"$binary" skill | grep -q '^name: dlgt$'

# Restart interrupts active work while preserving identity, provider binding, and history.
"$binary" send "$session_id" -- interrupted-by-restart >/dev/null
option_count_before=$(grep -c -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE")
"$binary" restart "$session_id" >"$state_dir/restart.json" &
restart_pid=$!
attempt=0
while ! "$binary" show "$session_id" | grep -q '"state":"starting"\|"state":"running"'; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-session"}' \
  | "$binary" hook emit "$session_id" claude
wait "$restart_pid"
option_count_after=$(grep -c -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE")
test "$option_count_after" -gt "$option_count_before"
grep -q "\"id\":\"$session_id\"" "$state_dir/restart.json"
"$binary" show "$session_id" | grep -q '"execution_seq":2'
"$binary" show "$session_id" | grep -q '"status":"interrupted"'
"$binary" send "$session_id" -- after-restart >/dev/null
printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"provider-session","turn_id":"provider-turn-3","user_prompt":"after-restart"}' \
  | "$binary" hook emit "$session_id" claude
printf '%s\n' '{"hook_event_name":"Stop","session_id":"provider-session","turn_id":"provider-turn-3","last_assistant_message":"resumed"}' \
  | "$binary" hook emit "$session_id" claude
"$binary" wait "$session_id" --timeout 2s | grep -q '"execution_seq":3'
"$binary" events "$session_id" | grep -q '"type":"session.restarting"'
"$binary" stop "$session_id" --force >/dev/null
attempt=0
while "$binary" show @smoke >/dev/null 2>&1; do
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done

# Exact aliases are reusable after terminal stop, while the old Session ID remains readable.
"$binary" new --title reused --alias @smoke --harness claude --cwd "$repo_root" \
  --env DLGT_FAKE_EXIT_AFTER=2 >"$state_dir/reused.json" &
new_pid=$!
attempt=0
reused_id=
while [ -z "$reused_id" ] || [ "$reused_id" = "$session_id" ]; do
  current=$("$binary" show @smoke 2>/dev/null || true)
  reused_id=$(printf '%s\n' "$current" | sed -n 's/.*"id":"\(ses_[0-9A-Z]*\)".*/\1/p')
  attempt=$((attempt + 1)); test "$attempt" -lt 100 || exit 1; sleep 0.02
done
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"provider-session-2"}' \
  | "$binary" hook emit "$reused_id" claude
wait "$new_pid"
"$binary" show "$session_id" | grep -q '"state":"stopped"'

# A default Session adds --permission-mode=auto beyond the one explicit
# harness option, and never the dangerous bypass flag.
test "$(grep -c -- '^--permission-mode=auto$' "$DLGT_FAKE_ARGS_FILE")" -ge 2
if grep -q -- '^--dangerously-skip-permissions$' "$DLGT_FAKE_ARGS_FILE"; then exit 1; fi

# Restart never steals an alias that a newer active Session owns.
set +e
alias_json=$("$binary" restart "$session_id")
alias_status=$?
set -e
test "$alias_status" -eq 1
printf '%s\n' "$alias_json" | grep -q '"code":"ALIAS_IN_USE"'
"$binary" show "$session_id" | grep -q '"state":"stopped"'

# Unexpected provider death creates a durable failed result in bounded time.
"$binary" send "$reused_id" -- crash >/dev/null
set +e
crash_json=$("$binary" wait "$reused_id" --timeout 4s)
crash_status=$?
set -e
test "$crash_status" -eq 2
printf '%s\n' "$crash_json" | grep -q '"status":"failed"'

"$binary" server stop >/dev/null
wait "$server_pid"
server_pid=
echo "dlgt smoke test passed"
