# syntax=docker/dockerfile:1.7
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# pghacker-mcp for Azure Kubernetes Service (deploy/README.md): the Rust
# services, PilotSwarm 0.8.2 with the app's agents, the home page,
# oauth2-proxy and the token proxy. One image for every component: each pod
# (deploy/k8s) runs its own command, as its own user, under tini.
#   docker build -t pghacker-mcp .                                    (local check)
#   az acr run --registry <acr> --file deploy/acr-build.yaml --set TAG=<tag> .
# ACR Tasks' classic `az acr build` can't use the cache mounts: use the task.

ARG RUST_IMAGE=rust:1.95-trixie
ARG NODE_IMAGE=node:24-trixie-slim

# The services, built against trixie's glibc, which the runtime image has.
FROM ${RUST_IMAGE} AS rust
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY vendor vendor
COPY crates crates
COPY migrations migrations
COPY config config
# The skills pghacker-mcp serves, embedded in its binary.
COPY skills skills
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p pghacker-indexer -p pghacker-mcp \
    && install -D -m 0755 -t /out target/release/pghacker-indexer target/release/pghacker-mcp

# PilotSwarm's release tarballs, checked against scripts/lib/release.mjs's
# checksums, and the app's Node dependencies. The portal's SPA is patched to
# sign in through oauth2-proxy (scripts/lib/portal-spa.mjs).
FROM ${NODE_IMAGE} AS node
WORKDIR /app
COPY package.json package-lock.json ./
COPY scripts/lib/release.mjs scripts/lib/release.mjs
COPY scripts/lib/portal-spa.mjs scripts/lib/portal-spa.mjs
RUN node --input-type=module -e "import { restoreRelease } from './scripts/lib/release.mjs'; await restoreRelease('/app');" \
    && npm ci --omit=dev --no-audit --no-fund \
    && node --input-type=module -e "import { patchPortalSpa } from './scripts/lib/portal-spa.mjs'; patchPortalSpa('/app/node_modules/pilotswarm');" \
    && npm cache clean --force

# The public home page (home/): its React, Vite and TypeScript stay in this
# stage, and only the built page reaches the image.
FROM ${NODE_IMAGE} AS home
WORKDIR /build
COPY home/package.json home/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY home/ ./
RUN npm run build

# oauth2-proxy, checked against its release's checksums.
FROM ${NODE_IMAGE} AS oauth2-proxy
ARG TARGETARCH
ARG OAUTH2_PROXY_VERSION=7.15.5
ARG OAUTH2_PROXY_SHA256_AMD64=f63f94bf72c5f46ab002a0a275aa8b3cf19b4d828aed08a13978cb9a62c3a1fd
ARG OAUTH2_PROXY_SHA256_ARM64=ebef73d06a05f76a03b0b2f95aa8424c10201228bd64621a2a7d375ce44dd637
RUN set -eu; \
    case "${TARGETARCH}" in \
      amd64) sha="${OAUTH2_PROXY_SHA256_AMD64}" ;; \
      arm64) sha="${OAUTH2_PROXY_SHA256_ARM64}" ;; \
      *) echo "no oauth2-proxy for ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    name="oauth2-proxy-v${OAUTH2_PROXY_VERSION}.linux-${TARGETARCH}"; \
    node -e "fetch(process.argv[1]).then(async (r) => { if (!r.ok) throw new Error('HTTP ' + r.status); require('fs').writeFileSync('/tmp/o2p.tgz', Buffer.from(await r.arrayBuffer())); })" \
      "https://github.com/oauth2-proxy/oauth2-proxy/releases/download/v${OAUTH2_PROXY_VERSION}/${name}.tar.gz"; \
    echo "${sha}  /tmp/o2p.tgz" | sha256sum -c -; \
    tar -xzf /tmp/o2p.tgz -C /tmp; \
    install -D -m 0755 "/tmp/${name}/oauth2-proxy" /out/oauth2-proxy

FROM ${NODE_IMAGE}
# tini reaps and forwards signals; git fetches PostgreSQL's code and the
# client tools serve the probe and restore jobs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends tini git ca-certificates postgresql-client-17 \
    && rm -rf /var/lib/apt/lists/*
# The users the pods run as (scripts/lib/azure.mjs, USERS): PilotSwarm and
# its agents; the Rust services, the token proxy and the jobs; oauth2-proxy;
# the home page.
RUN set -eu; \
    for spec in pgh-agent:10001 pgh-svc:10002 pgh-proxy:10003 pgh-home:10004; do \
      name="${spec%%:*}"; id="${spec##*:}"; \
      groupadd --system --gid "${id}" "${name}"; \
      useradd --system --uid "${id}" --gid "${id}" --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "${name}"; \
    done
WORKDIR /app
COPY --from=node /app/node_modules node_modules
COPY package.json package-lock.json ./
COPY plugin plugin
COPY config config
COPY scripts scripts
COPY --from=home /build/dist home/dist
COPY --from=rust /out/ /usr/local/bin/
COPY --from=oauth2-proxy /out/oauth2-proxy /usr/local/bin/oauth2-proxy
ENV NODE_ENV=production
# No default command: each pod's manifest names its own, as args, so tini
# stays PID 1, forwarding signals and reaping the agents' shells' children.
ENTRYPOINT ["/usr/bin/tini", "--"]
