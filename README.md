# frr-bootc

A [bootc](https://containers.github.io/bootc/) image that runs [FRR](https://frrouting.org/)
as a router appliance, meant to run as a VM under
**OpenShift Virtualization (KubeVirt)**.

FRR and network configuration are **not** baked into the image; they are
mounted into the VM at runtime from two Kubernetes `ConfigMaps` and
watched/applied automatically by systemd services.

For operating a full, multi-cluster fleet of these VMs as an ACI border-leaf
layer (per-tenant VRFs, BFD, worker peering, dedicated migration network,
ArgoCD `ApplicationSet`s, generated configs, CI validation), see the
separate [`mariusbertram/frr-argo`](https://github.com/mariusbertram/frr-argo)
repo - this file documents the `frr-bootc` image itself, `frr-argo`
documents how it's deployed at scale.

## Architecture

```
                         OpenShift / KubeVirt
                         ─────────────────────
 ConfigMap "frr-config"      ConfigMap "network-config"
        │                            │
        │ virtiofs                  │ virtiofs
        ▼                            ▼
 /run/config/frr (ro)        /run/config/network (ro)
        │                            │
        ▼                            ▼
 frr-config-sync.timer       network-config-sync.timer
 (2min poll - the only       (2min poll - the only
  trigger; see below)         trigger; see below)
        │                            │
        ▼                            ▼
 frr-config-sync.service     network-config-sync.service
   → /etc/frr/*                 → *.nmconnection to NetworkManager
   → vtysh -C (validation)      → nmstatectl apply *.yml/*.yaml
   → frr-reload.py (live)
   → on daemons change:
     systemctl restart frr
```

Both config sources are mounted into the VM via **virtiofs** (not as a disk
image). That's the way KubeVirt intends `ConfigMap`, `Secret` and
`ServiceAccount` contents to be handed to a VM as plain files 1:1, without
needing a cloud-init ISO or a reboot.

Both sync services are triggered purely by a 2-minute `systemd` timer, not
by an inotify `*.path` unit watching the mount for changes - an earlier
version used both, but the `*.path` trigger proved unreliable in practice:
virtiofs doesn't reliably propagate the host-side atomic symlink swap
kubelet uses to update a mounted `ConfigMap` as an inotify event into the
guest, so it would sometimes just never fire. A plain periodic poll is
simpler and, empirically, more reliable - the cost is that a change can
take up to ~2 minutes to land instead of being near-instant, which is a
non-issue for planned changes like onboarding a new tenant.

### Why Two Separate Sync Paths?

- **FRR configuration** (`frr.conf`, `daemons`, `vtysh.conf`) is
  re-synced on every change. `frr.conf` changes are validated with
  `vtysh -C` and then applied live via `frr-reload.py` (no restart, no
  disruption of running sessions/adjacencies, as far as FRR allows for
  that). If `daemons` changes (e.g. `bgpd` gets enabled), restarting
  `frr.service` is unavoidable since that's what determines which daemon
  processes are actually running.
- **Network configuration** (`nmstate.yml`/`*.nmconnection`) is applied by
  `network-config-sync.service`, which runs **after** `NetworkManager.service`
  since `nmstatectl` requires a running NetworkManager. The VM's single
  network device is identified by its fixed `macAddress` and named
  "eth-trunk" by `nmstatectl apply` itself, as part of this same sync step
  (see "A Trunk Instead of One NIC per Tenant" below) - no separate naming
  step to sequence around.

## Configuration Format

### `frr-config` ConfigMap → `/run/config/frr`

Any files are mirrored 1:1 into `/etc/frr/`. In particular:

- `daemons` — which FRR daemons run (see the FRR docs)
- `frr.conf` — the actual routing configuration
- `vtysh.conf` — optional

See [`manifests/10-configmap-frr-config.yaml`](manifests/10-configmap-frr-config.yaml).

### `network-config` ConfigMap → `/run/config/network`

- `*.yml` / `*.yaml` — [nmstate](https://nmstate.io/) desired-state
  documents, applied via `nmstatectl apply`. The VM's one and only network
  device is identified by `identifier: mac-address`/`mac-address:` against
  its fixed `macAddress` (set in the `VirtualMachine` spec) and named
  "eth-trunk" by nmstate itself as part of the same apply (see "A Trunk
  Instead of One NIC per Tenant" below), so every other VLAN sub-interface
  in the same document can just refer to `eth-trunk` directly as
  `base-iface`.
- `*.nmconnection` — raw NetworkManager keyfiles, installed into
  `/etc/NetworkManager/system-connections/` and activated.

Both formats (nmstate and NetworkManager keyfiles) can be used at the same
time - it just depends on which file extension the respective keys in the
ConfigMap have.

See [`manifests/11-configmap-network-config.yaml`](manifests/11-configmap-network-config.yaml).

## A Trunk Instead of One NIC per Tenant

A dedicated physical/Multus NIC per tenant doesn't scale: with, say, 150
BGP-coupled tenants the VM would need 150 additional interfaces, and every
new interface requires a VM restart (KubeVirt can't hot-plug bridge
interfaces) - which defeats the goal of being able to add a tenant with
nothing more than a ConfigMap commit.

That's why the VM has **exactly one** network device, regardless of
tenant count, and no separate pod/masquerade "default" network either:

- `eth-trunk` — the VM's only NIC, attached via a Linux bridge
  `NetworkAttachmentDefinition` **without** a `vlan` field: this makes the
  bridge CNI plugin set up the VM's interface as a trunk port instead of
  tagging/untagging it to a fixed VLAN ID, so it passes 802.1Q-tagged
  frames through unchanged. Requires a node-side bridge with
  `vlan_filtering: true` (see the comment in the
  `NetworkAttachmentDefinition`).

A fixed, deterministic `macAddress` is set for it in the `VirtualMachine`
spec, and `network-config`'s `nmstate.yml` identifies the device by that
same MAC (`identifier: mac-address`, `mac-address: <...>`) and names it
"eth-trunk" as part of the normal `nmstatectl apply` done by
`network-config-sync.service` at boot - no udev `.link` file, no
initramfs rebuild step, nothing that has to run before the real root
filesystem is even mounted. Whoever generates the `VirtualMachine`
manifest and `nmstate.yml` (in this repo's examples that's
`mariusbertram/frr-argo`'s `generator/generate.py`) just needs to put the
*same* MAC in both places - see its `replica_mac()` for a scheme that
derives one deterministically per replica instead of hand-picking/pinning
one.

Every tenant gets their own VLAN sub-interface on top of `eth-trunk` -
never their own interface - and so does the internal, non-tenant-specific
`eth-lan` network, previously used for management/default-VRF access: it's
the same kind of VLAN sub-interface, just left in the default VRF instead
of being enslaved into a tenant one (see "VRF per Tenant" below). Adding
tenant #151 never touches the VM spec or the NAD, only `nmstate.yml`.
Reach the VM itself via `virtctl console` (serial, not networked) or
whichever VLAN's IP you configure for management.

See [`manifests/20-networkattachmentdefinition.yaml`](manifests/20-networkattachmentdefinition.yaml)
for the trunk `NetworkAttachmentDefinition` and
[`manifests/30-virtualmachine.yaml`](manifests/30-virtualmachine.yaml) for
the VM side.

## VRF per Tenant and Policy-Based Routing

Every tenant gets its own VLAN sub-interface on `eth-trunk` in
`nmstate.yml`, plus its own VRF that sub-interface is enslaved into
(`vrf-tenant1`, `vrf-tenant2`, …), so its routing table - and the BGP
session running inside it - is fully isolated from the other tenants and
from the default VRF:

```yaml
interfaces:
  - name: vrf-tenant1
    type: vrf
    state: up
    vrf:
      port:
        - tenant1
      route-table-id: 1001
  - name: tenant1
    type: vlan
    state: up
    vlan:
      base-iface: eth-trunk
      id: 100
    ipv4: { ... }
```

Correspondingly, `frr.conf` runs a separate BGP instance per tenant
(`router bgp <ASN> vrf vrf-tenant1`, with `neighbor ... bfd` for fast
failure detection - see `bfdd=yes` in `daemons`), which advertises the
networks intended for that VRF via a `network` statement.

For those networks to actually leave via the corresponding tenant VLAN -
even though they're physically attached to a different VLAN sub-interface
(in the example: `eth-lan`, itself a `type: vlan` sub-interface of
`eth-trunk` like any tenant's, just left in the default VRF) - two more
building blocks are
needed, also configured declaratively via `nmstate.yml` (**not** FRR's
`pbrd`: a `pbr-map` would have to be bound to one fixed ingress interface,
whereas an nmstate/kernel `route-rule` matches purely on source address,
independent of the interface - a much better-scaling approach once there
are many tenants):

1. A leaked route per tenant VRF (`routes.config`, with `table-id` set to
   the VRF's matching `route-table-id`), so the respective BGP instance's
   `network` statement actually has something to advertise:

   ```yaml
   routes:
     config:
       - destination: 10.0.0.0/24
         next-hop-address: 10.0.2.254
         next-hop-interface: eth-lan
         table-id: 1001
   ```

2. A policy routing rule per network (`route-rules.config`) that assigns
   packets to the matching VRF's routing table based on their source
   address:

   ```yaml
   route-rules:
     config:
       - ip-from: 10.0.0.0/24
         priority: 1000
         route-table: 1001
   ```

In short: the VRF isolates the BGP session, the leaked route lets BGP know
about the network, and the `route-rule` makes sure the actual forwarding
path for that network runs through the right VRF (and thus out the right
tenant VLAN). See
[`manifests/11-configmap-network-config.yaml`](manifests/11-configmap-network-config.yaml)
for the full example.

## Optional IPv6 BGP Peering

IPv6 peering is entirely opt-in, per tenant - nothing here is on by
default, and adding it to one tenant doesn't require it for any other.
`manifests/10-configmap-frr-config.yaml`/`11-configmap-network-config.yaml`
show tenant1 peering dual-stack (IPv4 + IPv6) while tenant2 stays
IPv4-only, so both cases are visible side by side. IPv6 forwarding is
already enabled unconditionally at the OS level
(`net.ipv6.conf.all.forwarding = 1` in
[`files/etc/sysctl.d/71-frr-bootc-forwarding.conf`](files/etc/sysctl.d/71-frr-bootc-forwarding.conf)),
so turning on IPv6 for a tenant only ever touches the two ConfigMaps, the
same as any other tenant change (see "Adding Tenants Without a VM Restart"
below):

1. **`network-config`'s `nmstate.yml`:** add an `ipv6:` block to the
   tenant's VLAN sub-interface, shaped exactly like its `ipv4:` block:

   ```yaml
   - name: tenant1
     type: vlan
     state: up
     vlan:
       base-iface: eth-trunk
       id: 100
     ipv6:
       enabled: true
       address:
         - ip: 2001:db8:100::2
           prefix-length: 127
   ```

   If the tenant advertises an IPv6 network, add the matching leaked
   `routes.config`/`route-rules.config` entries too (same shape as the
   IPv4 ones, just with IPv6 prefixes/addresses - see the file for the
   full example). IPv4 and IPv6 policy-routing rules live in separate
   kernel rule databases (`ip rule` vs. `ip -6 rule`), so their
   `priority` numbers don't need to stay globally unique across both.

2. **`frr-config`'s `frr.conf`:** add an IPv6 `neighbor` line and an
   `address-family ipv6 unicast` block to the tenant's existing `router
   bgp ... vrf ...` instance - it does **not** need a separate BGP
   instance:

   ```
   router bgp 65001 vrf vrf-tenant1
    bgp router-id 198.51.100.2
    neighbor 198.51.100.1 remote-as 65000
    neighbor 2001:db8:100::1 remote-as 65000
    !
    address-family ipv4 unicast
     network 10.0.0.0/24
    exit-address-family
    !
    address-family ipv6 unicast
     network 2001:db8:a::/64
    exit-address-family
   !
   ```

   FRR auto-activates a neighbor under the address family matching its
   own address (IPv6 neighbor → `address-family ipv6 unicast`, IPv4 → v4)
   without needing an explicit `neighbor ... activate` line. `bgp
   router-id` stays in IPv4 dotted-quad form either way - that's a BGP
   protocol requirement, not something IPv6-specific.

Both changes are picked up live via `frr-config-sync.timer`/
`network-config-sync.timer` (up to ~2min), same as any other tenant
change - no VM restart either way.

## Adding Tenants Without a VM Restart

This is the common case when scaling to many (e.g. ~150) tenants, and it
touches neither the `VirtualMachine` nor the `NetworkAttachmentDefinition`
- only the `network-config` and `frr-config` ConfigMaps:

1. Pick a free VLAN ID within the range allowed by the `trunk`
   `NetworkAttachmentDefinition` (`vlan.trunk`; widen it there if
   exhausted - that's the only step that touches the
   `NetworkAttachmentDefinition`, and even that needs no VM restart).
2. In `network-config`'s `nmstate.yml`, add the triplet of VLAN
   sub-interface (`type: vlan`, `base-iface: eth-trunk`, `vlan.id: <ID>`),
   VRF interface, and the matching `routes`/`route-rules` entries (see
   above).
3. In `frr-config`'s `frr.conf`, add the matching
   `router bgp ... vrf ...` instance for the new tenant.

`frr-config-sync.timer` and `network-config-sync.timer` pick up both
changes automatically (up to ~2min) - `nmstatectl apply` creates the new
VLAN sub-interface without disturbing the existing interfaces or other
tenants' running BGP sessions. At this scale, the ConfigMap contents are
best generated (Helm/Kustomize/your own script) rather than hand-maintained;
that changes nothing about the ConfigMaps' format itself.

## Adding a Second Physical Interface (Out of Scope by Default)

This image is built around having exactly one network device. Since
`eth-trunk` is already identified by its own fixed `macAddress` rather than
by an assumed kernel-given name (see above), adding a genuinely second
physical NIC (e.g. a second trunk for redundancy or extra bandwidth) isn't
a special case: pin a second `macAddress` in the `VirtualMachine` spec and
add a matching second `identifier: mac-address` interface entry to
`nmstate.yml` with its own name. Still needs a VM restart either way, since
KubeVirt doesn't hot-plug bridge interfaces.

> Changes to already-existing interfaces (IP addresses, routing, new VLAN
> sub-interfaces on the trunk), on the other hand, are picked up **without
> a restart** via `network-config-sync.timer` (up to ~2min), live, through
> `nmstate.yml`/`*.nmconnection`.

## bootc Image Tracking

A third ConfigMap, `bootc-config` → `/run/config/bootc`, configures which
OCI image this system tracks for in-place updates - not the wrapped
`containerDisk` image the VM boots from, but the plain bootc image
`.github/workflows/build.yml`'s `build` job publishes
(`ghcr.io/<owner>/<repo>`):

```yaml
image: ghcr.io/mariusbertram/frr-bootc:latest
```

`bootc-image-sync.service` runs `bootc switch "$image"` against it,
triggered both by `bootc-image-sync.path` (when this ConfigMap changes)
and by `bootc-image-sync.timer` (every 30 minutes) - unlike the frr/network
sync services, the interesting case here isn't just "did the config
change" but also "is there new content behind the same tag" (e.g. a fresh
build CI just pushed to `:latest`), which only `bootc` itself can
determine, so it's simplest to just always ask it. Either way, this only
**stages** the update - applying a staged update still needs a reboot,
which is deliberately not automated here: rebooting a router is an
operator/orchestration decision (e.g. a rolling reboot across the
redundant instances mentioned below), not something to do automatically
per VM. See [`manifests/12-configmap-bootc-config.yaml`](manifests/12-configmap-bootc-config.yaml).

For a VM booting from a CDI DataVolume
(`manifests/31-virtualmachine-datavolume.yaml`), this is also the more
common way to update a *running* instance in place, since (unlike a
`containerDisk` VM) it doesn't automatically pick up new content from
`DataImportCron` on restart - see the comment in that manifest.

## Notes on High Throughput and Redundancy

The example manifest is deliberately kept minimal; for production use with
high throughput or redundancy requirements, it deliberately lacks (and is
therefore only noted here, not implemented as a ready-made manifest):

- **Throughput:** `networkInterfaceMultiqueue: true` is already set. For
  very high throughput (e.g. in the double-digit Gbit/s range), you'd also
  want `spec.domain.cpu.dedicatedCpuPlacement`, hugepages
  (`spec.domain.memory.hugepages`), and possibly SR-IOV
  `NetworkAttachmentDefinition`s for the trunk/LAN interfaces instead of
  `bridge: {}` - this needs matching node resources (isolated CPUs, a
  hugepage pool, SR-IOV-capable NICs) and is therefore cluster-specific.
- **Redundancy:** The manifest shows a single `VirtualMachine`. An
  n-instance redundancy model (e.g. two VMs on different nodes, with BFD
  between them or to the tenants for fast failover) would need multiple
  `VirtualMachine` objects spread across nodes/availability zones via
  `podAntiAffinity` (on `kubevirt.io/domain`) - deliberately left out of
  scope for this example.

## Build

Requires `podman` with access to a privileged `bootc-image-builder` setup.

```console
$ ./build.sh [tag]
```

The script:

1. builds the bootc image from `Containerfile`,
2. converts it to a `qcow2` with `bootc-image-builder`,
3. wraps that `qcow2` as a minimal `containerDisk` image
   (`containerdisk/Containerfile`), the format KubeVirt expects for
   `spec...volumes[].containerDisk.image`.

Afterwards, push the `containerDisk` image to a registry reachable from the
cluster and reference it in the `VirtualMachine`.

### CI: Automatically Building Both Images

Both OCI artifacts are built and published in CI; locally, `./build.sh`
remains useful for ad-hoc builds outside CI:

- **GitHub Actions** ([`.github/workflows/build.yml`](.github/workflows/build.yml)),
  on every push to `main`, on tags (`v*.*.*`), and as a push-less build
  check on pull requests:
  1. `build`: builds the bootc image (`Containerfile`) with
     `docker/build-push-action` and pushes it to `ghcr.io/<owner>/<repo>`.
  2. `containerdisk` (skipped on pull requests): converts the digest-pinned
     bootc image just pushed into a `qcow2` disk with
     [`bootc-image-builder`](https://github.com/osbuild/bootc-image-builder)
     (the same `podman pull` + `podman run --privileged` sequence as
     `build.sh`, just against the pushed image instead of a local one),
     wraps it via
     `containerdisk/Containerfile`, and pushes it to
     `ghcr.io/<owner>/<repo>-containerdisk` - the image
     [`manifests/30-virtualmachine.yaml`](manifests/30-virtualmachine.yaml)
     and [`manifests/25-dataimportcron.yaml`](manifests/25-dataimportcron.yaml)
     reference.
- **GitLab CI** ([`.gitlab-ci.yml`](.gitlab-ci.yml)): builds the bootc
  image with [Kaniko](https://github.com/GoogleContainerTools/kaniko) (no
  privileged runner needed) and pushes it to the project's own container
  registry (`$CI_REGISTRY_IMAGE`) - on pushes to the default branch and on
  tags, as a push-less build check on merge requests (`--no-push`). No
  `containerdisk` job here, since `bootc-image-builder` needs a privileged
  runner that GitLab's shared runners don't provide - use `./build.sh`
  locally instead.

## Deploy

Requires OpenShift Virtualization with the virtiofs feature gate enabled
for arbitrary volumes (ConfigMap/Secret/ServiceAccount as a filesystem) -
see the comment in [`manifests/30-virtualmachine.yaml`](manifests/30-virtualmachine.yaml).

```console
$ oc apply -f manifests/00-namespace.yaml
$ oc apply -f manifests/10-configmap-frr-config.yaml
$ oc apply -f manifests/11-configmap-network-config.yaml
$ oc apply -f manifests/12-configmap-bootc-config.yaml
$ oc apply -f manifests/20-networkattachmentdefinition.yaml
$ oc apply -f manifests/30-virtualmachine.yaml                # adjust <registry>/... first
```

`cloud-init` is installed and enabled (with the `growpart`/`/sysroot` drop-in
image-mode systems need - see `files/etc/cloud/cloud.cfg.d/10-bootc.cfg`),
so an initial user/SSH key can be provisioned the usual way by adding a
`cloudInitNoCloud` volume to the `VirtualMachine` - it's not included in
the example manifests since FRR/network config already come from the two
ConfigMaps and don't need it.

### Alternative: CDI DataVolume Boot Source

Instead of pulling the `containerDisk` image fresh on every VM (re)start
(`manifests/30-virtualmachine.yaml`), you can let CDI (Containerized Data
Importer, part of OpenShift Virtualization) import it into a PVC once and
keep it updated automatically - the "golden image" pattern described in
Red Hat's
["Build and deploy image mode for RHEL on OpenShift Virtualization"](https://developers.redhat.com/articles/2024/11/11/deploy-image-mode-rhel-openshift-virtualization):

```console
$ oc apply -f manifests/25-dataimportcron.yaml         # instead of / in addition to 30-virtualmachine.yaml
$ oc apply -f manifests/31-virtualmachine-datavolume.yaml   # instead of 30-virtualmachine.yaml
```

`manifests/25-dataimportcron.yaml`'s `DataImportCron` polls the
`containerdisk` image's `:latest` tag on a schedule; whenever the CI
`containerdisk` job above pushes a new digest, it imports it into a fresh
PVC and repoints the managed `DataSource` at it. `VirtualMachine`s that
boot from that `DataSource` via `dataVolumeTemplates`
(`manifests/31-virtualmachine-datavolume.yaml`) restart faster (no re-pull)
and keep running even if the registry is briefly unreachable, at the cost
of not picking up a new image automatically on every restart the way the
plain `containerDisk` approach does - see the comment in that manifest for
the trade-off.

Make configuration changes afterwards simply via `oc edit configmap/frr-config`
or `oc edit configmap/network-config -n frr-bootc` - the sync services
inside the VM take care of the rest.

## Console Dashboard

This VM has no external dashboard/alerting (see the comment at the top of
`frr-config-sync`) - console access is the only way in. So rather than a
plain login prompt, the VM's two consoles each run a persistent status
dashboard (`/usr/local/bin/frr-console`), Talos-Linux style: it's there
before, without, and regardless of anyone logging in, and it comes right
back the moment they log back out.

`frr-console` is a small Rust crate ([`console/`](console/)) built on
[ratatui](https://ratatui.rs/), compiled in its own build stage in the
`Containerfile` (statically against musl, so its binary carries no runtime
libc dependency of its own to version-match against the final image's) and
copied into `/usr/local/bin/frr-console`. `ratatui::Terminal::draw` diffs
each frame against the last one and only touches the terminal cells that
actually changed, which is what makes redraws flicker-free *and* immune to
stale content bleeding through when a section's height changes between
refreshes (an interface's address line coming and going, a VRF's BGP peers
appearing/disappearing, ...) - a class of bug an earlier, hand-rolled
cursor-position/erase-sequence bash version of this dashboard had to
chase down one escape sequence at a time.

- `frr-console-tty1.service` - the graphical/VNC console (`virtctl vnc`)
- `frr-console-ttyS0.service` - the serial console (`virtctl console`)

Both replace `getty@tty1.service`/`serial-getty@ttyS0.service` outright -
masked in the Containerfile so nothing (not the getty generator, not a
manual `systemctl start`) can bring the plain login prompt back on either
tty - rather than sitting in front of them. **SSH is unaffected**: SSH
sessions use their own pty, never `/dev/tty1`/`/dev/ttyS0`, so they still
land in a plain shell exactly as before; this only changes the two
consoles KubeVirt exposes directly.

The dashboard auto-refreshes every 5s, showing:

- uptime, load, memory, disk
- network interfaces (`ip -brief addr`) with up/down state and live
  rx/tx throughput (a delta between two `/sys/class/net/*/statistics`
  samples, so it needs one redraw to warm up - "throughput: -" the first
  time an interface is seen)
- `frr.service` state, active daemons, and the IPv4 RIB route count
- established-vs-configured BGP peer counts **per VRF** (each tenant gets
  its own VRF and its own BGP instance - see "VRF per Tenant" above - so
  one tenant's session being down doesn't hide behind another's being
  fine), when `bgpd` is running
- health of the three sync services (`frr-config-sync`,
  `network-config-sync`, `bootc-image-sync`) - FAILED if a unit's last run
  failed, a warning if its timer isn't active, so a silently-broken sync
  is visible right on the console instead of only in `journalctl`
- the current `bootc status` (booted/staged image)

Viewing it needs no authentication - whoever already has console access to
the VM (via `virtctl`/`oc` RBAC) is already privileged enough that this
isn't new exposure. `b` execs a real `/bin/login` (the normal
"login:"/password prompt), so an actual shell is still gated on real
credentials. Exiting that shell ends the `login` process, which the owning
unit's `Restart=always` immediately answers by relaunching the dashboard
on the same tty - there's no separate "log out of the dashboard" step,
the shell exiting *is* that step.

To get a plain login prompt back on a given tty instead (e.g. while
debugging this mechanism itself), on the VM:

```console
$ systemctl disable --now frr-console-tty1.service
$ systemctl unmask getty@tty1.service
$ systemctl start getty@tty1.service
```

(swap in `frr-console-ttyS0.service`/`serial-getty@ttyS0.service` for the
serial console).

## Troubleshooting

- `oc logs`/console access to the VM, then inside the VM:
  `journalctl -u frr-config-sync.service -u network-config-sync.service -u bootc-image-sync.service`
  (or just look at the "Sync Services" section of the console dashboard -
  see "Console Dashboard" above)
- Both `frr-config-sync` and `network-config-sync` run on a 2min timer only
  (no inotify) and always re-apply, whether or not the ConfigMap actually
  changed - `vtysh -C`/`rsync`/`frr-reload.py`/`nmstatectl apply`/`nmcli`
  are all cheap and safe to re-run, so expect a change to take up to ~2min
  to land, not immediately.
- FRR validation failures also end up in `/tmp/frr-config-check.log`
  inside the VM.
- `nmstatectl show` or `nmcli connection show` to check the current network
  state.
- If the trunk NIC shows up as `enp3s0` (or another kernel-assigned name)
  instead of `eth-trunk`, check that the `macAddress` in the
  `VirtualMachine` spec actually matches the `mac-address:` nmstate
  identifies `eth-trunk` by in `network-config`'s `nmstate.yml` - see "A
  Trunk Instead of One NIC per Tenant" above. A mismatch there (e.g. one
  side regenerated without the other) means nmstate can't find the device
  it's supposed to rename.
