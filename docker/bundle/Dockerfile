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
# FROM scratch, because the binary is the whole product. The busbar binary is built from a sibling
# ../busbarAI checkout (source of truth for busbar itself, see
# .github/workflows/docker-bundle.yml) with the SAME PGO-mandatory release build busbarAI's own
# release/docker pipelines use, so this image's busbar binary is equivalently optimized, not a
# naive debug build. CA roots are compiled into the binary (webpki-roots), so no /etc/ssl needed.
#
# busbar here is DYNAMICALLY linked against musl, NOT static-pie like busbarAI's own image, and
# that difference is load-bearing rather than incidental. A fully static musl binary has no
# dynamic loader in it at all, so dlopen() is a stub that always fails: every version of this
# bundle built before this change died at boot with
#
#   [warn] memfd load unavailable for plugin 'busbar-headroom' (... Dynamic loading not supported)
#   [error] gate hook 'busbar-headroom' (plugin 'busbar-headroom') failed to open; refusing to
#           boot/reload with a silently-absent admission gate (fail-closed): failed to load
#           plugin 'busbar-headroom': Dynamic loading not supported
#
# which made THIS IMAGE'S ENTIRE REASON FOR EXISTING non-functional: the whole point of the
# bundle is that the plugin is pre-loaded. busbarAI's own getbusbar/busbar image can stay static
# because it ships plugin-free; a bundle cannot. So busbar is built with crt-static disabled in
# the same musl-native rust:alpine container as the cdylib, and the musl DYNAMIC LOADER
# (/lib/ld-musl-<arch>.so.1, which is the busbar binary's PT_INTERP) is carried in alongside the
# other runtime libs below. Without that one file the image cannot exec /busbar AT ALL: the
# kernel reports a bare "exec /busbar: permission denied", which looks like a file-mode problem
# and is not one.
#
# headroom-hook's signed plugin tarball ships PRE-INSTALLED at /etc/busbar/plugins, and the
# baked-in default config.yaml (docker/bundle/config.yaml) enables plugins and wires headroom
# into the default pool as a global rewrite gate — so compression works with zero plugin setup.
# Mounting your own config.yaml (see below) replaces that default entirely; copy its
# `plugins:`/`pools.*.hooks` blocks into yours to keep headroom wired.
#
# /lib/{ld-musl-*,libc.musl-*,libgcc_s,libstdc++}.* below are what the busbar binary and THIS
# repo's headroom cdylib need to run and to dlopen(). ld-musl-<arch>.so.1 is the dynamic loader
# named by busbar's PT_INTERP; libc.musl-<arch>.so.1 is the SAME file under its DT_NEEDED name
# (in Alpine the latter is a symlink to the former, and a symlink does not survive being carried
# through a CI artifact, so both names are shipped as real files).
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
#   -v busbar-data:/var/lib/busbar   with store.settings.db_path: /var/lib/busbar/governance.db
# (`governance.db_path` was the 1.4.x spelling; the 1.5.0 config redesign dissolved `governance:`
# into store/rate_card/groups/advanced, and 1.5.3 refuses to boot a config still using it. The
# same volume is the right home for config.overlay.file if you want admin-API config mutations to
# outlive the container - see docker/bundle/config.yaml.)
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
# --chown as well as --chmod: /tmp now carries a SECOND load-bearing job beyond the plugin
# loader's staging fallback. busbar 1.5.3 refuses to boot a mutable config whose overlay backend
# is not writable, and the default backend sits next to config.yaml in /etc/busbar, which this
# image gives to root while the process runs as 65532. docker/bundle/config.yaml therefore points
# config.overlay.file at /tmp/busbar-overlay.json. The comment above records that BuildKit's
# legacy builder was OBSERVED to drop the sticky bit on a plain multi-stage COPY; if it ever
# drops --chmod again, 1777-vs-0755 stops being a fallback-path nicety and becomes a hard boot
# failure, so the directory is ALSO owned by the runtime uid as a second, independent guarantee.
COPY --chown=65532:65532 --chmod=1777 --from=tmp-seed /tmp-seed/tmp /tmp
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
