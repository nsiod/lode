# Built by .github/workflows/docker.yml from prebuilt release binaries.
# The build context holds linux-amd64/lode and linux-arm64/lode (extracted from
# the GitHub release tarballs); buildx sets TARGETARCH per platform.
#
# Base: zzci/ubase — a general-purpose image (glibc, a shell, common tools), not a
# minimal/static one, so this same image can also host script apps whose runtime
# (bun/node/deno) lode downloads at boot into its runtime cache. lode itself is a
# musl-static binary that runs on any base, and its TLS roots are bundled
# (webpki-roots), so no system ca-certificates are required. Pin by digest
# (zzci/ubase@sha256:…) for reproducible image builds; must be multi-arch
# (linux/amd64 + linux/arm64) to match the platforms buildx targets below.
FROM zzci/ubase
ARG TARGETARCH
# lode is a multi-call binary: as `lode` it is the loader (the entrypoint); as
# `lode-cli` (same binary, different name) it is the operator/publisher toolkit,
# reachable via `docker exec <container> lode-cli status`. Both go on PATH at
# /usr/bin so downstream images and `docker exec` can call them by bare name.
COPY linux-${TARGETARCH}/lode /usr/bin/lode
COPY linux-${TARGETARCH}/lode /usr/bin/lode-cli
ENTRYPOINT ["/usr/bin/lode"]
