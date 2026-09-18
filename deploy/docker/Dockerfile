# syntax=docker/dockerfile:1
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /build/keeper
COPY keeper/Cargo.toml keeper/Cargo.lock keeper/explorer-schema.sql ./
COPY keeper/src/ ./src/
ARG CARGO_BUILD_JOBS=2
RUN cargo build --release --locked --jobs "$CARGO_BUILD_JOBS"

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 keeper && useradd --uid 10001 --gid 10001 --no-create-home keeper \
    && install -d -o 10001 -g 10001 -m 0700 /var/lib/d20dao /run/keeper-keys
COPY --from=build /build/keeper/target/release/d20dao-keeper /usr/local/bin/d20dao-keeper
COPY --chmod=0555 deploy/docker/entrypoint.sh deploy/docker/import-keys.sh /usr/local/bin/
COPY LICENSE /usr/share/doc/d20dao-keeper/LICENSE
USER 10001:10001
# Keep core runtime threads bounded even on hosts with hundreds of CPUs.
ENV TOKIO_WORKER_THREADS=4 KEEPER_DB=/var/lib/d20dao/keeper.sqlite
WORKDIR /var/lib/d20dao
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=15s --timeout=3s --start-period=30s --retries=2 CMD ["/usr/local/bin/d20dao-keeper", "health", "--max-age", "30"]
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["run"]
