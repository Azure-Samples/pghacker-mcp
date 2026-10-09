# syntax=docker/dockerfile:1.7
# pghacker-mcp for Azure Container Apps (deploy/README.md): the Rust
# services, PilotSwarm 0.8.2 with the app's agents, oauth2-proxy, and the
# supervisor that runs each as its own user (scripts/container.mjs).
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
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p pghacker-indexer -p pghacker-mcp \
    && install -D -m 0755 -t /out target/release/pghacker-indexer target/release/pghacker-mcp

# PilotSwarm's release tarballs, checked against scripts/lib/release.mjs's
# checksums, and the app's Node dependencies.
FROM ${NODE_IMAGE} AS node
WORKDIR /app
COPY package.json package-lock.json ./
COPY scripts/lib/release.mjs scripts/lib/release.mjs
RUN node --input-type=module -e "import { restoreRelease } from './scripts/lib/release.mjs'; await restoreRelease('/app');" \
    && npm ci --omit=dev --no-audit --no-fund \
    && npm cache clean --force

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
# The services' users: agents and PilotSwarm, the Rust services, the proxy.
RUN set -eu; \
    for spec in pgh-agent:10001 pgh-svc:10002 pgh-proxy:10003; do \
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
COPY --from=rust /out/ /usr/local/bin/
COPY --from=oauth2-proxy /out/oauth2-proxy /usr/local/bin/oauth2-proxy
ENV NODE_ENV=production
EXPOSE 8080 8081
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["node", "scripts/container.mjs"]
