# frr-bootc: a bootc image that runs FRR as a router appliance, meant to be
# booted as a VM under OpenShift Virtualization (KubeVirt).
#
# FRR and network configuration are not baked into the image; they are
# mounted at runtime from Kubernetes ConfigMaps (via virtiofs) and kept in
# sync by frr-config-sync.service and network-config-sync.service - each
# a persistent daemon (Type=simple, Restart=always, started once at boot)
# that polls its ConfigMap on its own internal ~2min interval
# (config_sync::POLL_INTERVAL), not a systemd .timer anymore. Still a
# plain poll, deliberately not inotify (a "*.path" unit watching the
# virtiofs mount): virtiofs doesn't reliably propagate the host-side
# atomic symlink swap kubelet uses to update a mounted ConfigMap as an
# inotify event into the guest, so relying on it left both triggering
# unreliably in practice. Both always re-apply in full rather than trying
# to skip unchanged content - cheap and safe to re-run every ~2min, and
# unlike a skip, can't get stuck treating a config change as "already
# applied" when it wasn't (see config-sync/src for the full control
# flow). Each writes a one-line status file after every attempt
# (/run/{frr,network}-config-sync.status) that the console dashboard
# reads for "did the last sync succeed" - the daemon's own exit code no
# longer means that, since a single failed attempt just logs and retries
# next tick instead of exiting. bootc-image-sync.{service,path,timer}
# still uses the oneshot+.path+.timer combo (much longer 30min interval,
# and switching images is comparatively rare/heavyweight, so a missed
# inotify event there is a smaller/rarer cost than for these two).
# See README.md for the full architecture and deployment manifests.

# BASE_IMAGE has to stay declared here, before ANY "FROM" - an ARG's value
# is only visible to a later "FROM <image>" instruction's own image-name
# substitution when the ARG was declared before the *first* FROM in the
# file; declared after one (even in an earlier, unrelated build stage like
# console-builder below), it's scoped to that stage's instructions and
# doesn't carry into a later stage's FROM line, which resolved to a blank
# image name and failed the build ("base name (${BASE_IMAGE}) should not
# be blank") the one time this got tried the other way round.
ARG BASE_IMAGE=quay.io/fedora/fedora-bootc:44

# frr-console (the console dashboard - see console/src/main.rs, and the
# "Console Dashboard" section of README.md) is a separate Rust crate built
# on ratatui, compiled here in its own stage rather than installing a Rust
# toolchain into the final image. --target ...-musl links it fully static:
# a musl binary has no runtime libc of its own to version-mismatch against
# the final Fedora image's glibc, which sidesteps that question outright
# instead of relying on this builder's glibc happening to be old enough.
# frr-vty (see frr-vty/src/lib.rs) is a small, dependency-free crate
# implementing FRR's vty Unix-socket protocol directly, used by console
# for its own read-only FRR status queries - a `path = "../frr-vty"`
# dependency, so it needs copying alongside console/ at the same
# relative path its Cargo.toml expects. config-sync does NOT depend on
# it (frr.conf validation goes through `vtysh -C` as a subprocess
# instead - see config-sync/src/frr/mod.rs's `validate()` doc comment
# for why a vty-socket approach was tried there and reverted).
FROM docker.io/library/rust:1-alpine AS console-builder
RUN apk add --no-cache musl-dev gcc
WORKDIR /build
COPY frr-vty/ /frr-vty/
COPY console/ .
RUN cargo build --release --locked --target x86_64-unknown-linux-musl

# config-sync (frr-config-sync + network-config-sync - see config-sync/src,
# and the "Configuration Sync" section of README.md) replaces what used to
# be two plain bash scripts. It talks to nmstate directly via the
# `nmstate` Rust crate instead of shelling out to nmstatectl - see
# config-sync/src/network.rs. Same musl-static-build reasoning as
# console-builder above.
FROM docker.io/library/rust:1-alpine AS config-sync-builder
RUN apk add --no-cache musl-dev gcc
WORKDIR /build
COPY config-sync/ .
RUN cargo build --release --locked --target x86_64-unknown-linux-musl

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
        qemu-guest-agent \
    && dnf clean all

COPY files/etc/frr/daemons /etc/frr/daemons
COPY files/etc/frr/frr.conf /etc/frr/frr.conf
COPY files/etc/frr/vtysh.conf /etc/frr/vtysh.conf
COPY files/etc/sysctl.d/71-frr-bootc-forwarding.conf /etc/sysctl.d/71-frr-bootc-forwarding.conf
COPY files/etc/sysctl.d/72-frr-bootc-console-quiet.conf /etc/sysctl.d/72-frr-bootc-console-quiet.conf
COPY files/etc/sysctl.d/73-frr-bootc-vrf-strict-mode.conf /etc/sysctl.d/73-frr-bootc-vrf-strict-mode.conf
COPY files/etc/modules-load.d/vrf.conf /etc/modules-load.d/vrf.conf
# fedora-bootc doesn't include cloud-init's own image-mode drop-in (unlike
# quay.io/centos-bootc): growpart must target /sysroot, not /, since that's
# where the real root filesystem is mounted in image mode. See
# https://gitlab.com/fedora/bootc/examples/-/tree/main/cloud-init.
COPY files/etc/cloud/cloud.cfg.d/10-bootc.cfg /etc/cloud/cloud.cfg.d/10-bootc.cfg

