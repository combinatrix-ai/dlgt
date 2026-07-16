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
done
