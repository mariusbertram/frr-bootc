#!/usr/bin/env python3
"""CI gate for the GitOps repo. Runs across EVERY registered cluster/site,
not just the one changed in a PR, since ASN/prefix collisions are only
visible cluster-to-cluster:

1. JSON-schema validation of cluster.yaml / values.yaml / tenants.yaml
2. VLAN-ID uniqueness within a site
3. Tenant ASN falls inside its cluster's asn_range
4. asn_range disjointness between clusters
5. Advertised-prefix overlap between tenants within a site (route-rules are
   a flat, source-prefix-keyed list per site - see
   generator/templates/nmstate.yml.j2 - so an overlap there is a real
   routing ambiguity, unlike overlap between tenants in DIFFERENT VRFs,
   which is exactly what VRFs are for)
6. Cross-cluster ASN reuse combined with prefix overlap (fabric-level
   ambiguity if the same ACI fabric is reachable from both clusters) -
   error; ASN reuse alone - warning only
7. Renders every site's frr.conf/nmstate.yml (via generator/generate.py) and,
   best-effort, runs `vtysh -f frr.conf -C` (skipped with a warning if vtysh
   isn't on PATH - see ci/README or the GitHub Actions workflow, which runs
   this inside the frrouting/frr container image specifically so vtysh IS
   present)

Exit code is non-zero if any hard error was found.
"""
import ipaddress
import itertools
import pathlib
import shutil
import subprocess
import sys
import tempfile

import jsonschema
import yaml

CI_DIR = pathlib.Path(__file__).resolve().parent
GITOPS_ROOT = CI_DIR.parent
CLUSTERS_DIR = GITOPS_ROOT / "clusters"
SCHEMA_DIR = CI_DIR / "schema"

sys.path.insert(0, str(GITOPS_ROOT / "generator"))
import generate  # noqa: E402  (local import, path adjusted above)

ERRORS = []
WARNINGS = []


def error(msg):
    ERRORS.append(msg)
    print(f"ERROR: {msg}", file=sys.stderr)


def warn(msg):
    WARNINGS.append(msg)
    print(f"WARNING: {msg}", file=sys.stderr)


def load(path):
    with open(path) as f:
        return yaml.safe_load(f)


def validate_schema(doc, schema_path, source):
    schema = load(schema_path)
    try:
        jsonschema.validate(doc, schema)
    except jsonschema.ValidationError as e:
        error(f"{source}: schema validation failed: {e.message} (at {'/'.join(str(p) for p in e.path)})")


def discover():
    """Returns {cluster_name: {"cluster": dict, "sites": {site_name: {"site": dict, "tenants": list, "dir": Path}}}}"""
    result = {}
    for cluster_dir in sorted(CLUSTERS_DIR.iterdir()):
        cluster_yaml = cluster_dir / "cluster.yaml"
        if not cluster_yaml.exists():
            continue
        cluster_doc = load(cluster_yaml)
        validate_schema(cluster_doc, SCHEMA_DIR / "cluster.schema.json", cluster_yaml)
        cluster = cluster_doc["cluster"]
        sites = {}
        for site_dir in sorted((cluster_dir / "sites").iterdir()):
            values_yaml = site_dir / "values.yaml"
            tenants_yaml = site_dir / "tenants.yaml"
            if not values_yaml.exists():
                continue
            site_doc = load(values_yaml)
            validate_schema(site_doc, SCHEMA_DIR / "site.schema.json", values_yaml)
            tenants_doc = load(tenants_yaml)
            validate_schema(tenants_doc, SCHEMA_DIR / "tenants.schema.json", tenants_yaml)
            sites[site_doc["site"]["name"]] = {
                "site": site_doc["site"],
                "tenants": tenants_doc.get("tenants", []),
                "dir": site_dir,
            }
        result[cluster["name"]] = {"cluster": cluster, "sites": sites}
    return result


def networks_of(tenant):
    return [ipaddress.ip_network(n, strict=False) for n in tenant["advertised_networks"]]


def check_asn_ranges(registry):
    ranges = {name: data["cluster"]["asn_range"] for name, data in registry.items()}
    for (a, ra), (b, rb) in itertools.combinations(ranges.items(), 2):
        if ra["min"] <= rb["max"] and rb["min"] <= ra["max"]:
            error(f"cluster asn_range collision: {a} {ra} overlaps {b} {rb} "
                  f"(each cluster needs a disjoint ASN range)")


