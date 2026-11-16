#!/bin/sh
# A stand-in for `claude -p` speaking stream-json, driven by prompt keywords:
#   hold*      stay busy until interrupted or steered
#   ask*       request permission for a Bash call, then report the answer
#   question*  ask the user a question, then report the answer
#   late*      acknowledge the prompt only after six seconds
#   long*      answer with 12000 characters
#   flood*     emit 12000 assistant messages before answering
#   crash*     write to stderr and exit non-zero
# While holding, a steer starting with `queued` is left unread; any other
# steer is read and ends the turn.

if [ -n "${DLGT_FAKE_ARGS_FILE:-}" ]; then
  printf '%s\n' "$@" >>"$DLGT_FAKE_ARGS_FILE"
fi

if [ "$1" = auth ] && [ "${2:-}" = status ]; then
  if [ -f "$HOME/.fake-auth-delay" ]; then
    delay=$(cat "$HOME/.fake-auth-delay")
    printf '%s\n' "$$" >"$HOME/.fake-auth-parent-pid"
    sleep "$delay" &
    auth_child=$!
    printf '%s\n' "$auth_child" >"$HOME/.fake-auth-child-pid"
    wait "$auth_child"
  fi
  if [ -f "$HOME/.fake-auth-status" ]; then
    if [ -f "$HOME/.fake-auth-output-bytes" ]; then
      output_bytes=$(cat "$HOME/.fake-auth-output-bytes")
      awk -v count="$output_bytes" 'BEGIN { for (i = 0; i < count; i++) printf " " }'
    fi
    cat "$HOME/.fake-auth-status"
    if [ -f "$HOME/.fake-auth-trailing-bytes" ]; then
      trailing=$(cat "$HOME/.fake-auth-trailing-bytes")
      awk -v count="$trailing" 'BEGIN { for (i = 0; i < count; i++) printf " "; printf "not json" }'
    fi
    if [ -f "$HOME/.fake-auth-hold-stdout" ]; then
      sleep "$(cat "$HOME/.fake-auth-hold-stdout")" &
      printf '%s\n' "$!" >"$HOME/.fake-auth-holder-pid"
      exit 0
    fi
  else
    printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai"}'
  fi
  exit 0
fi

if [ -n "${DLGT_FAKE_PID_FILE:-}" ]; then
  printf '%s\n' "$$" >"$DLGT_FAKE_PID_FILE"
fi

session=
mode=default
model=fake-model
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id | --resume) session=$2; shift ;;
    --permission-mode) [ "$2" = auto ] && mode=auto; shift ;;
    --dangerously-skip-permissions) mode=bypassPermissions ;;
    --model) model=$2; shift ;;
  esac
  shift
done
# The provider may refuse auto mode for a model and fall back.
[ "$model" = no-auto-model ] && [ "$mode" = auto ] && mode=default
if [ -n "${DLGT_FAKE_ROTATE_FILE:-}" ] && [ -f "$DLGT_FAKE_ROTATE_FILE" ] \
  && [ "$(cat "$DLGT_FAKE_ROTATE_FILE")" = "$session" ]; then
  session="$session-rotated"
fi
key_source=none
[ -n "${ANTHROPIC_API_KEY:-}" ] && key_source=ANTHROPIC_API_KEY
[ -n "${DLGT_FAKE_API_KEY_SOURCE:-}" ] && key_source=$DLGT_FAKE_API_KEY_SOURCE

if [ -n "${DLGT_FAKE_NO_INIT:-}" ]; then
  echo 'Not logged in · Please run /login' >&2
  exit 1
fi

init_sent=0
emit_init() {
  printf '{"type":"system","subtype":"init","session_id":"%s","model":"%s","permissionMode":"%s","apiKeySource":"%s"}\n' \
    "$session" "$model" "$mode" "$key_source"
  init_sent=1
}
[ -n "${DLGT_FAKE_INIT_ON_START:-}" ] && emit_init

holding=0
asking=
queued=
ask_id=0

field() {
  printf '%s' "$line" | sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p"
}
queued_contains() {
  for queued_uuid in $queued; do
    [ "$queued_uuid" = "$1" ] && return 0
  done
  return 1
}
lifecycle() {
  printf '{"type":"command_lifecycle","command_uuid":"%s","state":"%s"}\n' "$1" "$2"
}
replay() {
  printf '{"type":"user","uuid":"%s","isReplay":true,"message":{"role":"user","content":"%s"}}\n' "$1" "$2"
}
say() {
  printf '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"%s"}]}}\n' "$1"
}
finish() {
  say "$1"
  printf '{"type":"result","subtype":"success","is_error":false,"result":"%s","usage":{"input_tokens":1,"output_tokens":1},"permission_denials":[%s]}\n' "$1" "${2:-}"
}
control_ok() {
  printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":%s}}\n' "$1" "$2"
}