COPY files/usr/local/bin/bootc-image-sync /usr/local/bin/bootc-image-sync
COPY files/usr/local/bin/frr-console-reset-faillock /usr/local/bin/frr-console-reset-faillock
COPY --from=console-builder /build/target/x86_64-unknown-linux-musl/release/frr-console /usr/local/bin/frr-console
COPY --from=config-sync-builder /build/target/x86_64-unknown-linux-musl/release/frr-config-sync /usr/local/bin/frr-config-sync
COPY --from=config-sync-builder /build/target/x86_64-unknown-linux-musl/release/network-config-sync /usr/local/bin/network-config-sync

COPY files/usr/lib/systemd/system/run-config-frr.mount /usr/lib/systemd/system/run-config-frr.mount
COPY files/usr/lib/systemd/system/run-config-network.mount /usr/lib/systemd/system/run-config-network.mount
COPY files/usr/lib/systemd/system/run-config-bootc.mount /usr/lib/systemd/system/run-config-bootc.mount
COPY files/usr/lib/systemd/system/frr-config-sync.service /usr/lib/systemd/system/frr-config-sync.service
COPY files/usr/lib/systemd/system/network-config-sync.service /usr/lib/systemd/system/network-config-sync.service
COPY files/usr/lib/systemd/system/bootc-image-sync.service /usr/lib/systemd/system/bootc-image-sync.service
COPY files/usr/lib/systemd/system/bootc-image-sync.path /usr/lib/systemd/system/bootc-image-sync.path
COPY files/usr/lib/systemd/system/bootc-image-sync.timer /usr/lib/systemd/system/bootc-image-sync.timer
COPY files/usr/lib/systemd/system/frr-console-tty1.service /usr/lib/systemd/system/frr-console-tty1.service
COPY files/usr/lib/systemd/system/frr-console-ttyS0.service /usr/lib/systemd/system/frr-console-ttyS0.service
COPY files/usr/lib/systemd/system/frr-console-reset-faillock.service /usr/lib/systemd/system/frr-console-reset-faillock.service

# frr-console-tty1.service/frr-console-ttyS0.service (enabled below) take
# over the console outright, Talos-Linux style - mask the getty units they
# replace so nothing (not the generator that would otherwise recreate
# serial-getty@ttyS0.service from the kernel's "console=ttyS0" argument,
# not a manual `systemctl start`) can bring the plain login prompt back on
# either tty. See the comment in each of those two unit files for the
# full picture, including how a real shell is still reachable (the 'b'
# key, via /bin/login).
RUN chmod 0755 \
        /usr/local/bin/frr-config-sync \
        /usr/local/bin/network-config-sync \
        /usr/local/bin/bootc-image-sync \
        /usr/local/bin/frr-console-reset-faillock \
        /usr/local/bin/frr-console \
    && mkdir -p /run/config/frr /run/config/network /run/config/bootc \
    && chown -R frr:frr /etc/frr \
    && chmod -R u=rwX,g=rX,o= /etc/frr \
    && systemctl mask getty@tty1.service serial-getty@ttyS0.service \
    && systemctl enable \
        frr.service \
        NetworkManager.service \
        auditd.service \
        cloud-init.target \
        qemu-guest-agent.service \
        frr-config-sync.service \
        network-config-sync.service \
        bootc-image-sync.service \
        bootc-image-sync.path \
        bootc-image-sync.timer \
        frr-console-reset-faillock.service \
        frr-console-tty1.service \
        frr-console-ttyS0.service

# A custom SELinux policy module, not a file-context label - see
# files/selinux/frr_console.te for the full story (short version: labeling
# frr-console itself as getty_exec_t was tried first and reverted, since
# it confines the *entire* dashboard process as getty_t for its whole
# lifetime, not just the moment of the 'b' handoff into /bin/login, which
# denied frr-console's own systemctl/ip/vtysh/bootc calls outright).
# selinux-policy-devel/make are build-only - removed again once the
# module's compiled and loaded, same reasoning as the Rust builder stage
# not shipping a Rust toolchain in the final image.
COPY files/selinux/frr_console.te /usr/local/src/frr_console.te
RUN dnf -y install selinux-policy-devel make \
    && make -C /usr/local/src -f /usr/share/selinux/devel/Makefile frr_console.pp \
    && semodule -i /usr/local/src/frr_console.pp \
    && rm -rf /usr/local/src/frr_console.te /usr/local/src/frr_console.pp /usr/local/src/tmp \
    && dnf -y remove selinux-policy-devel make \
    && dnf clean all

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
