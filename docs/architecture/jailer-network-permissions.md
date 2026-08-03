# Jailer network permission design

Status: accepted

This document records the network-permission model introduced by
[#1079](https://github.com/boxlite-ai/boxlite/pull/1079) to fix
[#1072](https://github.com/boxlite-ai/boxlite/issues/1072).

## Problem

BoxLite previously treated one setting as two different controls:

- `network.mode` decides whether the guest has a network backend.
- `security.networkEnabled` controls network permissions granted to the
  host-side shim by the jailer.

On macOS, Seatbelt classifies Unix-domain socket operations as
`network-bind` and `network-outbound`. Disabling the jailer's network grants
therefore also denied the shim's own AF_UNIX control plane. The shim could not
bind its per-box sockets and the box failed immediately during startup.

## Decision

BoxLite treats the following as three independent concerns:

1. **Guest networking** is controlled by `network.mode`.
2. **Host-side IP permissions** are controlled by
   `security.networkEnabled` (`advanced.security.network_enabled` in Rust).
3. **Shim control-plane sockets** are derived from the per-box socket layout
   and granted independently of IP networking.

When jailer isolation is enabled, it grants the shim the exact AF_UNIX
endpoints it needs. These grants are path-filtered, so they do not permit
AF_INET or AF_INET6 traffic. The broader IP policy is still emitted only when
`networkEnabled` is true.

```text
network.mode --------------------------> guest network backend

security.networkEnabled --------------> host AF_INET/AF_INET6 grants

BoxSockets per-box layout ------------> exact AF_UNIX bind/connect grants
```

## Configuration contract

Both security fields are optional user inputs. Their defaults are
`jailerEnabled=true` and `networkEnabled=true`.

| `network.mode` | `security.networkEnabled` | Result |
| --- | --- | --- |
| `enabled` | `true` | Valid; guest networking is available. |
| `enabled` | `false` | Rejected; this disables host grants, not guest networking. |
| `disabled` | `true` | Valid; no guest network backend is created. |
| `disabled` | `false` | Valid; only the required AF_UNIX control plane is granted. |

The invalid combination is rejected at box creation even when the jailer is
disabled. Otherwise the configuration would claim to disable networking while
the guest could still have a working network.

To create a box without guest networking, use:

```ts
const box = new SimpleBox({
  network: { mode: "disabled" },
});
```

Do not use `security.networkEnabled=false` as the guest-network switch.

## Permission construction

Before generating the sandbox policy, `Jailer::prepare` creates the socket
directory and its short binding symlink. Setup failures stop box creation
before the shim is spawned.

`Jailer::context` then builds separate exact-path lists for sockets the shim
may bind and sockets it may connect to. Both the short binding path and the
resolved storage path are included because Seatbelt may audit either alias.
The directory itself is never granted as a socket namespace.

The resulting `UnixSocketAccess` is passed through `SandboxContext`. The macOS
Seatbelt backend emits literal-path `network-bind` and `network-outbound`
rules for those endpoints, independently of the optional IP-network policy.

Key implementation locations:

- Configuration validation: `src/boxlite/src/runtime/options.rs`
- Security option semantics: `src/boxlite/src/runtime/advanced_options.rs`
- Socket discovery and setup: `src/boxlite/src/jailer/mod.rs`
- Permission model: `src/boxlite/src/jailer/sandbox/mod.rs`
- Seatbelt policy generation: `src/boxlite/src/jailer/sandbox/seatbelt.rs`
- Per-box socket layout: `src/boxlite/src/net/socket_path.rs`

## Security properties

- Disabling guest networking does not break the shim control plane.
- AF_UNIX grants do not enable TCP or UDP networking.
- Writable directories do not automatically become bindable socket
  directories.
- Only known per-box socket endpoints are granted.
- Invalid mixed configurations fail before box startup.
