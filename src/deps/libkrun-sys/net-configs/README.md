# Privileged kernel config overlay

This directory holds `--kernel net` (DinD) kernel config overlays
applied on top of the upstream libkrunfw lean configs (at
`../vendor/libkrunfw/config-libkrunfw_<arch>`). They turn on the
networking subsystems Docker needs for bridge networks, NAT, and
iptables rule installation — i.e. everything required by the workflows
issue #276 calls out as broken under the lean profile (`docker compose
up` with custom bridge networks, port publishing, container-to-
container DNS).

The overlays do NOT touch any unrelated config knob. The resulting
"fat" libkrunfw is shipped alongside the lean one as a second `.so`
blob, opt-in via the per-box `net` flag. The default profile
remains the lean libkrunfw, byte-for-byte identical to today.

## How the overlay is applied

`make libkrunfw-net` does:

1. Copy `vendor/libkrunfw/config-libkrunfw_<arch>` → `vendor/libkrunfw/.config`
2. Append the relevant overlay file (this dir, `overlay-net_<arch>`)
   to `.config`
3. Run `make olddefconfig` inside `vendor/libkrunfw/<kernel-src>` so the
   Kconfig dependency resolver fills in any newly-required parent
   options (e.g. `CONFIG_NETFILTER_ADVANCED=y` once `CONFIG_NF_TABLES=y`)
4. Build libkrunfw as usual
5. Stamp the SONAME to a distinct filename
   (`libkrunfw-net.so.5`) so it can sit next to the lean one

The boxlite runtime stages both blobs and selects between them at VM
spawn time based on `BoxOptions::net`.

## Why not modify the upstream lean config

Two reasons. First, every CONFIG flag we add to lean grows the kernel
binary embedded in every box on the planet — that defeats the lean
profile's whole purpose (small footprint, fast boot, smaller attack
surface). Second, keeping the overlay external means upstream libkrunfw
updates (config refreshes, version bumps) merge cleanly without
conflicts in the file we maintain ourselves.

## Adding a new arch

`overlay-net_aarch64` is currently a copy of the x86_64 overlay since
the same CONFIG_* knobs apply. If a new arch needs different settings,
add a per-arch overlay here.

## Building a kernel with your own config

`overlay-net_<arch>` is just one overlay. To build a libkrunfw kernel
with your OWN modules, write an overlay (a file of `CONFIG_*=y` lines)
and run:

```
OVERLAY=/path/to/my-overlay make libkrunfw-custom
# → target/custom-kernel/lib64/libkrunfw-custom.so.5  (override with OUT=)
```

Then load it at runtime — no boxlite rebuild needed (the runtime symlinks
the blob into the box's libs dir and dlopens it):

```
boxlite run --kernel target/custom-kernel/lib64/libkrunfw-custom.so.5 ...
```

This differs from `--kernel net`, which stamps a distinct SONAME and lands
at the canonical path embedded into the runtime by `libkrun-sys/build.rs`.
A custom blob keeps the default `libkrunfw.so.5` SONAME and is loaded by
path, so it is never embedded.

`DRY_RUN=1 OVERLAY=... make libkrunfw-custom` validates the overlay and the
config merge without the (~10-20 min) kernel build.

`make libkrunfw-net` is the same machinery with the net overlay pinned;
both delegate to `scripts/build/build-libkrunfw.sh`.
