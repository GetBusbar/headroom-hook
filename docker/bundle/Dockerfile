# getbusbar/busbar-headroom: busbar + this repo's headroom-hook plugin, bundled into ONE image.
#
# This is the "one container, zero config" convenience path for anyone who came to busbar
# specifically to run Headroom's prompt-compression rewrite gate: pull this image, set your
# provider key, and compression is already wired in — no separate plugin install step.
#
# It is DISTINCT from:
#   - getbusbar/busbar (busbarAI's own image): plugin-free, same treatment every plugin gets.
#     If you want a DIFFERENT plugin (or a custom set), start there and drop in tarballs
#     yourself (docs/plugins.md in busbarAI).
#   - getbusbar/headroom-hook (this repo's OWN image, see the root Dockerfile): that image
#     predates busbar's dlopen plugin ABI and describes a retired Unix-socket transport this
#     repo no longer builds a binary for — see that Dockerfile's header for the deprecation
#     note. This bundle supersedes it as the "just run Headroom" path.
#
# FROM scratch, because the binary is the whole product. Static musl busbar binaries are built
# from a sibling ../busbarAI checkout (source of truth for busbar itself — see
# .github/workflows/docker-bundle.yml) with the SAME PGO-mandatory release build busbarAI's own
# release/docker pipelines use, so this image's busbar binary is equivalently optimized, not a
# naive debug build. CA roots are compiled into the binary (webpki-roots), so no /etc/ssl needed.
#
# headroom-hook's signed plugin tarball ships PRE-INSTALLED at /etc/busbar/plugins, and the
# baked-in default config.yaml (docker/bundle/config.yaml) enables plugins and wires headroom
# into the default pool as a global rewrite gate — so compression works with zero plugin setup.
# Mounting your own config.yaml (see below) replaces that default entirely; copy its
# `plugins:`/`pools.*.hooks` blocks into yours to keep headroom wired.
#
# /lib/lib{c.musl-*,gcc_s,stdc++}.* below are NOT busbar's own runtime deps (the busbar binary
# itself is fully static) — they are what THIS repo's headroom cdylib needs at dlopen() time.
# aarch64-unknown-linux-musl's Rust target does not support `cdylib` output under Rust's default
# (crt-static-on) linking, so the cdylib is built with crt-static explicitly disabled (see the
# workflow), which makes it a normally-dynamically-linked musl/C++ shared object instead of the
# usual self-contained static-pie one. Bundling its exact runtime libs (built in the same
# musl-native container, so the ABI matches) is what makes dlopen() succeed in a FROM-scratch
# image that otherwise has no libc at all. The mechanism — the musl-native rust:alpine build
# container, crt-static disabled, runtime-lib extraction — is the same one busbarAI's own image
# build uses for this cdylib.
#
# Run (zero-config quickstart — headroom active, one provider):
#   docker run -d -p 8080:8080 \
#     -e ANTHROPIC_KEY -e BUSBAR_ADMIN_TOKEN \
#     getbusbar/busbar-headroom
#
# Run (your own config — copy the plugins:/hooks: blocks from docker/bundle/config.yaml into it
# to keep headroom wired):
#   docker run -d -p 8080:8080 \
#     -e ANTHROPIC_KEY \
#     -v "$PWD/config.yaml:/etc/busbar/config.yaml:ro" \
#     getbusbar/busbar-headroom
#
# Governance (optional) needs a writable volume for the SQLite file, e.g.
#   -v busbar-data:/var/lib/busbar   with governance.db_path: /var/lib/busbar/governance.db
# Seed /tmp: busbar's plugin loader prefers a zero-disk Linux memfd load, but falls back to
# private-temp-file staging (busbar-plugins-*/ under the OS temp dir) whenever memfd_create is
# unavailable to the process, which happens under emulated linux/arm64 on Apple Silicon among
# others. FROM scratch ships with no /tmp directory at all, so that fallback fails closed with
# "No such file or directory"; `--chmod=1777` below is explicit because BuildKit's legacy builder
# can silently drop the sticky bit on a plain multi-stage COPY, giving "Permission denied"
# instead. A tiny busybox stage seeds a
# real, world-writable /tmp so the fallback path works wherever memfd isn't available, without
# adding anything to the final image but the directory entry itself.
FROM busybox:latest AS tmp-seed
RUN mkdir -p /tmp-seed/tmp && chmod 1777 /tmp-seed/tmp

FROM scratch

ARG TARGETARCH
COPY --chmod=1777 --from=tmp-seed /tmp-seed/tmp /tmp
COPY binaries/${TARGETARCH}/busbar /busbar
COPY providers.yaml /etc/busbar/providers.yaml
COPY docker/bundle/config.yaml /etc/busbar/config.yaml
COPY plugins/${TARGETARCH}/busbar-headroom.tar.gz /etc/busbar/plugins/busbar-headroom.tar.gz
COPY plugins/${TARGETARCH}/lib/ /lib/

ENV BUSBAR_PROVIDERS=/etc/busbar/providers.yaml \
    BUSBAR_CONFIG=/etc/busbar/config.yaml

EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/busbar"]
