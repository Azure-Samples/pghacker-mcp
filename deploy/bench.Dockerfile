# The engine benchmark's image: the app image plus pghacker-replay and
# scripts/engine-bench.mjs, which a one-off execution of the probe job runs
# (deploy/README.md, "Commit history"). A repository of its own, so the app's
# image doesn't change. From the repository root, given the app image's digest:
#   az acr build --registry <acr> --platform linux/amd64 --file deploy/bench.Dockerfile \
#     --image pghacker-bench:<tag> \
#     --build-arg APP_IMAGE=<acr>.azurecr.io/pghacker-mcp@sha256:<digest> .
ARG APP_IMAGE
ARG RUST_IMAGE=rust:1.95-trixie

FROM ${RUST_IMAGE} AS rust
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY vendor vendor
COPY crates crates
COPY migrations migrations
COPY config config
RUN cargo build --release --locked -p pghacker-replay \
    && install -D -m 0755 -t /out target/release/pghacker-replay

FROM ${APP_IMAGE}
COPY --from=rust /out/pghacker-replay /usr/local/bin/pghacker-replay
COPY scripts/engine-bench.mjs scripts/engine-bench.mjs
CMD ["node", "scripts/engine-bench.mjs"]
