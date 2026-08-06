# Runbook: Add a Tenant/VLAN

No manual intervention on any VM, ever - this touches data files only.

1. Pick a free `vlan_id` for the target site (unique within
   `clusters/<cluster>/sites/<site>/tenants.yaml` - check the existing file,
   `ci/validate.py` also catches a collision if you get it wrong) and an
   `local_asn` inside that cluster's `asn_range`
   (`clusters/<cluster>/cluster.yaml`).

2. Append an entry to
   `clusters/<cluster>/sites/<site>/tenants.yaml`:

   ```yaml
     - name: newtenant
       vlan_id: 103
       vrf: vrf-newtenant
       local_asn: 65004
       peer_asn: 65000
       local_ip: 198.51.100.14
       prefix_length: 30
       peer_ip: 198.51.100.13
       advertised_networks:
         - 10.20.4.0/24
       bfd:
         min_rx_ms: 150
         min_tx_ms: 150
         multiplier: 3
       graceful_restart: true
   ```

   See `ci/schema/tenants.schema.json` for every available field
   (`import_prefix_lists`/`export_prefix_lists`, `aggregate_networks`,
   custom `route_table_id`, ...).

3. Regenerate and commit:

   ```console
   $ python3 generator/generate.py --site clusters/<cluster>/sites/<site>
   $ git add clusters/<cluster>/sites/<site>/tenants.yaml \
             clusters/<cluster>/sites/<site>/generated
   $ git commit -m "Add newtenant to <cluster>/<site>"
   ```

4. Open a PR. CI (`ci/validate.py`, see
   `.github/workflows/gitops-ci.yml`) checks:
   - the new VLAN/ASN don't collide within the site or across clusters
   - the new `advertised_networks` don't overlap another tenant's in the
     same site
   - the regenerated `frr.conf` passes `vtysh -C`
   - the regenerated `nmstate.yml` matches its schema
   - `generated/` in your PR actually matches what the generator produces
     (i.e. you didn't forget step 3)

5. Merge. ArgoCD syncs the updated `frr-config`/`network-config` ConfigMaps
   (production clusters: `sync_policy: manual` in `cluster.yaml`, so this is
   an explicit `argocd app sync` after merge - see that cluster's Argo
   `Application`). Inside every VM in that site, `frr-config-sync.path` and
   `network-config-sync.path` pick up the change within seconds and apply
   it live - no VM restart, no disruption to other tenants' BGP sessions.

## Removing a tenant

Same procedure in reverse: delete the entry from `tenants.yaml`, regenerate,
commit, PR, merge. The sync services remove the corresponding VLAN/VRF/BGP
instance on the next reload.
