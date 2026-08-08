# syntax=docker/dockerfile:1.7

FROM --platform=linux/amd64 docker.io/library/rust:1.97.1-bookworm@sha256:e544a8ee0b93bb2ddc8c67a80606f040998eff3847e4deed988d0874559f52a8 AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY migrations/ migrations/
COPY src/ src/
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ghe-target,target=/build/target,sharing=locked \
    cargo build --locked --release \
    && install -D -m 0555 \
        target/release/github_webhook_exporter \
        /out/usr/local/bin/github_webhook_exporter \
    && install -d -m 0700 -o 65532 -g 65532 \
        /out/var/lib/github-webhook-exporter

FROM --platform=linux/amd64 gcr.io/distroless/cc-debian12:nonroot@sha256:471dbca9cad607b9a32c10e9c31fb09ffaeb2d460e0afbff86c27abbc80b1b98

COPY --from=builder /out/ /
WORKDIR /var/lib/github-webhook-exporter
USER 65532:65532
EXPOSE 8080/tcp
ENTRYPOINT ["/usr/local/bin/github_webhook_exporter"]
