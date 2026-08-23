#!/usr/bin/env bash
# Fake repair agent for engine-repair tests (and reused by demo/fake-agent.sh
# in Stage 2 Task 6). Ignores the SEMANTIC content of the prompt it receives
# — it does not read the failure signature or attribution to decide what to
# fix — and instead applies the one known fix this fixture exists to exercise:
# the checkout demo's promo-guard null check in server.js (cwd).
#
# Control flags are used instead of environment variables on purpose: the
# real CliRepairAgent runs the agent child with a MINIMAL, cleared
# environment (PATH, HOME, USER, and only present ANTHROPIC_*/CLAUDE_*/
# OPENAI_*/CODEX_* vars) — env vars set in the *test* process are not
# forwarded to this child, so test control has to travel as literal argv
# tokens baked into the per-test command_template instead.
#
#   --noop                        do nothing; exit 0 with no file changes
#   --sleep-secs N                sleep N seconds before doing anything else
#   --dump-prompt-to PATH         write the received prompt argv verbatim to PATH
#   --dump-env-to PATH            write the child's full environment to PATH
#                                 (one KEY=VALUE per line), so a test can
#                                 assert exactly what CliRepairAgent forwarded
#   --spawn-grandchild-write PATH background a job (inherits this script's
#                                 process group, since job control is off in
#                                 a non-interactive script) that sleeps 1s
#                                 then writes a marker to PATH — simulates a
#                                 real agent CLI's subprocess tree
#   --spawn-grandchild-sleep N    background a long-lived `sleep N` in this
#                                 script's process group and remember its pid,
#                                 so a test can assert by PID (not by a
#                                 side-effect marker) that a group kill
#                                 actually reached the agent's subprocess tree
#   --dump-pids-to PATH           write `parent=<pid>` and, if a grandchild
#                                 was spawned, `grandchild=<pid>` to PATH —
#                                 the seam a test uses to prove BOTH processes
#                                 were alive first and dead after
#   --hold-stdout-secs N          background a job that holds this script's
#                                 stdout fd open (without writing to it) for
#                                 N seconds, so the reader on the other end
#                                 of the pipe never sees EOF until it exits
#   --emit-stdout-kb N            print N KiB of agent-trace-shaped lines to
#                                 stdout BEFORE doing any work — the shape of
#                                 a real `codex exec` run trace or a verbose
#                                 `claude -p`. Anything past the ~64 KiB pipe
#                                 buffer blocks this script in write(2) unless
#                                 the caller is draining stdout concurrently
#                                 with its own wait on this process.
#   --create-file PATH            write a brand-new UNTRACKED file at PATH
#                                 (relative to cwd) instead of touching
#                                 server.js
#
# The remaining (non-flag) argument is the prompt text itself, passed as a
# single argv element by the caller (never through a shell).
set -euo pipefail

NOOP=0
SLEEP_SECS=0
DUMP_PATH=""
ENV_DUMP_PATH=""
GRANDCHILD_WRITE_PATH=""
GRANDCHILD_SLEEP_SECS=0
PIDS_DUMP_PATH=""
HOLD_STDOUT_SECS=0
EMIT_STDOUT_KB=0
CREATE_FILE_PATH=""
PROMPT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --noop)
      NOOP=1
      shift
      ;;
    --sleep-secs)
      SLEEP_SECS="$2"
      shift 2
      ;;
    --dump-prompt-to)
      DUMP_PATH="$2"
      shift 2
      ;;
    --dump-env-to)
      ENV_DUMP_PATH="$2"
      shift 2
      ;;
    --spawn-grandchild-write)
      GRANDCHILD_WRITE_PATH="$2"
      shift 2
      ;;
    --spawn-grandchild-sleep)
      GRANDCHILD_SLEEP_SECS="$2"
      shift 2
      ;;
    --dump-pids-to)
      PIDS_DUMP_PATH="$2"
      shift 2
      ;;
    --hold-stdout-secs)
      HOLD_STDOUT_SECS="$2"
      shift 2
      ;;
    --emit-stdout-kb)
      EMIT_STDOUT_KB="$2"
      shift 2
      ;;
    --create-file)
      CREATE_FILE_PATH="$2"
      shift 2
      ;;
    *)
      PROMPT="$1"
      shift
      ;;
  esac
done

if [[ -n "$DUMP_PATH" ]]; then
  printf '%s' "$PROMPT" > "$DUMP_PATH"
fi

if [[ -n "$ENV_DUMP_PATH" ]]; then
  env > "$ENV_DUMP_PATH"
fi

if [[ -n "$GRANDCHILD_WRITE_PATH" ]]; then
  ( sleep 1; printf 'grandchild wrote after the parent was killed\n' > "$GRANDCHILD_WRITE_PATH" ) &
fi

GRANDCHILD_PID=""
if [[ "$GRANDCHILD_SLEEP_SECS" != "0" ]]; then
  sleep "$GRANDCHILD_SLEEP_SECS" &
  GRANDCHILD_PID=$!
fi

if [[ -n "$PIDS_DUMP_PATH" ]]; then
  # Written via a temp file + rename so a polling test can never read a
  # half-written dump and parse only one of the two pids.
  {
    printf 'parent=%s\n' "$$"
    if [[ -n "$GRANDCHILD_PID" ]]; then
      printf 'grandchild=%s\n' "$GRANDCHILD_PID"
    fi
  } > "$PIDS_DUMP_PATH.tmp"
  mv "$PIDS_DUMP_PATH.tmp" "$PIDS_DUMP_PATH"
fi

if [[ "$HOLD_STDOUT_SECS" != "0" ]]; then
  ( sleep "$HOLD_STDOUT_SECS" ) >&1 &
fi

if [[ "$EMIT_STDOUT_KB" != "0" ]]; then
  # 8 lines x ~128 bytes = ~1 KiB. Emitted BEFORE the repair below on
  # purpose: a caller that only starts reading stdout after this process
  # exits wedges here forever once the kernel pipe buffer (~64 KiB) fills,
  # and its "timeout" then reports a repair that never got the chance to
  # happen.
  FILLER=$(printf 'x%.0s' {1..100})
  EMIT_LINES=$(( EMIT_STDOUT_KB * 8 ))
  for (( EMIT_I = 0; EMIT_I < EMIT_LINES; EMIT_I++ )); do
    printf 'agent trace line %06d %s\n' "$EMIT_I" "$FILLER"
  done
fi

if [[ "$SLEEP_SECS" != "0" ]]; then
  sleep "$SLEEP_SECS"
fi

if [[ "$NOOP" == "1" ]]; then
  echo "no-op: nothing to change"
  exit 0
fi

# --create-file is exclusive with the server.js fix: it exists to exercise a
# repair that ONLY adds a new (untracked) file, with no tracked-file diff at
# all, so a test can catch a `git diff --stat` (tracked-only) regression.
if [[ -n "$CREATE_FILE_PATH" ]]; then
  printf 'new file from the fake agent\n' > "$CREATE_FILE_PATH"
  echo "added a new helper file"
  exit 0
fi

if [[ -f server.js ]]; then
  node -e "
    const fs = require('fs');
    const p = 'server.js';
    let s = fs.readFileSync(p, 'utf8');
    s = s.replace('body.promo.code', 'body.promo && body.promo.code');
    fs.writeFileSync(p, s);
  "
fi

echo "fixed the promo guard null check on the checkout path"
