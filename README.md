# frr-bootc

A [bootc](https://containers.github.io/bootc/) image that runs [FRR](https://frrouting.org/)
as a router appliance, meant to run as a VM under
**OpenShift Virtualization (KubeVirt)**.

FRR and network configuration are **not** baked into the image; they are
mounted into the VM at runtime from two Kubernetes `ConfigMaps` and
watched/applied automatically by systemd services.

For operating a full, multi-cluster fleet of these VMs as an ACI border-leaf
layer (per-tenant VRFs, BFD, persistent-IP worker peering, dedicated
migration network, ArgoCD `ApplicationSet`s, generated configs, CI
validation), see [`gitops/`](gitops/README.md) - this file documents the
`frr-bootc` image itself, `gitops/` documents how it's deployed at scale.

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
 frr-config-sync.path        network-config-sync.path
 (inotify on directory)      (inotify on directory)
        │                            │
        ▼                            ▼
 frr-config-sync.service     frr-bootc-ifnaming.service (MAC → name)
   → /etc/frr/*               network-config-sync.service
   → vtysh -C (validation)      → *.nmconnection to NetworkManager
   → frr-reload.py (live)       → nmstatectl apply *.yml/*.yaml
   → on daemons change:
     systemctl restart frr
```

Both config sources are mounted into the VM via **virtiofs** (not as a disk
image). That's the way KubeVirt intends `ConfigMap`, `Secret` and
`ServiceAccount` contents to be handed to a VM as plain files 1:1, without
needing a cloud-init ISO or a reboot.

### Why Two Separate Sync Paths?

- **FRR configuration** (`frr.conf`, `daemons`, `vtysh.conf`) is
  re-synced on every change. `frr.conf` changes are validated with
  `vtysh -C` and then applied live via `frr-reload.py` (no restart, no
  disruption of running sessions/adjacencies, as far as FRR allows for
  that). If `daemons` changes (e.g. `bgpd` gets enabled), restarting
  `frr.service` is unavoidable since that's what determines which daemon
  processes are actually running.
- **Network configuration** is applied in two steps:
  1. `frr-bootc-ifnaming.service` runs **before** `NetworkManager.service`
     and writes `.link` files from `interfaces.yaml`
     (`/etc/systemd/network/70-frr-bootc-<name>.link`), pinning each
     interface to a fixed name based on its MAC address.
  2. `network-config-sync.service` runs **after** `NetworkManager.service`
     and applies the actual configuration (nmstate or NetworkManager
     keyfiles).

  This split is necessary because interface renaming has to happen before
  NetworkManager starts, while `nmstatectl` requires a running
  NetworkManager.

## Configuration Format

### `frr-config` ConfigMap → `/run/config/frr`

Any files are mirrored 1:1 into `/etc/frr/`. In particular:

- `daemons` — which FRR daemons run (see the FRR docs)
- `frr.conf` — the actual routing configuration
- `vtysh.conf` — optional

See [`manifests/10-configmap-frr-config.yaml`](manifests/10-configmap-frr-config.yaml).

### `network-config` ConfigMap → `/run/config/network`

- `interfaces.yaml` (optional, but recommended) — MAC-address-to-name
  mapping, only for interfaces KubeVirt actually presents to the VM as a
  PCI/virtio device (i.e. the physical uplink/trunk and the LAN interface,
  not the per-tenant VLAN sub-interfaces created via nmstate):

  ```yaml
  interfaces:
    - mac: "02:00:00:12:34:01"
      name: eth-trunk
  ```

- `*.yml` / `*.yaml` (other than `interfaces.yaml`) — [nmstate](https://nmstate.io/)
  desired-state documents, applied via `nmstatectl apply`.
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

That's why the VM only has **two** additional interfaces, regardless of
tenant count:

- `eth-trunk` — a single NIC, attached via a Linux bridge
  `NetworkAttachmentDefinition` **without** a `vlan` field: this makes the
  bridge CNI plugin set up the VM's interface as a trunk port instead of
  tagging/untagging it to a fixed VLAN ID, so it passes 802.1Q-tagged
  frames through unchanged. Each tenant gets their own VLAN - not their
  own interface. Requires a node-side bridge with `vlan_filtering: true`
  (see the comment in the `NetworkAttachmentDefinition`).
- `eth-lan` — the internal, non-tenant-specific uplink interface.

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
even though they're physically attached to a different interface (in the
example: `eth-lan` in the default VRF) - two more building blocks are
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

`frr-config-sync.path` and `network-config-sync.path` pick up both changes
automatically and live - `nmstatectl apply` creates the new VLAN
sub-interface without disturbing the existing interfaces or other
tenants' running BGP sessions. At this scale, the ConfigMap contents are
best generated (Helm/Kustomize/your own script) rather than hand-maintained;
that changes nothing about the ConfigMaps' format itself.

## Adding a New Physical Interface Reliably

By contrast, adding a completely new physical NIC (e.g. a second trunk for
more bandwidth or additional uplink redundancy) is rare and does genuinely
require a VM restart, since KubeVirt doesn't hot-plug bridge interfaces.
The core problem here is that the order in which the guest sees new NICs
(and thus the kernel-assigned name, like `enp2s0`) isn't guaranteed to be
stable. That's why this image pins interface names to MAC addresses (see
above), and the workflow is deliberately two-pronged so both sides (VM
spec and guest configuration) match up exactly:

1. **Pick a MAC address.** Choose a fixed, unique MAC address for the new
   interface (e.g. from the locally administered range `02:xx:xx:xx:xx:xx`).
2. **Extend the VM spec:** In the `VirtualMachine`, add a new entry under
   `spec.template.spec.domain.devices.interfaces` with `name` and exactly
   that `macAddress`, plus the matching `multus.networkName` under
   `spec.template.spec.networks`
   (see [`manifests/20-networkattachmentdefinition.yaml`](manifests/20-networkattachmentdefinition.yaml)).
3. **Extend the `network-config` ConfigMap:** Add an entry with the same
   MAC address and the desired name to `interfaces.yaml`, and add the
   configuration for exactly that name to `nmstate.yml` (or an
   `*.nmconnection` file).
4. **Restart the VM.** Only then does `frr-bootc-ifnaming.service`, which
   runs before NetworkManager, kick in and rename the new interface before
   NetworkManager claims it.

This makes adding a physical interface reliable: the name inside the guest
depends solely on the MAC address explicitly set in the VM spec - not on
the PCI slot order in which KubeVirt attaches interfaces.

> Changes to already-existing interfaces (IP addresses, routing, new VLAN
> sub-interfaces on an existing trunk), on the other hand, are picked up
> **without a restart** via `network-config-sync.path`, live, through
> `nmstate.yml`/`*.nmconnection`.

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
     (the same `docker run --privileged` invocation as `build.sh`, just
     against the pushed image instead of a local one), wraps it via
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
$ oc apply -f manifests/20-networkattachmentdefinition.yaml   # if additional NICs are needed
$ oc apply -f manifests/30-virtualmachine.yaml                # adjust <registry>/... first
```

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

## Troubleshooting

- `oc logs`/console access to the VM, then inside the VM:
  `journalctl -u frr-config-sync.service -u network-config-sync.service -u frr-bootc-ifnaming.service`
- Currently applied configuration hash: `/var/lib/frr-bootc/*.sha256`
- FRR validation failures also end up in `/tmp/frr-config-check.log`
  inside the VM.
- `nmstatectl show` or `nmcli connection show` to check the current network
  state.
