# syntax=docker/dockerfile:1.7

FROM --platform=linux/amd64 docker.io/library/rust:1.97.1-bookworm@sha256:e544a8ee0b93bb2ddc8c67a80606f040998eff3847e4deed988d0874559f52a8 AS chef

WORKDIR /build
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    cargo install cargo-chef --version 0.1.71 --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY migrations/ migrations/
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    cargo chef cook --locked --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY migrations/ migrations/
COPY src/ src/
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --locked --release

ARG SOURCE_DATE_EPOCH=0
RUN install -D -m 0555 \
        target/release/github_webhook_exporter \
        /out/usr/local/bin/github_webhook_exporter \
    && install -d -m 0700 -o 65532 -g 65532 \
        /out/var/lib/github-webhook-exporter \
    && find /out -exec \
        touch --no-dereference --date="@${SOURCE_DATE_EPOCH}" -- {} +

FROM --platform=linux/amd64 gcr.io/distroless/cc-debian12:nonroot@sha256:471dbca9cad607b9a32c10e9c31fb09ffaeb2d460e0afbff86c27abbc80b1b98

COPY --from=builder /out/ /
WORKDIR /var/lib/github-webhook-exporter
USER 65532:65532
EXPOSE 8080/tcp
ENTRYPOINT ["/usr/local/bin/github_webhook_exporter"]
