# headroom-hook container image.
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
