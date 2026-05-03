#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   scripts/profile-perf.sh -- <dbfs args>
# Example:
#   scripts/profile-perf.sh -- --help

if ! command -v perf >/dev/null 2>&1; then
  echo "error: 'perf' not found (linux perf tooling)." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: 'cargo' not found." >&2
  exit 1
fi

if ! command -v rustfilt >/dev/null 2>&1; then
  echo "error: 'rustfilt' not found. Install with: cargo install rustfilt" >&2
  exit 1
fi

if ! command -v flamegraph >/dev/null 2>&1; then
  echo "error: 'flamegraph' not found. Install with: cargo install flamegraph" >&2
  exit 1
fi

if command -v sudo >/dev/null 2>&1; then
  # Some environments mount /usr/bin/sudo nosuid, which breaks flamegraph's internal
  # attempt to elevate perf. Failures are confusing; bail out early.
  if ! sudo -n true >/dev/null 2>&1; then
    echo "error: 'sudo' is present but not usable (likely nosuid)." >&2
    echo "       Run flamegraph/perf manually, or use a system where sudo works." >&2
    exit 1
  fi
fi

if [[ "${1:-}" != "--" ]]; then
  echo "usage: $0 -- <dbfs args>" >&2
  exit 2
fi
shift

mkdir -p /tmp/dbfs-prof

echo "Building (release, with debuginfo)..."
RUSTFLAGS="-C debuginfo=1" cargo build --release

echo "Recording perf + generating flamegraph..."
set +e
flamegraph \
  --root \
  --output /tmp/dbfs-prof/dbfs.svg \
  -- \
  ./target/release/dbfs "$@"
rc=$?
set -e

# If the target exits too quickly (e.g. --help), perf may not capture any stacks.
# In that case, just leave perf.data behind for manual inspection.
if [[ $rc -ne 0 || ! -s /tmp/dbfs-prof/dbfs.svg ]]; then
  echo "warning: flamegraph not generated (process may have exited too quickly)." >&2
fi

echo "Flamegraph written to: /tmp/dbfs-prof/dbfs.svg"
