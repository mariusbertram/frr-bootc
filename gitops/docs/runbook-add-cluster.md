# Runbook: Connect a New Cluster

Neither ApplicationSet (`argocd/applicationset-sites.yaml`,
`argocd/applicationset-cluster-infra.yaml`) is touched by this procedure -
that's the point of the cluster-generator × git-generator matrix.

## 1. Prepare the target cluster

- Install OpenShift Virtualization (CNV) with the virtiofs feature gate,
  the kubernetes-nmstate operator, and confirm OVN-Kubernetes supports the
  Layer2 secondary-network/persistent-IP feature you're running (see
  `gitops/README.md`'s "Requirements assumed of the target clusters").
- Label the nodes that should run FRR VMs: `frr-bootc.io/site=<site>` per
  datacenter, plus `frr-bootc.io/cluster=<cluster-name>` on every node that
  should carry live-migration traffic.
- Provision the isolated-CPU pool and 1Gi-hugepage pool those nodes need to
  actually satisfy `dedicatedCpuPlacement`/`hugepages` (via
  `PerformanceProfile`/`MachineConfig` - cluster-specific, out of this
  repo's scope).
- Create the node-side bridges the NNCPs in `base/nncp/` and
  `base/cluster-infra/migration-nncp.yaml` expect (or adjust the NIC/bridge
  names in the new site's `kustomization.yaml` patches to match your actual
  hardware).

## 2. Register the cluster with ArgoCD

```console
$ argocd cluster add <kubeconfig-context-name> --name <cluster-name>
```

Then add the two labels this repo's ApplicationSets require (ArgoCD's
`cluster add` doesn't set them) to the Secret it created in
`openshift-gitops` (find it via
`oc get secret -n openshift-gitops -l argocd.argoproj.io/secret-type=cluster`):

```console
$ oc label secret -n openshift-gitops <secret-name> \
    frr-bootc.io/enabled=true \
    frr-bootc.io/sync-policy=manual   # or "automated" - see argocd/cluster-secret-example.yaml
```

## 3. Add the cluster's data

```console
$ mkdir -p clusters/<cluster-name>/sites/<first-site>
```

`clusters/<cluster-name>/cluster.yaml`:

```yaml
cluster:
  name: <cluster-name>
  asn_range: {min: 65200, max: 65249}   # must not overlap any existing cluster's range
  default_image: registry.example.com/frr-bootc/frr-bootc-containerdisk:v1.4.0
  default_replicas: 1
  default_resources:
    cores: 4
    memory: 8Gi
    hugepage_size: 1Gi
  sync_policy: manual
```

`clusters/<cluster-name>/cluster-infra/kustomization.yaml` - copy an
existing one (e.g. `clusters/cluster-forge-b/cluster-infra/kustomization.yaml`)
and change the `frr-bootc.io/cluster` patch value.

`clusters/<cluster-name>/sites/<first-site>/values.yaml` and `tenants.yaml` -
copy the shape from an existing site (e.g.
`clusters/cluster-forge-b/sites/dc1/`), assign a `mac_base` not used by any
other site (MACs only need to be unique cluster-wide, but repo-wide
uniqueness avoids surprises if VMs ever move clusters).

```console
$ python3 generator/generate.py --site clusters/<cluster-name>/sites/<first-site>
$ python3 ci/validate.py   # confirms the new asn_range doesn't collide with existing clusters
```

## 4. Commit and merge

Open a PR with the new `clusters/<cluster-name>/` tree (including
`generated/`). CI validates it exactly like any tenant change (see
`docs/runbook-add-tenant.md`, step 4) but now also across the new cluster.

Once merged, both ApplicationSets discover the new cluster (via the
`frr-bootc.io/enabled` label from step 2) and site (via the git generator's
glob over `clusters/<cluster-name>/sites/*`) on their next reconciliation
and create the matching `Application` objects automatically - no
`ApplicationSet` edit, no ArgoCD restart.

## 5. Staged rollout of a new bootc image version

Bump `default_image` (or a single site's `values.yaml` `image:` override)
on the staging/test cluster first (`sync_policy: automated` clusters catch
regressions fastest), verify, then repeat the same one-line change on
production clusters (`sync_policy: manual`, so it needs an explicit
`argocd app sync` there). This is the same mechanism as any other config
change - a `containerDisk` image swap on an existing `VirtualMachine`
requires a VM restart (KubeVirt doesn't live-swap boot images), so expect
one controlled restart per replica, not a live reload like tenant changes.
