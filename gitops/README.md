# frr-bootc GitOps

ArgoCD-managed GitOps repository that operates a fleet of [`frr-bootc`](../README.md)
border-leaf VMs on KubeVirt/OpenShift Virtualization, across **multiple**
OpenShift clusters from **one** central ArgoCD instance.

Read this file first, then:

- [`docs/architecture.md`](docs/architecture.md) - diagram + where every
  architecture decision from the design brief is actually implemented
- [`docs/runbook-add-tenant.md`](docs/runbook-add-tenant.md) - add an ACI
  tenant/VLAN to an existing site
- [`docs/runbook-add-cluster.md`](docs/runbook-add-cluster.md) - connect a
  new OpenShift cluster to this repo

## Layout

```
gitops/
├── base/                        # shared shape, no concrete values
│   ├── namespace.yaml
│   ├── nad/                     # trunk + worker-peering NAD (per-site values)
│   ├── nncp/                    # host-side bridges for the above (per-site values)
│   ├── vm/virtualmachine-example.yaml   # annotated, standalone example VM
│   ├── cluster-infra/           # HyperConverged patch + migration NAD/NNCP
│   │   │                        # (cluster-scoped, not per-site)
│   └── kustomization.yaml
├── generator/                   # single source of truth -> generated artifacts
│   ├── generate.py
│   ├── templates/*.j2           # nmstate.yml, frr.conf, daemons, interfaces.yaml, VM(s)
│   └── requirements.txt
├── clusters/
│   ├── cluster-forge-a/
│   │   ├── cluster.yaml         # ASN range, default image/replicas/resources, sync policy
│   │   ├── cluster-infra/       # kustomization.yaml -> base/cluster-infra (1x per cluster)
│   │   └── sites/
│   │       ├── dc1/
│   │       │   ├── values.yaml      # site overrides (replicas, resources, MACs, LAN subnet)
│   │       │   ├── tenants.yaml     # <-- the single source of truth for this site
│   │       │   ├── kustomization.yaml
│   │       │   └── generated/       # committed output of generate.py, NOT hand-edited
│   │       └── dc2/ (same shape)
│   └── cluster-forge-b/ (same shape, different values - see cluster.yaml)
├── argocd/
│   ├── appproject.yaml
│   ├── applicationset-sites.yaml         # cluster x site matrix
│   ├── applicationset-cluster-infra.yaml # cluster only
│   └── cluster-secret-example.yaml
├── ci/
│   ├── validate.py              # cross-cluster ASN/prefix overlap, schema, `vtysh -C`
│   ├── schema/*.json
│   └── requirements.txt
└── docs/
```

Two example clusters are checked in as real, buildable data -
`cluster-forge-a` (production, 2 sites: `dc1`/`dc2`, ASN range 65000-65099,
manual sync) and `cluster-forge-b` (staging, 1 site: `dc1`, ASN range
65100-65149, automated sync) - see their `cluster.yaml` files for the full
diff in replicas/resources/image.

## How it fits together

1. **`tenants.yaml`** (per site) is the only file you edit for day-to-day
   tenant changes. `cluster.yaml` (per cluster) and `values.yaml` (per site)
   hold everything that varies by cluster/site but isn't tenant data:
   ASN range, image, replica count, resource profile, MAC base, LAN subnet.
2. **`generator/generate.py`** renders those into `nmstate.yml`, `frr.conf`
   (+`daemons`/`vtysh.conf`/`interfaces.yaml`), and N `VirtualMachine`
   manifests per site, written to `clusters/<cluster>/sites/<site>/generated/`.
   This output is **committed to git**, not rendered at apply-time - ArgoCD
   only ever reads plain YAML, so nothing about the sync mechanism below
   changes.
3. **`kustomization.yaml`** (per site) combines `base/` with that site's
   `generated/` output; `configMapGenerator` with
   `disableNameSuffixHash: true` keeps the `frr-config`/`network-config`
   ConfigMaps at fixed names (see "Why fixed ConfigMap names" below).
4. **ArgoCD `ApplicationSet`s** (`argocd/applicationset-sites.yaml`,
   `argocd/applicationset-cluster-infra.yaml`) turn every registered cluster
   × every `clusters/<cluster>/sites/*` directory into one `Application`
   each, fully automatically - see `docs/runbook-add-cluster.md`.
5. Inside the guest, the `frr-bootc` image's own sync services
   (`frr-config-sync.path`/`network-config-sync.path`) pick up the
   ConfigMap changes live - see the [repo root README](../README.md) for
   that half of the mechanism. **None of the above ever requires a VM
   restart for a tenant change.**

## Why fixed ConfigMap names

Kustomize's `configMapGenerator` defaults to suffixing a content hash onto
the generated ConfigMap's name so that *Pod-based* workloads referencing it
get rolled (new Pod, new ConfigMap name, old one eventually pruned). A
`VirtualMachine` is not a Pod-based workload with that rollout model, and
`spec.volumes[].configMap.name` is not in kustomize's built-in set of
"objects whose ConfigMap references get renamed automatically" - so
`disableNameSuffixHash: true` (set in every site's `kustomization.yaml`) is
required, not a style choice, to keep the fixed `frr-config`/`network-config`
names the VM/README's sync services expect.

## Local usage

```console
$ pip install -r generator/requirements.txt -r ci/requirements.txt

# after editing tenants.yaml/values.yaml/cluster.yaml:
$ python3 generator/generate.py --site clusters/cluster-forge-a/sites/dc1
$ git add clusters/cluster-forge-a/sites/dc1/generated
$ git commit -m "..."

# before opening a PR (CI runs the same checks, see
# ../.github/workflows/gitops-ci.yml):
$ python3 generator/generate.py --all
$ python3 ci/validate.py
$ kustomize build clusters/cluster-forge-a/sites/dc1
```

## Requirements assumed of the target clusters

- OpenShift Virtualization (CNV) with the virtiofs feature gate, matching
  the [repo root README](../README.md)'s deploy prerequisites.
- `nmstate`/kubernetes-nmstate operator installed (for the
  `NodeNetworkConfigurationPolicy` objects in `base/nncp/`).
- OVN-Kubernetes with the Layer2 secondary-network/persistent-IP feature
  available (for `base/nad/peering.yaml`).
- Node labels `frr-bootc.io/site=<site>` (and, for cluster-infra,
  `frr-bootc.io/cluster=<cluster>`) applied out-of-band to the nodes that
  should run FRR VMs / provide migration bandwidth.
- A registered isolated-CPU + 1Gi-hugepage pool on those nodes, matching
  each site's `resources` in `values.yaml`.

These are cluster-preparation steps, not something this GitOps repo itself
provisions - see `docs/runbook-add-cluster.md`.
