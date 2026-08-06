# headroom-hook container image — DEPRECATED / NOT CURRENTLY BUILDABLE.
#
# This Dockerfile describes the RETIRED pre-dlopen-ABI architecture: a standalone
# `headroom-hook` binary serving busbar's old Unix-socket hook wire. Since "Port
# headroom-hook to busbar's signed dlopen plugin ABI" (commit c37b798), this crate
# builds ONLY a cdylib (`crate-type = ["cdylib", "rlib"]`, no `[[bin]]`, no `fn main`)
# that busbar dlopen's in-process — there is no more standalone binary for this
# Dockerfile's `COPY binaries/${TARGETARCH}/headroom-hook /headroom-hook` line to copy,
# and no more socket for it to serve. `cargo build --release` against this repo's
# current Cargo.toml does not produce a `headroom-hook` executable at all, so
# .github/workflows/docker.yml (which builds this image) cannot succeed as written —
# its automatic `push: tags: v*` trigger has been removed for exactly this reason (see
# that workflow's header).
#
# For "one container, zero config, busbar + headroom together" today, use
# getbusbar/busbar-headroom instead (docker/bundle/Dockerfile,
# .github/workflows/docker-bundle.yml) — see README.md's "Install and run" section.
# Alternatively, release.yml's signed plugin tarball (drop into an existing busbar's
# plugins.dir) is the other real release path — see README.md "Install and run".
# Whether the standalone image concept is worth reviving (e.g. as a from-source dlopen
# sidecar loader) or should simply be deleted is still open; this header documents why
# the file as it stands no longer describes anything buildable.
#
# ---- Everything below this point is the ORIGINAL (stale) socket-transport content,
# ---- left as-is / historical reference. Do not trust it against the current code.
#
# Unlike busbar (a static musl binary on FROM scratch), this hook links a C++
# runtime: its dependency headroom-core pulls the ONNX Runtime crate (`ort`)
# through its default tree, which needs glibc + libstdc++ even though the
# TextCrusher path this hook uses is pure BM25 and loads no model at runtime
# (`ort` is `load-dynamic`, so nothing ML is dlopen'd unless actually used).
# The right long-term shrink is upstream — feature-gate `ort` out of a
# TextCrusher-only build — after which this could move to FROM scratch too.
# Until then, distroless/cc is the small, shell-less, glibc+libstdc++ base.
#
# The per-arch binaries are built on native runners in CI (.github/workflows/
# docker.yml) and copied in; buildx selects the right one via TARGETARCH. The
# runner's toolchain (glibc 2.38 / libstdc++ CXXABI 1.3.15 on ubuntu-24.04) is
# NEWER than debian 12 (glibc 2.36), so the base MUST be debian 13 (trixie,
# glibc 2.40) or the binary aborts at load with a `GLIBC_2.38 not found` error.
#
# Run standalone:
#   docker run --rm -v busbar-sock:/run/busbar getbusbar/headroom-hook
# (busbar mounts the same volume and connects to /run/busbar/headroom.sock —
#  see docker-compose.yml for the one-command "just works" setup.)
# Seed /run/busbar OWNED BY the nonroot runtime user (65532). When docker first
# mounts the shared named volume here, it inherits this ownership — so the hook,
# running as 65532, can create the socket. Without this the volume is root-owned
# and the hook aborts with EACCES ("Permission denied") on bind.
FROM busybox:latest AS prep
RUN mkdir -p /run/busbar && chown 65532:65532 /run/busbar

FROM gcr.io/distroless/cc-debian13

ARG TARGETARCH
COPY binaries/${TARGETARCH}/headroom-hook /headroom-hook
COPY --from=prep --chown=65532:65532 /run/busbar /run/busbar

# In a container the socket lives on a volume shared with busbar; the bare
# binary still defaults to /tmp/headroom.sock when this env is unset.
ENV HEADROOM_SOCKET=/run/busbar/headroom.sock

USER 65532:65532
ENTRYPOINT ["/headroom-hook"]
