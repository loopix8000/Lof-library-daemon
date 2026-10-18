FROM rust:1-slim-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY desktop/src-tauri ./desktop/src-tauri
COPY desktop/ui ./desktop/ui
RUN cargo build --release --locked -p lof-library-server \
    && mkdir -p /out/data \
    && cp target/release/lof-library-server /out/

FROM debian:bookworm-slim

ENV LOF_DATA_DIR=/data
ENV LOF_WEB_ADDR=0.0.0.0:8787
EXPOSE 8787

RUN groupadd --gid 10001 lof \
    && useradd --uid 10001 --gid lof --no-create-home lof
COPY --from=build /out/lof-library-server /usr/local/bin/lof-library-server
COPY --from=build --chown=lof:lof /out/data /data

USER lof
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD ["lof-library-server", "healthcheck"]
ENTRYPOINT ["lof-library-server"]
