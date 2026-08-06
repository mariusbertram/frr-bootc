#!/usr/bin/env python3
"""Renders per-site nmstate/FRR ConfigMap content and VirtualMachine
manifests from the single source of truth (tenants.yaml + cluster.yaml +
site values.yaml).

Usage:
    generate.py --all
    generate.py --site clusters/cluster-forge-a/sites/dc1

Output is written to <site-dir>/generated/, which the site's
kustomization.yaml picks up via configMapGenerator (frr-config,
network-config) and as plain resources (virtualmachines.yaml). Re-running
this script is idempotent and produces no VM restart on its own - it only
changes ConfigMap content and (if replicas changed) VirtualMachine object
count, both of which the frr-bootc image and KubeVirt handle live/declaratively
(see repo root README.md).
"""
import argparse
import copy
import pathlib
import sys

import jinja2
import yaml

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
GITOPS_ROOT = REPO_ROOT
CLUSTERS_DIR = GITOPS_ROOT / "clusters"
TEMPLATES_DIR = pathlib.Path(__file__).resolve().parent / "templates"


def load_yaml(path: pathlib.Path):
    with open(path) as f:
        return yaml.safe_load(f) or {}


def deep_merge(base: dict, override: dict) -> dict:
    """override wins; nested dicts are merged, not replaced wholesale."""
    result = copy.deepcopy(base)
    for key, value in override.items():
        if isinstance(value, dict) and isinstance(result.get(key), dict):
            result[key] = deep_merge(result[key], value)
        else:
            result[key] = value
    return result


def mac_for(mac_base: str, offset: int) -> str:
    """mac_base is a 5-octet prefix like '02:00:00:d1:00'; the 6th octet is
    derived from the offset so trunk/lan MACs never collide across replicas
    within a site."""
    return f"{mac_base}:{offset:02x}"


def build_vms(cluster: dict, site: dict) -> list:
    replicas = site.get("replicas", cluster.get("default_replicas", 1))
    mac_base = site["mac_base"]
    vms = []
    for i in range(replicas):
        vms.append(
            {
                "index": i,
                "name": f"frr-bootc-{site['name']}-{i}",
                "mac_trunk": mac_for(mac_base, i * 2),
                "mac_lan": mac_for(mac_base, i * 2 + 1),
            }
        )
    return vms


def default_route_table_id(vlan_id: int) -> int:
    return 1000 + int(vlan_id)


def validate_site(cluster: dict, site: dict, tenants: list, site_dir: pathlib.Path):
    errors = []
    asn_range = cluster.get("asn_range")
    seen_vlans = set()
    for tenant in tenants:
        if "route_table_id" not in tenant:
            tenant["route_table_id"] = default_route_table_id(tenant["vlan_id"])
        if tenant["vlan_id"] in seen_vlans:
            errors.append(
                f"{site_dir}: duplicate vlan_id {tenant['vlan_id']} "
                f"(tenant {tenant['name']})"
            )
        seen_vlans.add(tenant["vlan_id"])
        if asn_range:
            if not (asn_range["min"] <= tenant["local_asn"] <= asn_range["max"]):
                errors.append(
                    f"{site_dir}: tenant {tenant['name']} local_asn "
                    f"{tenant['local_asn']} outside cluster asn_range "
                    f"{asn_range}"
                )
    if errors:
        raise SystemExit("\n".join(errors))


def render_site(site_dir: pathlib.Path, env: jinja2.Environment):
    cluster_dir = site_dir.parents[1]
    cluster = load_yaml(cluster_dir / "cluster.yaml")["cluster"]
    site = load_yaml(site_dir / "values.yaml")["site"]
    site = deep_merge(
        {
            "resources": cluster.get("default_resources", {}),
            "image": cluster.get("default_image"),
        },
        site,
    )
    tenants_doc = load_yaml(site_dir / "tenants.yaml")
    tenants = tenants_doc.get("tenants", [])

    validate_site(cluster, site, tenants, site_dir)

    vms = build_vms(cluster, site)
    ctx = {"cluster": cluster, "site": site, "tenants": tenants, "vms": vms}

    out_frr = site_dir / "generated" / "frr-config"
    out_net = site_dir / "generated" / "network-config"
    out_vm = site_dir / "generated" / "virtualmachines"
    for d in (out_frr, out_net, out_vm):
        d.mkdir(parents=True, exist_ok=True)

    (out_frr / "daemons").write_text(env.get_template("daemons.j2").render(ctx))
    (out_frr / "frr.conf").write_text(env.get_template("frr.conf.j2").render(ctx))
    (out_frr / "vtysh.conf").write_text(env.get_template("vtysh.conf.j2").render(ctx))

    (out_net / "interfaces.yaml").write_text(
        env.get_template("interfaces.yml.j2").render(ctx)
    )
    (out_net / "nmstate.yml").write_text(
        env.get_template("nmstate.yml.j2").render(ctx)
    )

    (out_vm / "virtualmachines.yaml").write_text(
        env.get_template("virtualmachine.yaml.j2").render(ctx)
    )

    print(f"generated {site_dir.relative_to(GITOPS_ROOT)}/generated "
          f"({len(tenants)} tenants, {len(vms)} VM replicas)")


def all_site_dirs():
    for cluster_dir in sorted(CLUSTERS_DIR.iterdir()):
        if not cluster_dir.is_dir():
            continue
        sites_dir = cluster_dir / "sites"
        if not sites_dir.is_dir():
            continue
        for site_dir in sorted(sites_dir.iterdir()):
            if (site_dir / "values.yaml").exists():
                yield site_dir


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--all", action="store_true", help="render every registered cluster/site")
    group.add_argument("--site", type=pathlib.Path, help="render a single site dir, e.g. clusters/cluster-forge-a/sites/dc1")
    args = parser.parse_args()

    env = jinja2.Environment(
        loader=jinja2.FileSystemLoader(str(TEMPLATES_DIR)),
        trim_blocks=True,
        lstrip_blocks=True,
        undefined=jinja2.StrictUndefined,
    )

    if args.all:
        site_dirs = list(all_site_dirs())
        if not site_dirs:
            print("no clusters/*/sites/* with values.yaml found", file=sys.stderr)
            sys.exit(1)
    else:
        site_dirs = [args.site.resolve()]

    for site_dir in site_dirs:
        render_site(site_dir, env)


if __name__ == "__main__":
    main()
