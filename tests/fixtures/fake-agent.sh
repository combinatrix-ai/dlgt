#!/bin/sh

if [ -n "${DLGT_FAKE_ARGS_FILE:-}" ]; then
  printf '%s\n' "$@" >>"$DLGT_FAKE_ARGS_FILE"
fi

stty -icanon -echo
printf '\033[?25hdlgt fake agent ready\r\n'
if [ -n "${DLGT_FAKE_EXIT_AFTER:-}" ]; then
  (sleep "$DLGT_FAKE_EXIT_AFTER"; kill -TERM "$$") &
fi
while IFS= read -r line; do
  printf 'fake:%s\r\n' "$line"
  case "$line" in
    *crash*) kill -TERM "$$" ;;
    *flood*)
      # Emit more rows than the stable-row retention bound so a cursor taken
      # before the flood provably falls off the retained floor.
      index=0
      while [ "$index" -lt 12000 ]; do
        printf 'flood-%s\r\n' "$index"
        index=$((index + 1))
      done
      ;;
  esac
done
