#!/usr/bin/env bash
# Build a custom libkrunfw kernel blob from a config overlay.
#
# This is the generalized core behind `make libkrunfw-net` (the built-in
# "net" variant) and `make libkrunfw-custom` (user-supplied overlay). It
# merges a config overlay on top of the upstream lean libkrunfw config,
# builds the kernel, stamps a SONAME, and stages the resulting `.so` at a
# chosen path.
#
# A user-built blob is loaded at runtime with `boxlite run --kernel <path>`
# (no rebuild needed — the runtime symlinks it into the box's libs dir and
# dlopens it). The "net" variant instead stamps a distinct SONAME and lands
# at the canonical path that `libkrun-sys/build.rs` embeds into the runtime.
#
# Parameters (all via env):
#   OVERLAY   path to a config overlay to append on top of the lean config.
#             Required — it's what makes the kernel non-lean.
#   KCONFIG   base libkrunfw config NAME under vendor/libkrunfw/
#             (default: arch lean `config-libkrunfw_<arch>`).
#   SONAME    ELF SONAME stamped on the output (default: `libkrunfw.so.5`,
#             which is what `--kernel <path>` expects).
#   OUT       output blob path
#             (default: target/custom-kernel/lib64/libkrunfw-custom.so.5).
#   HINT      optional one-line "next step" message printed after the build.
#   DRY_RUN   if set, resolve + validate + merge the config and print the
#             plan, but skip the (~10-20 min) kernel build and staging.
#   ARCH      override target arch (default: uname -m).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LIBKRUNFW_SRC="$REPO_ROOT/src/deps/libkrun-sys/vendor/libkrunfw"

ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
  x86_64)        DEFAULT_KCONFIG="config-libkrunfw_x86_64"  ;;
  aarch64|arm64) DEFAULT_KCONFIG="config-libkrunfw_aarch64" ;;
  *) echo "❌ unsupported ARCH=$ARCH" >&2; exit 1 ;;
esac

KCONFIG_NAME="${KCONFIG:-$DEFAULT_KCONFIG}"
SONAME="${SONAME:-libkrunfw.so.5}"
OUT="${OUT:-$REPO_ROOT/target/custom-kernel/lib64/libkrunfw-custom.so.5}"
OVERLAY="${OVERLAY:-}"

# ── Sanity ──────────────────────────────────────────────────────────────────

if [ -z "$OVERLAY" ]; then
  echo "❌ OVERLAY is required (path to a config overlay to append)." >&2
  echo "   Write a file of CONFIG_*=y lines for the subsystems you need," >&2
  echo "   then: OVERLAY=/path/to/overlay make libkrunfw-custom" >&2
  exit 1
fi
if [ ! -f "$OVERLAY" ]; then
  echo "❌ overlay not found: $OVERLAY" >&2; exit 1
fi

if [ ! -f "$LIBKRUNFW_SRC/Makefile" ]; then
  echo "❌ libkrunfw submodule not initialised at $LIBKRUNFW_SRC" >&2
  echo "   Run:  git submodule update --init --recursive src/deps/libkrun-sys/vendor/libkrunfw" >&2
  exit 1
fi

LEAN_CONFIG="$LIBKRUNFW_SRC/$KCONFIG_NAME"
if [ ! -f "$LEAN_CONFIG" ]; then
  echo "❌ base config not found: $LEAN_CONFIG" >&2; exit 1
fi

# ── Merge overlay onto the lean config ──────────────────────────────────────
#
# Append the overlay to the lean config. The overlay's `CONFIG_X=y` lines
# OVERRIDE the lean config's `# CONFIG_X is not set` lines because Kconfig
# parses sequentially. `make olddefconfig` (run by libkrunfw's Makefile)
# fills in any dependent options the newly-enabled parents require.
MERGED_CONFIG="$(mktemp)"
trap 'rm -f "$MERGED_CONFIG"' EXIT
cat "$LEAN_CONFIG" "$OVERLAY" > "$MERGED_CONFIG"

echo "🔧 libkrunfw build plan ($ARCH)"
echo "   base config: $LEAN_CONFIG"
echo "   overlay:     $OVERLAY"
echo "   merged size: $(wc -l < "$MERGED_CONFIG") lines"
echo "   soname:      $SONAME"
echo "   output:      $OUT"

if [ -n "${DRY_RUN:-}" ]; then
  echo "🟡 DRY_RUN set — validated config merge, skipping kernel build + staging."
  exit 0
fi

# Swap the merged config into the submodule's config in place so libkrunfw's
# Makefile (which always reads $KCONFIG_NAME) picks it up. Restore on exit so
# the lean build path isn't permanently polluted with the overlay's lines.
LEAN_BACKUP="$(mktemp)"
trap 'cp -f "$LEAN_BACKUP" "$LEAN_CONFIG" 2>/dev/null || true; rm -f "$MERGED_CONFIG" "$LEAN_BACKUP"' EXIT
cp "$LEAN_CONFIG" "$LEAN_BACKUP"
cp "$MERGED_CONFIG" "$LEAN_CONFIG"

# ── Build ───────────────────────────────────────────────────────────────────

echo "🔨 Building libkrunfw (this downloads kernel source on first run, ~10-20 min)..."
cd "$LIBKRUNFW_SRC"
make -j"$(nproc)" MAKEFLAGS=""

# ── Stage the result ────────────────────────────────────────────────────────

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"

# libkrunfw's Makefile produces libkrunfw.so.5.<minor>.<patch> with a symlink
# chain libkrunfw.so.5 → it. Copy the real file and stamp the requested SONAME.
REAL_BLOB=$(ls "$LIBKRUNFW_SRC"/libkrunfw.so.5.* 2>/dev/null | head -1 || true)
if [ -z "$REAL_BLOB" ]; then
  echo "❌ build succeeded but couldn't find libkrunfw.so.5.* in $LIBKRUNFW_SRC" >&2
  exit 1
fi

cp "$REAL_BLOB" "$OUT"
patchelf --set-soname "$SONAME" "$OUT"

echo ""
echo "✅ Built libkrunfw: $OUT"
echo "   Size: $(du -h "$OUT" | cut -f1) (vs lean: $(du -h "$REAL_BLOB" 2>/dev/null | cut -f1 || echo '?'))"
if [ -n "${HINT:-}" ]; then
  echo ""
  echo "$HINT"
fi
