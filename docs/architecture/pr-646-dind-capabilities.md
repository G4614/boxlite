# PR 646: Privileged Containers

PR 646 adds a `privileged` container profile for Docker-in-Docker (DinD) and
other workloads that need to manage nested containers. The profile is
Docker-shaped inside the BoxLite guest, but it is not a host-escape switch:
the BoxLite VM remains the outer security boundary.

## User Interface

### CLI

Start a DinD box immediately:

```bash
boxlite run --privileged docker:dind
```

Create a named box and start it later:

```bash
boxlite create --privileged --name dind docker:dind
boxlite start dind
```

The same `run` and `create` commands also support the narrower capability
interface:

```bash
boxlite run --cap-add NET_ADMIN alpine:latest
boxlite run --cap-drop ALL --cap-add NET_BIND_SERVICE nginx:alpine
```

### REST

The BoxLite runtime REST API accepts `privileged` under `advanced` when creating
the box. The public path is `/v1/boxes` (the SDK's configured REST base may
already include `/v1`):

```http
POST /v1/boxes
Content-Type: application/json
```

```json
{
  "image": "docker:dind",
  "advanced": {
    "privileged": true
  }
}
```

Capability-only requests use the same `advanced` object:

```json
{
  "image": "alpine:latest",
  "advanced": {
    "capabilities": {
      "add": ["NET_ADMIN"],
      "drop": ["NET_RAW"]
    }
  }
}
```

The runner/cloud control API exposes the equivalent fields at the top level of
`POST /boxes`:

```json
{
  "id": "dind-01",
  "image": "docker:dind",
  "privileged": true
}
```

Its capability-only form is `"capabilities": {"add": [...], "drop": [...]}`.
The runner maps these flat fields to the same BoxLite advanced-options policy.

### SDKs

Python and Node use `AdvancedBoxOptions(privileged=True)` and
`advanced.privileged = true`, respectively. Rust, Go, and C expose the same
advanced option through their native builders/setters. Local SDK REST uses the
nested `advanced` shape; cloud SDK requests are mapped to the runner's
equivalent top-level fields. `privileged` is not an experimental feature flag.

## Meaning of `privileged`

`privileged=true` selects one complete guest-side security profile. The guest
resolves it to all capabilities supported by the running guest kernel and then
uses that resolved policy for the container init process and every later
`exec` process.

The profile opens these permissions and resources:

