#!/usr/bin/env bash
# Build the built-in "fat" libkrunfw variant required by `boxlite run --kernel net`.
#
# Thin wrapper over build-libkrunfw.sh: it pins the net overlay, a distinct
# SONAME (so the net blob can sit next to the lean one in the embedded runtime
# without a dlopen identity collision), and the canonical output path that
# `libkrun-sys/build.rs` auto-detects and embeds on the next `make cli`.
#
# Why not download a prebuilt: the net kernel adds ~2 MB of network subsystems
# on top of the lean kernel. Until upstream boxlite-ai/libkrunfw publishes the
# net variant in its releases, anyone iterating on `--kernel net` builds it
# locally. (Set BOXLITE_LIBKRUNFW_NET_PATH only when the blob lives outside the
# workspace — e.g. CI cache, distro packaging.)
#
# To build a kernel with your OWN config instead, see `make libkrunfw-custom`
# (src/deps/libkrun-sys/net-configs/README.md).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

ARCH="${ARCH:-$(uname -m)}"
case "$ARCH" in
  x86_64)        OVERLAY_NAME="overlay-net_x86_64"  ;;
  aarch64|arm64) OVERLAY_NAME="overlay-net_aarch64" ;;
  *) echo "❌ unsupported ARCH=$ARCH (only x86_64 and aarch64 have a net overlay yet)" >&2; exit 1 ;;
esac

OVERLAY="$REPO_ROOT/src/deps/libkrun-sys/net-configs/$OVERLAY_NAME" \
SONAME="libkrunfw-net.so.5" \
OUT="$REPO_ROOT/target/net-kernel/lib64/libkrunfw-net.so.5" \
HINT="Rebuild boxlite to embed it (libkrun-sys/build.rs auto-detects this path):

   make cli

Then \`boxlite run --kernel net\` loads this kernel instead of the lean one." \
ARCH="$ARCH" \
  exec bash "$SCRIPT_DIR/build-libkrunfw.sh"
