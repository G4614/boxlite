#!/usr/bin/env bash
#
# reap_boxes.sh — kill leftover boxlite test shims under given home root(s).
#
# Out-of-process backstop for the integration suites. It is wired into the
# `make test:integration:*` recipes via `trap ... EXIT`, so it runs after the
# test runner exits for ANY reason — pass, assertion failure, or (the case
# that matters most) a nextest `--profile vm` timeout, which SIGKILLs the test
# binary and therefore bypasses every in-process cleanup hook (Rust
# `PerTestBoxHome`/`TestContext` Drop, pytest fixtures, vitest `afterAll`).
#
# Detached boxes (`detach=true`) are the leak class: since #851 they no longer
# get bwrap's `--die-with-parent`, so a leaked shim outlives the test binary,
# `make`, and the CI job — accumulating run-over-run on a self-hosted runner
# (#137/#141). The in-process guards cannot catch a SIGKILL'd test; only an
# out-of-process sweep can.
#
# SCOPING — this never does a blanket `pkill boxlite-shim`. It only touches
# processes whose box home lives under one of the ROOT arguments (a per-run
# temp dir unique to this suite), so a developer's real boxlite shims on the
# same machine are left alone.
#
# Usage: reap_boxes.sh <root|glob> [<root|glob> ...]

set -u

reaped=0

# Resolve our own process chain so the argv-scan in step 2 never kills the
# shell running this script or the make/CI process that invoked it.
self_pid=$$
parent_pid=${PPID:-0}

is_self_or_parent() {
  [ "$1" = "$self_pid" ] || [ "$1" = "$parent_pid" ]
}

# Interpreters / build drivers that may legitimately have a root path in argv
# (e.g. a wrapper shell). Never kill these — only test-spawned VM processes.
is_protected_comm() {
  case "$1" in
    bash | sh | dash | zsh | make | gmake | cargo | cargo-nextest | nextest \
      | node | python | python3 | pytest | npm | vitest | ctest | go | reap_boxes.sh)
      return 0
      ;;
    *) return 1 ;;
  esac
}

kill_pid() {
  local pid="$1"
  kill -KILL "$pid" 2>/dev/null && reaped=$((reaped + 1))
}

for root in "$@"; do
  # The caller may pass a literal dir or an unexpanded glob; let the shell
  # expand it here and skip anything that matched nothing.
  for resolved in $root; do
    [ -e "$resolved" ] || continue

    # 1) Precise: every <resolved>/**/boxes/<id>/shim.pid records a live VM.
    while IFS= read -r pidfile; do
      pid=$(head -n1 "$pidfile" 2>/dev/null | tr -dc '0-9')
      [ -n "$pid" ] || continue
      is_self_or_parent "$pid" && continue
      kill -0 "$pid" 2>/dev/null && kill_pid "$pid"
    done < <(find "$resolved" -type f -name shim.pid 2>/dev/null)

    # 2) Catch-all: a shim whose pid file was deleted or corrupted (e.g.
    #    recovery marked the box Stopped + pid=None, so `rm --force` had no
    #    pid to signal) is invisible to step 1. Match any live process whose
    #    argv references this unique per-run root — a shim is launched with
    #    box paths under its home, so its argv carries the root string.
    command -v pgrep >/dev/null 2>&1 || continue
    while IFS= read -r pid; do
      [ -n "$pid" ] || continue
      is_self_or_parent "$pid" && continue
      # `ps comm` is portable across Linux and macOS (no /proc dependency).
      comm=$(ps -p "$pid" -o comm= 2>/dev/null | xargs -r basename 2>/dev/null)
      is_protected_comm "$comm" && continue
      kill_pid "$pid"
    done < <(pgrep -f -- "$resolved" 2>/dev/null)
  done
done

if [ "$reaped" -gt 0 ]; then
  echo "🧹 reap_boxes: SIGKILL'd $reaped leftover test process(es) under: $*" >&2
fi

# Never fail the recipe — this is a best-effort backstop on the exit path.
exit 0