| Area | Effect in a privileged BoxLite container |
| --- | --- |
| Linux capabilities | The guest capability ceiling (`CAP_LAST_CAP`) is placed in OCI `bounding`, `permitted`, and `effective` sets. This includes `CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, `CAP_MKNOD`, `CAP_SYS_PTRACE`, and every other capability supported by that guest kernel. |
| OCI system paths | `readonly_paths` and `masked_paths` are empty. The ordinary restrictions on `/proc/bus`, `/proc/fs`, `/proc/irq`, `/proc/sys`, `/proc/sysrq-trigger`, and the runtime's masked system paths are removed. |
| `/sys` | The standard guest `/sys` bind is mounted read-write instead of read-only. |
| cgroups | A private cgroup namespace is added. The guest cgroup2 hierarchy exposed at `/sys/fs/cgroup` is writable. |
| devices | The guest device cgroup gets one allow-all `rwm` rule. This allows privileged processes to use or create guest-local device nodes, subject to the devices that actually exist in the guest. |
| root filesystem | The container root filesystem remains read-write, as it is for the normal container path. `privileged` does not change image or volume handling. |

The following are deliberately **not** implied by `privileged`:

- Host devices are not automatically passed through. Device requests are still
  resolved from and constrained to the guest's `/dev` tree; the flag does not
  expose a host disk, GPU, audio device, or arbitrary host node.
- Host filesystems and the host mount namespace are not exposed. The VM remains
  between the container and the host.
- The BoxLite outer jailer, VMM security policy, and host-side seccomp policy are
  not disabled by this flag. Those controls remain governed by the separate
  `advanced.security` option and the platform.
- Network mode is unchanged. `privileged` does not mean host networking.

## `privileged` versus `cap_add` / `cap_drop`

Capabilities and the privileged profile are related but not interchangeable:

| Request | Capabilities | Guest shape |
| --- | --- | --- |
| No capability options | BoxLite's 14-capability baseline | Ordinary read-only system paths, read-only `/sys`, no private cgroup namespace |
| `cap_add=["ALL"]` | All capabilities supported by the guest kernel | Still the ordinary guest shape |
| `privileged=true` | All capabilities supported by the guest kernel | Full guest privileged shape described above |
| `cap_drop=["ALL"]`, `cap_add=["NET_BIND_SERVICE"]` | Only `NET_BIND_SERVICE` | Ordinary guest shape |

Rules:

1. `privileged` implies the capability equivalent of `cap_add=["ALL"]`.
2. `cap_add=["ALL"]` does **not** imply `privileged` and does not open system
   paths, `/sys`, cgroups, or device access.
3. `privileged` must not be combined with a non-empty `cap_add` or `cap_drop`.
   `--privileged --cap-drop ALL` is rejected instead of relying on an
   ambiguous precedence rule.
4. Capability names are case-insensitive and may use the optional `CAP_`
   prefix. `ALL` is supported in either list.
5. API and SDK layers validate the name format. The guest checks whether a
   well-formed capability is supported by its own kernel and rejects it when it
   cannot apply it.

The effective OCI capability set contains `bounding`, `permitted`, and
`effective`. Inheritable and ambient sets remain unset for both ordinary and
privileged containers; `privileged` does not broaden those propagation rules.

## Implementation Layers Changed by PR 646

The request crosses the stack as one policy:

1. **CLI:** `run` and `create` parse `--privileged`, `--cap-add`, and
   `--cap-drop`, then reject conflicting combinations.
2. **Public options:** `AdvancedBoxOptions` carries the high-level
   `privileged` flag. Capability names are normalized at the API/SDK boundary,
   and `privileged` is normalized to the canonical `add=["ALL"]`, empty-drop
   representation.
3. **REST and cloud mapping:** the nested `advanced` contract carries both the
   capability policy and `privileged`. The API validates syntax and conflict
   rules but does not maintain a second list of capabilities supported by the
   guest kernel.
4. **Host-to-guest contract:** the container initialization request carries the
   normalized capability policy and the privileged bit to the guest.
5. **Guest policy resolution:** the guest resolves the request once into a
   `ResolvedSecurityPolicy` containing the effective capability set and the
   privileged shape. Later lifecycle and exec code consumes that resolved
   policy instead of reinterpreting `add`/`drop` independently.
6. **OCI spec:** the guest applies the capability sets, path policy, `/sys`
   mount mode, cgroup namespace, and device cgroup rule described above.

## Docker Alignment

Docker documents `docker run --privileged` as enabling all Linux capabilities,
disabling the default seccomp/AppArmor confinement, granting all host devices,
and making `/sys` and cgroup mounts read-write. See the [Docker run reference]
and [Docker runtime privilege documentation].

| Docker `--privileged` behavior | BoxLite 646 | Alignment |
| --- | --- | --- |
| All Linux capabilities | All capabilities supported by the guest kernel | Aligned at the guest capability level |
| `/sys` read-write | Guest `/sys` is read-write | Aligned |
| Cgroup mounts read-write | Guest cgroup2 is read-write in a private cgroup namespace | Aligned in guest behavior; implementation is VM-specific |
| System path restrictions removed | OCI masked and readonly path lists are cleared | Aligned for the guest OCI spec |
| All devices | Allow-all device cgroup policy, but only guest-local devices can be used or created | Intentionally not aligned; no host device passthrough |
| Default seccomp disabled | BoxLite outer security policy remains enabled unless separately configured | Intentionally not aligned |
| AppArmor/SELinux confinement disabled | No container-level Docker LSM profile is toggled; host/VM policy remains | Intentionally not aligned |
| Host-level access | VM boundary remains in place | Intentionally not aligned |

Therefore the correct compatibility claim is: **BoxLite `privileged` is
Docker-aligned for the guest-side DinD operations, not a byte-for-byte clone of
Docker's host-container privilege escape semantics.** It provides the nested
Docker daemon with the capabilities, writable kernel interfaces, cgroups, and
guest device policy it needs while preserving the VM boundary.

[Docker run reference]: https://docs.docker.com/reference/cli/docker/container/run/#escalate-container-privileges---privileged
[Docker runtime privilege documentation]: https://docs.docker.com/engine/containers/run/#runtime-privilege-and-linux-capabilities

## Tests Covered

PR 646 covers the behavior at each boundary:

- **Guest capability tests:** capability parsing, optional `CAP_` prefix,
  `ALL`, kernel capability ceiling, named drops, and the rejection of
  `privileged` capability overrides.
- **Guest OCI spec tests:** ordinary versus privileged readonly/masked paths,
  writable `/sys`, cgroup namespace, allow-all device rule, guest device path
  validation, and capability propagation to TTY exec.
- **Core runtime tests:** privileged normalization, option validation, and
  rejection when an existing non-privileged box is reused for a privileged
  request.
- **CLI tests:** parsing and conflict rejection for `--privileged` with
  `--cap-add` or `--cap-drop`.
- **REST/API tests:** advanced-option normalization, mapper serialization,
  syntax validation, and conflict rejection without a duplicated guest
  capability allowlist.
- **SDK tests:** Python, Node, Go, and C option conversion and conflict
  handling.
- **Local E2E:** `apps/e2e/cases/test_privileged_options.py` creates one
  privileged box and one `cap_add=ALL` box, then verifies that both have all
  guest capabilities while only the privileged box can write sysctls and the
  guest cgroup hierarchy.
