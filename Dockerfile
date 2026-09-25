# syntax=docker/dockerfile:1
#
# kioku server image: `docker build -t kioku .`
# Run: docker run -d -p 7391:7391 -e KIOKU_AUTH_TOKEN=$(openssl rand -hex 32) -v kioku-data:/data kioku
#
# Note: the build downloads the IPADIC dictionary (lindera-ipadic build script),
# so it needs network access.

FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
# Cache mounts keep the registry and target dir between builds (BuildKit).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p kioku-cli \
    && cp target/release/kioku /usr/local/bin/kioku

FROM debian:bookworm-slim
# git: the wiki is a git repository (kioku shells out to `git`).
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --create-home --shell /usr/sbin/nologin kioku \
    && mkdir -p /data \
    && chown kioku:kioku /data
COPY --from=build /usr/local/bin/kioku /usr/local/bin/kioku

# config.toml is optional: with KIOKU_AUTH_TOKEN set, `kioku serve` creates the
# data dir (wiki, db, index) on first start.
ENV KIOKU_DATA_DIR=/data \
    KIOKU_BIND=0.0.0.0
VOLUME /data
EXPOSE 7391
USER kioku
ENTRYPOINT ["kioku", "serve"]
