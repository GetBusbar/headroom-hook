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
# docker.yml) and copied in; buildx selects the right one via TARGETARCH.
#
# Run standalone:
#   docker run --rm -v busbar-sock:/run/busbar getbusbar/headroom-hook
# (busbar mounts the same volume and connects to /run/busbar/headroom.sock —
#  see docker-compose.yml for the one-command "just works" setup.)
FROM gcr.io/distroless/cc-debian12

ARG TARGETARCH
COPY binaries/${TARGETARCH}/headroom-hook /headroom-hook

# In a container the socket lives on a volume shared with busbar; the bare
# binary still defaults to /tmp/headroom.sock when this env is unset.
ENV HEADROOM_SOCKET=/run/busbar/headroom.sock

USER 65532:65532
ENTRYPOINT ["/headroom-hook"]
