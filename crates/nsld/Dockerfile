# syntax=docker/dockerfile:1.7
#
# Multi-arch image for the nsld tunnel server, published to Docker Hub
# by `.github/workflows/docker.yml`. **Production / CI only** — for
# local hacking use `Dockerfile.local` next to this file (single-arch,
# builds from source, runs without staging).
#
# We don't compile from source in here. The release workflow already
# produced static-musl Linux binaries; `docker.yml` downloads them from
# the just-published GitHub Release and stages them in the build
# context as:
#
#   linux-amd64/nsld
#   linux-arm64/nsld
#
# `TARGETARCH` is set automatically by buildx for each platform pass —
# one Dockerfile produces both arches with no Rust toolchain in sight,
# no QEMU-emulated compilation, ~2-minute multi-arch builds.

FROM alpine:3.22

ARG TARGETARCH

# Root CA bundle is required so nsld can hit:
#   - Let's Encrypt (ACME directory + finalize endpoints)
#   - the operator's DNS-01 webhook (e.g. dnsall.com)
#   - the operator's forward-auth endpoint
# tini becomes PID-1 — reaps zombies and forwards SIGTERM/SIGINT.
RUN apk add --no-cache ca-certificates tini \
    && update-ca-certificates

# musl-static binary is alpine-native, drops in unmodified.
COPY linux-${TARGETARCH}/nsld /usr/local/bin/nsld

# All persistent state lives under one directory. The binary defaults
# `state_dir` to `./data` under the current working directory, so this
# WORKDIR + VOLUME pair makes `docker run -v "$PWD/data:/data" ...`
# the standard mount.
WORKDIR /data
VOLUME ["/data"]

# Public HTTP (redirect or plain routing).
EXPOSE 80/tcp
# Public HTTPS (TLS).
EXPOSE 443/tcp
# QUIC tunnel plane (must match `[server].listen`).
EXPOSE 443/udp

# Defaults to root so nsld can bind 80/443 without extra capabilities.
# Operators who don't need privileged ports can override at run time
# with `--user 65534:65534` (Alpine's `nobody`).

ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/nsld"]
CMD ["serve"]
