# syntax=docker/dockerfile:1
#
# kioku server image (SPEC-M3.3 §2): ghcr.io/misorafa/kioku:<tag> and :latest, for
# linux/amd64 and linux/arm64. No Rust toolchain: the image packages a release binary.
#
# The build context holds one binary per platform at `<os>/<arch>/kioku`
# (`linux/amd64/kioku`, `linux/arm64/kioku`). release.yml extracts them from the release's
# musl tarballs and builds both platforms with `docker buildx`; CI builds the image from a
# locally compiled binary:
#
#   mkdir -p ctx/linux/amd64 && cp target/release/kioku ctx/linux/amd64/
#   docker build -f Dockerfile -t kioku ctx
#
# Run (the token is generated on first `init`, or provisioned with KIOKU_AUTH_TOKEN):
#   docker run -d --name kioku -p 7391:7391 -e KIOKU_AUTH_TOKEN=$(openssl rand -hex 32) \
#     -v kioku-data:/data ghcr.io/misorafa/kioku
# The image never updates itself (immutable; `kioku serve` only logs a newer release):
# update with `docker compose pull && docker compose up -d`, or watchtower (README "Docker").

# trixie: glibc 2.41, so a glibc build from a current CI runner runs here too (the release
# image uses the static musl binaries, which run anywhere).
FROM debian:trixie-slim

ARG TARGETPLATFORM

# git: the wiki is a git repository (kioku shells out to `git`). curl: the health check.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --create-home --shell /usr/sbin/nologin kioku \
    && mkdir -p /data \
    && chown kioku:kioku /data

COPY --chmod=0755 ${TARGETPLATFORM}/kioku /usr/local/bin/kioku

# config.toml is optional: with KIOKU_AUTH_TOKEN set, `kioku serve` creates the data dir
# (wiki, db, index) on first start. KIOKU_CONTAINER=1 tells kioku it runs in this image:
# no self-update, and `kioku init` prints the token it generates once.
ENV KIOKU_DATA_DIR=/data \
    KIOKU_BIND=0.0.0.0 \
    KIOKU_CONTAINER=1
VOLUME /data
EXPOSE 7391
USER kioku
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${KIOKU_PORT:-7391}/api/v1/health" >/dev/null || exit 1
ENTRYPOINT ["kioku"]
CMD ["serve"]
