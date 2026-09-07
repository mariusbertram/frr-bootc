# frr-bootc: a bootc image that runs FRR as a router appliance, meant to be
# booted as a VM under OpenShift Virtualization (KubeVirt).
#
# FRR and network configuration are not baked into the image; they are
# mounted at runtime from Kubernetes ConfigMaps (via virtiofs) and kept in
# sync by frr-config-sync.{service,timer} and
# network-config-sync.{service,timer} - a periodic poll (every 2min) is
# the ONLY trigger for both, deliberately not inotify (a "*.path" unit
# watching the virtiofs mount): virtiofs doesn't reliably propagate the
# host-side atomic symlink swap kubelet uses to update a mounted
# ConfigMap as an inotify event into the guest, so relying on it left
# both scripts triggering unreliably in practice. Both scripts always
# re-apply in full rather than trying to skip unchanged content - cheap
# and safe to re-run every 2min, and unlike a skip, can't get stuck
# treating a config change as "already applied" when it wasn't (see each
# script's own comment). bootc-image-sync.{service,path,timer} still
# uses the .path+.timer combo (much longer 30min interval, and switching
# images is comparatively rare/heavyweight, so a missed inotify event
# there is a smaller/rarer cost than for these two).
# See README.md for the full architecture and deployment manifests.
ARG BASE_IMAGE=quay.io/fedora/fedora-bootc:44
FROM ${BASE_IMAGE}

RUN dnf -y install \
        frr \
        NetworkManager \
        nmstate \
        rsync \
        util-linux \
        policycoreutils \
        audit \
        cloud-init \
        tcpdump \
    && dnf clean all

COPY files/etc/frr/daemons /etc/frr/daemons
COPY files/etc/frr/frr.conf /etc/frr/frr.conf
COPY files/etc/frr/vtysh.conf /etc/frr/vtysh.conf
COPY files/etc/sysctl.d/71-frr-bootc-forwarding.conf /etc/sysctl.d/71-frr-bootc-forwarding.conf
# fedora-bootc doesn't include cloud-init's own image-mode drop-in (unlike
# quay.io/centos-bootc): growpart must target /sysroot, not /, since that's
# where the real root filesystem is mounted in image mode. See
# https://gitlab.com/fedora/bootc/examples/-/tree/main/cloud-init.
COPY files/etc/cloud/cloud.cfg.d/10-bootc.cfg /etc/cloud/cloud.cfg.d/10-bootc.cfg

COPY files/usr/local/bin/frr-config-sync /usr/local/bin/frr-config-sync
COPY files/usr/local/bin/network-config-sync /usr/local/bin/network-config-sync
COPY files/usr/local/bin/bootc-image-sync /usr/local/bin/bootc-image-sync
COPY files/usr/local/bin/frr-console /usr/local/bin/frr-console
COPY files/etc/profile.d/91-frr-console.sh /etc/profile.d/91-frr-console.sh

COPY files/usr/lib/systemd/system/run-config-frr.mount /usr/lib/systemd/system/run-config-frr.mount
COPY files/usr/lib/systemd/system/run-config-network.mount /usr/lib/systemd/system/run-config-network.mount
COPY files/usr/lib/systemd/system/run-config-bootc.mount /usr/lib/systemd/system/run-config-bootc.mount
COPY files/usr/lib/systemd/system/frr-config-sync.service /usr/lib/systemd/system/frr-config-sync.service
COPY files/usr/lib/systemd/system/frr-config-sync.timer /usr/lib/systemd/system/frr-config-sync.timer
COPY files/usr/lib/systemd/system/network-config-sync.service /usr/lib/systemd/system/network-config-sync.service
COPY files/usr/lib/systemd/system/network-config-sync.timer /usr/lib/systemd/system/network-config-sync.timer
COPY files/usr/lib/systemd/system/bootc-image-sync.service /usr/lib/systemd/system/bootc-image-sync.service
COPY files/usr/lib/systemd/system/bootc-image-sync.path /usr/lib/systemd/system/bootc-image-sync.path
COPY files/usr/lib/systemd/system/bootc-image-sync.timer /usr/lib/systemd/system/bootc-image-sync.timer

RUN chmod 0755 \
        /usr/local/bin/frr-config-sync \
        /usr/local/bin/network-config-sync \
        /usr/local/bin/bootc-image-sync \
        /usr/local/bin/frr-console \
    && chmod 0644 /etc/profile.d/91-frr-console.sh \
    && mkdir -p /run/config/frr /run/config/network /run/config/bootc \
    && chown -R frr:frr /etc/frr \
    && chmod -R u=rwX,g=rX,o= /etc/frr \
    && systemctl enable \
        frr.service \
        NetworkManager.service \
        auditd.service \
        cloud-init.target \
        frr-config-sync.service \
        frr-config-sync.timer \
        network-config-sync.service \
        network-config-sync.timer \
        bootc-image-sync.service \
        bootc-image-sync.path \
        bootc-image-sync.timer

# Images produced via bootc-image-builder from a container build can end up
# with SELinux file contexts that don't match the target policy (an
# overlay/container-build-tooling quirk, not specific to this image), which
# can manifest as AVC denials early in boot. Relabel at build time so the
# shipped image is already correct, and mark for a fallback relabel on
# first boot too. Separately, auditd is enabled above so audit records
# (e.g. systemd service start/stop) are consumed and logged normally
# instead of falling back to being echoed onto the console, where they can
# look like alarming SELinux errors even when they're not (res=success).
RUN restorecon -Rv / || true
RUN touch /etc/selinux/.autorelabel

LABEL org.opencontainers.image.title="frr-bootc" \
      org.opencontainers.image.description="bootc image running FRR as a router appliance for OpenShift Virtualization (KubeVirt), with FRR and network config supplied via mounted ConfigMaps."
