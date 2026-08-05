# frr-bootc: a bootc image that runs FRR as a router appliance, meant to be
# booted as a VM under OpenShift Virtualization (KubeVirt).
#
# FRR and network configuration are not baked into the image; they are
# mounted at runtime from Kubernetes ConfigMaps (via virtiofs) and kept in
# sync by frr-config-sync.{service,path} and network-config-sync.{service,path}.
# See README.md for the full architecture and deployment manifests.
ARG BASE_IMAGE=quay.io/fedora/fedora-bootc:42
FROM ${BASE_IMAGE}

RUN dnf -y install \
        frr \
        NetworkManager \
        nmstate \
        rsync \
        python3-pyyaml \
        util-linux \
    && dnf clean all

COPY files/etc/frr/daemons /etc/frr/daemons
COPY files/etc/frr/frr.conf /etc/frr/frr.conf
COPY files/etc/frr/vtysh.conf /etc/frr/vtysh.conf
COPY files/etc/sysctl.d/71-frr-bootc-forwarding.conf /etc/sysctl.d/71-frr-bootc-forwarding.conf

COPY files/usr/local/bin/frr-config-sync /usr/local/bin/frr-config-sync
COPY files/usr/local/bin/network-config-sync /usr/local/bin/network-config-sync
COPY files/usr/local/bin/frr-bootc-gen-links /usr/local/bin/frr-bootc-gen-links

COPY files/usr/lib/systemd/system/run-config-frr.mount /usr/lib/systemd/system/run-config-frr.mount
COPY files/usr/lib/systemd/system/run-config-network.mount /usr/lib/systemd/system/run-config-network.mount
COPY files/usr/lib/systemd/system/frr-config-sync.service /usr/lib/systemd/system/frr-config-sync.service
COPY files/usr/lib/systemd/system/frr-config-sync.path /usr/lib/systemd/system/frr-config-sync.path
COPY files/usr/lib/systemd/system/frr-bootc-ifnaming.service /usr/lib/systemd/system/frr-bootc-ifnaming.service
COPY files/usr/lib/systemd/system/network-config-sync.service /usr/lib/systemd/system/network-config-sync.service
COPY files/usr/lib/systemd/system/network-config-sync.path /usr/lib/systemd/system/network-config-sync.path

RUN chmod 0755 \
        /usr/local/bin/frr-config-sync \
        /usr/local/bin/network-config-sync \
        /usr/local/bin/frr-bootc-gen-links \
    && mkdir -p /run/config/frr /run/config/network /var/lib/frr-bootc \
    && chown -R frr:frr /etc/frr \
    && chmod -R u=rwX,g=rX,o= /etc/frr \
    && systemctl enable \
        frr.service \
        NetworkManager.service \
        frr-bootc-ifnaming.service \
        frr-config-sync.service \
        frr-config-sync.path \
        network-config-sync.service \
        network-config-sync.path

LABEL org.opencontainers.image.title="frr-bootc" \
      org.opencontainers.image.description="bootc image running FRR as a router appliance for OpenShift Virtualization (KubeVirt), with FRR and network config supplied via mounted ConfigMaps."
