# Build and run Henosis as an unprivileged, immutable container process.

FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release --no-default-features -p syntheos-server

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818
RUN apt-get update -q && apt-get install -y --no-install-recommends wget \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system henosis \
    && useradd --system --gid henosis --uid 10001 --create-home --home-dir /var/lib/henosis henosis
COPY --from=build /src/target/release/henosis /usr/local/bin/henosis
USER 10001:10001
WORKDIR /var/lib/henosis
ENV SYNTHEOS_ADDR=127.0.0.1:8088
EXPOSE 8088
HEALTHCHECK --interval=30s --timeout=3s --start-period=15s --retries=3 CMD wget -qO- http://127.0.0.1:8088/health | grep -qx ok
ENTRYPOINT ["/usr/local/bin/henosis"]