while IFS= read -r line; do
  case "$line" in
    *'"type":"control_request"'*)
      request=$(field request_id)
      case "$line" in
        *'"subtype":"initialize"'*)
          control_ok "$request" "{\"current_permission_mode\":\"$mode\",\"session_state\":\"idle\"}"
          ;;
        *'"subtype":"cancel_async_message"'*)
          target=$(field message_uuid)
          if [ -n "$queued" ] && queued_contains "$target"; then
            lifecycle "$target" cancelled
            remaining=
            for queued_uuid in $queued; do
              [ "$queued_uuid" = "$target" ] || remaining="$remaining $queued_uuid"
            done
            queued=$remaining
            control_ok "$request" '{"cancelled":true}'
          else
            control_ok "$request" '{"cancelled":false}'
          fi
          ;;
        *'"subtype":"interrupt"'*)
          if [ -n "${DLGT_FAKE_INTERRUPT_DELAY:-}" ]; then sleep "$DLGT_FAKE_INTERRUPT_DELAY"; fi
          if [ -n "$asking" ]; then
            printf '{"type":"control_cancel_request","request_id":"ask-%s"}\n' "$ask_id"
          fi
          control_ok "$request" '{"still_queued":[]}'
          if [ "$holding" = 1 ] || [ -n "$asking" ]; then
            printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"permission_denials":[]}'
            if [ -n "${DLGT_FAKE_INTERRUPT_RESULT_FILE:-}" ]; then
              : >"$DLGT_FAKE_INTERRUPT_RESULT_FILE"
              while [ ! -e "${DLGT_FAKE_INTERRUPT_RELEASE_FILE:-/nonexistent}" ]; do sleep 0.002; done
            fi
          fi
          holding=0
          asking=
          ;;
        *) control_ok "$request" '{}' ;;
      esac
      ;;
    *'"type":"control_response"'*)
      case "$asking" in
        approval)
          case "$line" in
            *'"behavior":"allow"'*) finish allowed ;;
            *) finish "denied:$(field message)" '{"tool_name":"Bash"}' ;;
          esac
          ;;
        question)
          finish "answered:$(printf '%s' "$line" | sed -n 's/.*"answers":{"[^"]*":"\([^"]*\)".*/\1/p')"
          ;;
      esac
      asking=
      ;;
    *'"type":"user"'*)
      uuid=$(field uuid)
      content=$(field content)
      lifecycle "$uuid" queued
      if [ "$holding" = 1 ]; then
        case "$content" in
          queued*) queued="$queued $uuid" ;;
          *)
            replay "$uuid" "$content"
            holding=0
            finish "fake:$content"
            ;;
        esac
        continue
      fi
      case "$content" in late*) sleep 6 ;; esac
      lifecycle "$uuid" started
      if [ -z "${DLGT_FAKE_INIT_ONCE:-}" ] || [ "$init_sent" -eq 0 ]; then emit_init; fi
      replay "$uuid" "$content"
      case "$content" in
        hold*) holding=1 ;;
        ask*)
          asking=approval
          ask_id=$((ask_id + 1))
          printf '{"type":"control_request","request_id":"ask-%s","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch probe.txt"},"description":"Create a file"}}\n' "$ask_id"
          ;;
        question*)
          asking=question
          ask_id=$((ask_id + 1))
          printf '{"type":"control_request","request_id":"ask-%s","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","requires_user_interaction":true,"input":{"questions":[{"question":"Which color?","header":"Color","options":[{"label":"Red","description":"r"},{"label":"Blue","description":"b"}],"multiSelect":false}]}}}\n' "$ask_id"
          ;;
        crash*)
          echo 'fake provider crashed' >&2
          exit 1
          ;;
        flood*)
          awk 'BEGIN { for (i = 0; i < 12000; i++) printf "{\"type\":\"assistant\",\"parent_tool_use_id\":null,\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"flood-%d\"}]}}\n", i }'
          finish flooded
          ;;
        long*) finish "$(awk 'BEGIN { for (i = 0; i < 12000; i++) printf "x" }')" ;;
        *) finish "fake:$content" ;;
      esac
      ;;
  esac
done