def check_site(cluster_name, cluster, site_name, tenants, site_dir):
    seen_vlans = {}
    for tenant in tenants:
        if tenant["vlan_id"] in seen_vlans:
            error(f"{site_dir}: vlan_id {tenant['vlan_id']} used by both "
                  f"{seen_vlans[tenant['vlan_id']]} and {tenant['name']}")
        seen_vlans[tenant["vlan_id"]] = tenant["name"]

        asn_range = cluster["asn_range"]
        if not (asn_range["min"] <= tenant["local_asn"] <= asn_range["max"]):
            error(f"{site_dir}: tenant {tenant['name']} local_asn "
                  f"{tenant['local_asn']} outside {cluster_name}'s asn_range {asn_range}")

    for t1, t2 in itertools.combinations(tenants, 2):
        for n1 in networks_of(t1):
            for n2 in networks_of(t2):
                if n1.overlaps(n2):
                    error(f"{site_dir}: advertised network overlap between "
                          f"{t1['name']} ({n1}) and {t2['name']} ({n2}) - "
                          f"route-rules are a flat source-prefix list per "
                          f"site, this is ambiguous PBR, not VRF isolation")


def check_cross_cluster(registry):
    all_tenants = []  # (cluster_name, site_name, tenant)
    for cname, cdata in registry.items():
        for sname, sdata in cdata["sites"].items():
            for tenant in sdata["tenants"]:
                all_tenants.append((cname, sname, tenant))

    for (c1, s1, t1), (c2, s2, t2) in itertools.combinations(all_tenants, 2):
        if c1 == c2:
            continue  # same-cluster overlap already covered by check_site
        if t1["local_asn"] != t2["local_asn"]:
            continue
        overlap = any(
            n1.overlaps(n2) for n1 in networks_of(t1) for n2 in networks_of(t2)
        )
        where = f"{c1}/{s1}/{t1['name']} (ASN {t1['local_asn']}) vs {c2}/{s2}/{t2['name']}"
        if overlap:
            error(f"cross-cluster ASN+prefix reuse: {where} - both reachable "
                  f"from the same fabric would make routes ambiguous")
        else:
            warn(f"cross-cluster ASN reuse without prefix overlap: {where} - "
                 f"double-check this is intentional")


def render_and_check_frr(site_dir):
    tmp = tempfile.mkdtemp(prefix="frr-bootc-ci-")
    try:
        import jinja2
        env = jinja2.Environment(
            loader=jinja2.FileSystemLoader(str(GITOPS_ROOT / "generator" / "templates")),
            trim_blocks=True,
            lstrip_blocks=True,
            undefined=jinja2.StrictUndefined,
        )
        cluster_dir = site_dir.parents[1]
        cluster = load(cluster_dir / "cluster.yaml")["cluster"]
        site = generate.deep_merge(
            {"resources": cluster.get("default_resources", {}), "image": cluster.get("default_image")},
            load(site_dir / "values.yaml")["site"],
        )
        tenants = load(site_dir / "tenants.yaml").get("tenants", [])
        generate.validate_site(cluster, site, tenants, site_dir)
        vms = generate.build_vms(cluster, site)
        ctx = {"cluster": cluster, "site": site, "tenants": tenants, "vms": vms}

        frr_conf = pathlib.Path(tmp) / "frr.conf"
        frr_conf.write_text(env.get_template("frr.conf.j2").render(ctx))

        vtysh = shutil.which("vtysh")
        if not vtysh:
            warn(f"{site_dir}: vtysh not on PATH, skipping frr.conf syntax check "
                 f"(the GitHub Actions workflow runs this step inside the "
                 f"frrouting/frr container, where vtysh is present)")
        else:
            proc = subprocess.run(
                [vtysh, "-f", str(frr_conf), "-C"],
                capture_output=True, text=True,
            )
            if proc.returncode != 0:
                error(f"{site_dir}: generated frr.conf failed `vtysh -C`:\n{proc.stdout}\n{proc.stderr}")

        nmstate_doc = yaml.safe_load(env.get_template("nmstate.yml.j2").render(ctx))
        validate_schema(nmstate_doc, SCHEMA_DIR / "nmstate.schema.json", f"{site_dir}/generated/network-config/nmstate.yml")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    registry = discover()
    check_asn_ranges(registry)
    for cname, cdata in registry.items():
        for sname, sdata in cdata["sites"].items():
            check_site(cname, cdata["cluster"], sname, sdata["tenants"], sdata["dir"])
            render_and_check_frr(sdata["dir"])
    check_cross_cluster(registry)

    print(f"\n{len(ERRORS)} error(s), {len(WARNINGS)} warning(s) across "
          f"{len(registry)} cluster(s)")
    sys.exit(1 if ERRORS else 0)


if __name__ == "__main__":
    main()
