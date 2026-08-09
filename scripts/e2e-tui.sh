#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  printf 'usage: %s BINARY\n' "$0" >&2
  exit 2
fi

binary=$1
case "$binary" in
  /*) : ;;
  *)
    binary_dir=$(CDPATH='' cd -- "$(dirname -- "$binary")" && pwd)
    binary=${binary_dir}/$(basename -- "$binary")
    ;;
esac

[ -x "$binary" ] || {
  printf '%s is not executable\n' "$binary" >&2
  exit 1
}
command -v tmux >/dev/null 2>&1 || {
  printf "need 'tmux' on PATH\n" >&2
  exit 1
}
command -v stty >/dev/null 2>&1 || {
  printf "need 'stty' on PATH\n" >&2
  exit 1
}

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/mach-tui-e2e.XXXXXX")
socket=${tmpdir}/tmux.sock
session=mach-tui-e2e
wait_attempts=200
pane=
terminal_status=

cleanup() {
  tmux -S "$socket" kill-server 2>/dev/null || :
  rm -rf "$tmpdir"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

capture_pane() {
  pane=$(tmux -S "$socket" capture-pane -p -t "$session" 2>/dev/null || :)
}

wait_for_text() {
  expected=$1
  attempts=0
  while [ "$attempts" -lt "$wait_attempts" ]; do
    capture_pane
    case "$pane" in
      *"$expected"*) return 0 ;;
    esac
    if ! tmux -S "$socket" has-session -t "$session" 2>/dev/null; then
      printf 'TUI exited while waiting for %s\n%s\n' "$expected" "$pane" >&2
      return 1
    fi
    attempts=$((attempts + 1))
    sleep 0.05
  done
  printf 'timed out waiting for %s\n%s\n' "$expected" "$pane" >&2
  return 1
}

wait_for_exit() {
  attempts=0
  while tmux -S "$socket" has-session -t "$session" 2>/dev/null; do
    attempts=$((attempts + 1))
    if [ "$attempts" -ge "$wait_attempts" ]; then
      capture_pane
      printf 'TUI did not exit\n%s\n' "$pane" >&2
      return 1
    fi
    sleep 0.05
  done
}

start_tui() {
  data_dir=$1
  run_name=$2
  terminal_status=${tmpdir}/${run_name}.terminal
  # The quoted program is expanded by the shell running inside the tmux pane.
  # shellcheck disable=SC2016
  MACH_DIR="$data_dir" \
  MACH_UPDATE_STATE_DIR="${tmpdir}/update-state" \
  MACH_E2E_BINARY="$binary" \
  MACH_E2E_STATUS="$terminal_status" \
    tmux -S "$socket" new-session -d -x 120 -y 40 -s "$session" \
    'before=$(stty -g)
     "$MACH_E2E_BINARY"
     result=$?
     after=$(stty -g)
     printf "%s\n%s\n%s\n" "$result" "$before" "$after" > "$MACH_E2E_STATUS"
     exit "$result"'
}

assert_terminal_restored() {
  [ -f "$terminal_status" ] || {
    printf 'TUI exited without recording terminal state\n' >&2
    return 1
  }
  {
    IFS= read -r exit_status
    IFS= read -r terminal_before
    IFS= read -r terminal_after
  } < "$terminal_status"
  [ "$exit_status" = 0 ] || {
    printf 'TUI exited with status %s\n' "$exit_status" >&2
    return 1
  }
  [ "$terminal_before" = "$terminal_after" ] || {
    printf 'TUI did not restore terminal modes\nbefore: %s\nafter:  %s\n' \
      "$terminal_before" "$terminal_after" >&2
    return 1
  }
}

stop_tui() {
  tmux -S "$socket" send-keys -t "$session" C-c
  wait_for_text 'Press Ctrl+C again to quit'
  tmux -S "$socket" send-keys -t "$session" C-c
  wait_for_exit
  assert_terminal_restored
  tmux -S "$socket" kill-server 2>/dev/null || :
}

assert_list_contains() {
  data_dir=$1
  expected=$2
  output=$("$binary" --dir "$data_dir" --json list)
  case "$output" in
    *"\"title\": \"$expected\""*) : ;;
    *)
      printf 'persisted task %s was not returned by the CLI\n%s\n' \
        "$expected" "$output" >&2
      return 1
      ;;
  esac
}

roundtrip_data=${tmpdir}/roundtrip
roundtrip_title='E2E task round trip'
start_tui "$roundtrip_data" roundtrip-create
wait_for_text 'Welcome to mach'
tmux -S "$socket" send-keys -t "$session" C-a
wait_for_text 'New task'
tmux -S "$socket" send-keys -l -t "$session" "$roundtrip_title"
tmux -S "$socket" send-keys -t "$session" C-s
wait_for_text "$roundtrip_title"
stop_tui
assert_list_contains "$roundtrip_data" "$roundtrip_title"

# Reopening the real binary must render the task loaded from SQLite, not just
# leave it in the first process's in-memory state.
start_tui "$roundtrip_data" roundtrip-reopen
wait_for_text "$roundtrip_title"
stop_tui
printf 'TUI end-to-end passed: task round trip\n'

external_data=${tmpdir}/external
external_title='E2E external commit'
start_tui "$external_data" external-refresh
wait_for_text 'Welcome to mach'
tmux -S "$socket" send-keys -t "$session" Enter
wait_for_text 'All tasks'
"$binary" --dir "$external_data" --json add "$external_title" >/dev/null
wait_for_text "$external_title"
stop_tui
assert_list_contains "$external_data" "$external_title"
printf 'TUI end-to-end passed: external CLI refresh\n'
