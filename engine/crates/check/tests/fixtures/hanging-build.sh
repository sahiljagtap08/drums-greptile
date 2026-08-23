#!/usr/bin/env bash
# Fixture build script for engine-check's process-group regression test
# (tests/adversarial.rs). Simulates a build tool (bundler, test runner) that
# forks a real subprocess and then hangs — the shape a compute-bound
# grandchild takes in real life.
#
# Usage: hanging-build.sh <pids-dump-path>
#
# Spawns a background `sleep 300` in THIS script's own process group
# (inherited automatically — job control is off in a non-interactive
# script), dumps `parent=<pid>` and `grandchild=<pid>` to the given path
# (via a temp file + atomic rename, so a polling reader can never observe a
# half-written dump), then waits on the grandchild itself — for far longer
# than any test's timeout, so the ONLY way both processes ever die is a
# real process-group kill reaching this whole tree, not just the direct
# `sh -c` process `run_script` spawns.
set -euo pipefail

PIDS_DUMP_PATH="$1"

sleep 300 &
GRANDCHILD_PID=$!

{
  printf 'parent=%s\n' "$$"
  printf 'grandchild=%s\n' "$GRANDCHILD_PID"
} > "$PIDS_DUMP_PATH.tmp"
mv "$PIDS_DUMP_PATH.tmp" "$PIDS_DUMP_PATH"

wait "$GRANDCHILD_PID"
