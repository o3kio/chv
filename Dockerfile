# CHV all-in-one image — DEV / LAB CONVENIENCE ONLY
# =================================================
# This image is NOT the qualified deployment path. It is fenced by
# docs/DEPLOYMENT-ARCHITECTURE.md §10 together with docker-compose.yml,
# docker-compose.prod.yml and deploy/entrypoint.sh: the whole surface predates
# the #323 ownership model and the prompt-02 authority cutover. For anything
# real use the .deb/.rpm packages (packaging/) or scripts/install.sh.
#
# What it is good for: exercising the four CHV daemons in a container for local
# iteration and for image-build smoke checks. It carries Cloud Hypervisor at the
# qualified pin so VM work is possible inside the container, but it provisions
# no TLS/mTLS material — the control plane fails closed without it, so the
# compose stacks cannot complete agent enrollment as configured.
#
# Usage: docker compose up -d
# Access: http://localhost:80 (localhost only; never expose this stack)

FROM rust:1.98.1-bookworm AS builder

# tonic-prost-build/prost-build 0.14 do not vendor a protoc: every
# gen/rust/*/build.rs shells out to the system compiler. aws-lc-sys (reached
# from chv-controlplane via rustls/object_store, verified with `cargo tree -i
# aws-lc-sys`) needs cmake at build time. The workspace has no openssl-sys in
# its build graph, so libssl-dev is deliberately not installed.
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
# Toolchain pin (rustup resolves it from the workspace root) and the version
# metadata read by cmd/*/build.rs as ../../VERSION.
COPY VERSION rust-toolchain.toml ./
COPY cmd/ cmd/
COPY crates/ crates/
COPY gen/ gen/
COPY proto/ proto/
# crates/chv-architecture-validate embeds this schema with include_str! at
# compile time, so it is a build input of chv-controlplane.
COPY docs/schemas/ docs/schemas/

RUN cargo build --release --locked \
    --bin chv-controlplane \
    --bin chv-agent \
    --bin chv-stord \
    --bin chv-nwd

# Frontend build
FROM node:22-bookworm-slim AS ui-builder

WORKDIR /build/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci --ignore-scripts
COPY ui/ .
RUN npx svelte-kit sync && npx vite build

# Runtime image
FROM debian:bookworm-slim

# Cloud Hypervisor qualified pin (v53.0, #448 campaign). These three ARGs move
# together with the pin in scripts/install.sh and with the requalification
# evidence in docs/evidence/vmm-requalification/v53.0/ — never bump the version
# without re-qualifying and updating BOTH digests. The download below aborts
# the build on any digest mismatch.
ARG CHV_CLOUD_HYPERVISOR_VERSION=53.0
ARG CHV_CLOUD_HYPERVISOR_SHA256=448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc
ARG CHV_REMOTE_SHA256=13f32ba952e6791fd901f2279be2055fbacc64005f96c42a8e90d58860df84a7

RUN apt-get update && apt-get install -y --no-install-recommends \
    nginx ca-certificates \
    dnsmasq iproute2 bridge-utils iptables \
    wget curl \
    tini \
    && rm -rf /var/lib/apt/lists/*

# Create service users (matches scripts/install.sh). debian:bookworm-slim has
# no kvm group of its own (udev creates it on a full host), so create it when
# missing before adding chv to it. The chv-stord group is required by the
# qualified ownership contract (#323): storage dirs are chv:chv-stord 0770,
# and chv-stord itself runs as the 'chv' user (see packaging/scripts/postinstall.sh).
RUN groupadd -r chv && \
    groupadd -r chv-stord && \
    useradd -r -g chv -s /usr/sbin/nologin -M chv && \
    useradd -r -g chv-stord -s /usr/sbin/nologin -M chv-stord && \
    (getent group kvm >/dev/null || groupadd -r kvm) && \
    usermod -aG kvm,chv-stord chv

# Cloud Hypervisor + ch-remote at the qualified pin, verified exactly like
# scripts/install.sh does (sha256sum -c against the pinned digests).
RUN set -eux; \
    base="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v${CHV_CLOUD_HYPERVISOR_VERSION}"; \
    curl -fsSL "${base}/cloud-hypervisor-static" -o /tmp/cloud-hypervisor; \
    echo "${CHV_CLOUD_HYPERVISOR_SHA256}  /tmp/cloud-hypervisor" | sha256sum -c -; \
    install -m 0755 /tmp/cloud-hypervisor /usr/local/bin/cloud-hypervisor; \
    rm -f /tmp/cloud-hypervisor; \
    curl -fsSL "${base}/ch-remote-static" -o /tmp/ch-remote; \
    echo "${CHV_REMOTE_SHA256}  /tmp/ch-remote" | sha256sum -c -; \
    install -m 0755 /tmp/ch-remote /usr/local/bin/ch-remote; \
    rm -f /tmp/ch-remote; \
    ln -sf /usr/local/bin/cloud-hypervisor /usr/bin/cloud-hypervisor; \
    ln -sf /usr/local/bin/ch-remote /usr/bin/ch-remote; \
    cloud-hypervisor --version

COPY --from=builder /build/target/release/chv-controlplane /usr/local/bin/
COPY --from=builder /build/target/release/chv-agent /usr/local/bin/
COPY --from=builder /build/target/release/chv-stord /usr/local/bin/
COPY --from=builder /build/target/release/chv-nwd /usr/local/bin/

# Frontend
COPY --from=ui-builder /build/ui/build /opt/chv/ui

# Nginx config
COPY docs/examples/nginx/chv-ui.conf /etc/nginx/sites-enabled/default

# Migrations
COPY cmd/chv-controlplane/migrations /usr/local/share/chv/migrations

# Runtime dirs with the qualified ownership (packaging/scripts/postinstall.sh):
# storage is chv:chv-stord 0770 because chv-stord runs as 'chv' and the VMM
# (spawned by chv-agent as 'chv') must read/write volume files; the stord
# runtime dir is chv:chv 0755.
RUN mkdir -p /run/chv/controlplane /run/chv/agent /run/chv/stord /run/chv/nwd \
    /var/lib/chv/agent /var/lib/chv/storage/localdisk /var/lib/chv/storage/lvm \
    /var/lib/chv/images /var/lib/chv/cache \
    /etc/chv /etc/chv/certs && \
    chown -R chv:chv /var/lib/chv /run/chv/controlplane /run/chv/agent /run/chv/nwd && \
    chown -R chv:chv-stord /var/lib/chv/storage && \
    chmod 0770 /var/lib/chv/storage /var/lib/chv/storage/localdisk /var/lib/chv/storage/lvm && \
    chown chv:chv /run/chv/stord && \
    chmod 0755 /run/chv/stord && \
    chmod 775 /run/chv && \
    chmod 750 /var/lib/chv

# Default config (auto-generates jwt_secret on first run)
RUN printf '[database]\nurl = "sqlite:///var/lib/chv/controlplane.db"\nmigrations_dir = "/usr/local/share/chv/migrations"\n' > /etc/chv/controlplane.toml

# Entrypoint starts all services
COPY deploy/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 80 8080 8443 8444 9901

ENTRYPOINT ["/entrypoint.sh"]
