#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# Docker A/B benchmark for the Headroom hook — the same methodology as busbar's own *_ab.py
# harnesses, but driven against a SHIPPED busbar image (getbusbar/busbar) the way a real install
# runs it. It measures the hook's added cost on busbar's OWN clock (`busbar;dur`), so the
# harness/network floor cancels in the with/without-hook delta.
#
# Topology (1.5.0 signed dlopen plugin ABI — NOT the retired socket/webhook hook transports):
#   * mock upstream — a recording OpenAI-shaped mock on 127.0.0.1:9001; tallies the chars that
#     actually ARRIVED upstream, proving the rewrite shipped rather than being hook-side accounting.
#   * busbar — ONE container, the getbusbar/busbar image, sharing the mock container's NETWORK
#     NAMESPACE so the mock is reachable at 127.0.0.1 (busbar's plaintext-loopback carve-out, same
#     as a local-model deployment). Its :8080 is published through the mock container.
#
# There is no second "headroom-hook" container and no shared Unix-socket volume: a `kind: hook`
# plugin loads IN-PROCESS (dlopen) inside the one busbar container. The hook phase differs from the
# baseline phase ONLY in which config.yaml is mounted (config.hook.yaml adds `plugins:` +
# `global_hooks:`) and in a `plugins/` directory bind-mounted at /etc/busbar/plugins containing the
# Headroom plugin tarball — see `prep_plugin()` below.
#
# The plugin tarball is built and packed from THIS checkout (not pulled from a release), so the
# bench always measures the current source tree: `cargo build --release` here, then
# `busbar-plugin-pack pack --allow-unsigned` (the busbar-plugin-pack binary is built from the
# busbarAI checkout — see BUSBARAI_DIR below). Both busbar and the plugin must target the SAME OS/
# arch as the container (Linux); on a GitHub Actions Linux runner the host IS that arch, so a plain
# `cargo build --release` is correct. On a non-Linux dev host (e.g. macOS), skip Docker and run
# `../scripts/local_verify.sh`-style native verification instead — a locally-built .dylib cannot
# dlopen inside a Linux container.
#
#   python3 docker_ab.py --requests 1000 --concurrency 1 --history-kb 11 [--delay-ms 0]
#
# Stdlib only (+ the docker CLI + cargo). Writes results/docker_ab.json and prints a summary.

import argparse
import http.client
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)  # headroom-hook checkout root
sys.path.insert(0, HERE)  # corpus.py is a sibling in this folder
import corpus  # noqa: E402

BUSBAR_IMAGE = os.environ.get("BUSBAR_IMAGE", "getbusbar/busbar:latest")
MOCK = "hb-mock"
BUSBAR = "hb-busbar"
# INTERNAL ports (inside the shared netns — fixed, never collide): busbar listens on
# 8080 (see config*.yaml), the mock on 9001 (providers.yaml points busbar there).
PORT = 8080
MOCK_PORT = 9001
# The only HOST-published port; override on a busy host with BENCH_PORT. The driver
# and healthz poll this; the mock's /stats,/reset are reached in-container via docker exec.
HOST_PORT = int(os.environ.get("BENCH_PORT", "8080"))
PLUGIN_DIR = os.path.join(HERE, "plugins")  # bind-mounted read-only at /etc/busbar/plugins


def sh(*args, check=True, quiet=True, cwd=None):
    kw = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL} if quiet else {}
    return subprocess.run(args, check=check, cwd=cwd, **kw)


def rm(*names):
    for n in names:
        sh("docker", "rm", "-f", n, check=False)


def percentile(vals, q):
    if not vals:
        return None
    s = sorted(vals)
    i = min(len(s) - 1, max(0, round(q * (len(s) - 1))))
    return s[i]


def busbar_dur_us(server_timing):
    # Server-Timing: busbar;dur=<milliseconds> -> integer microseconds.
    for part in (server_timing or "").split(","):
        part = part.strip()
        if part.startswith("busbar;dur="):
            try:
                return round(float(part.split("=", 1)[1]) * 1000.0)
            except ValueError:
                return None
    return None


def wait_ready(timeout=25.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://localhost:{HOST_PORT}/healthz", timeout=1) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.3)
    return False


def openai_body(history_kb):
    # The deterministic tool-log history as an OpenAI /v1/chat/completions body.
    msgs = corpus.history_messages("tool_log", history_kb * 1024)
    return json.dumps({"model": "bench-pool", "messages": msgs}).encode()


def mock_get(path):
    # The mock binds 127.0.0.1 inside its own netns, so reach its control endpoints
    # (/stats, /reset) from INSIDE that container (it ships python).
    method = "POST" if path == "/reset" else "GET"
    code = (
        "import urllib.request as u,sys;"
        f"r=u.urlopen(u.Request('http://127.0.0.1:{MOCK_PORT}{path}',method='{method}'));"
        "sys.stdout.write(r.read().decode())"
    )
    out = subprocess.run(
        ["docker", "exec", MOCK, "python", "-c", code],
        capture_output=True, text=True, check=True,
    ).stdout
    return json.loads(out) if out.strip() else {}


def load(body, n, conc, warmup):
    """Drive n keep-alive POSTs (after `warmup`), collect busbar;dur µs samples."""
    hdrs = {"content-type": "application/json"}  # auth.chain: [] in bench configs — no bearer token

    def run(count):
        durs = []
        conn = http.client.HTTPConnection("localhost", HOST_PORT, timeout=30)
        for _ in range(count):
            conn.request("POST", "/v1/chat/completions", body=body, headers=hdrs)
            r = conn.getresponse()
            st = r.getheader("Server-Timing")
            r.read()
            d = busbar_dur_us(st)
            if d is not None:
                durs.append(d)
        conn.close()
        return durs

    if conc <= 1:
        run(warmup)
        return run(n)
    # simple threaded fan-out for concurrency > 1
    import threading

    run(warmup)
    out, per = [], n // conc
    lock = threading.Lock()

    def w():
        d = run(per)
        with lock:
            out.extend(d)

    ts = [threading.Thread(target=w) for _ in range(conc)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    return out


def start_mock(delay_ms):
    rm(MOCK)
    # Publish only busbar's data port (busbar shares this netns and listens on PORT
    # internally). The mock's own port stays internal — reached via docker exec.
    sh("docker", "run", "-d", "--name", MOCK, "-p", f"{HOST_PORT}:{PORT}",
       "-v", f"{HERE}/mock_upstream.py:/mock.py:ro",
       "python:3.12-slim", "python", "/mock.py", "--port", str(MOCK_PORT),
       "--delay-ms", str(delay_ms))
    time.sleep(2)


def busbarai_dir():
    """Locate the busbarAI checkout that provides busbar-plugin-pack.

    headroom-hook/Cargo.toml currently carries an INTERIM absolute path dependency on
    busbar-plugin-sdk (busbarAI is not yet public — see the Cargo.toml comment). Parse that same
    path out of Cargo.toml rather than hardcoding it a second time here, so this script and
    Cargo.toml can only drift together. Override with BUSBARAI_DIR for a CI checkout laid out
    differently (e.g. busbarAI's own release.yml checks headroom-hook out as a ../ sibling and
    patches this path — see that workflow's "Patch headroom-hook's interim path dependencies" step).
    """
    env = os.environ.get("BUSBARAI_DIR")
    if env:
        return env
    cargo_toml = open(os.path.join(REPO, "Cargo.toml")).read()
    m = re.search(r'path\s*=\s*"([^"]+)/crates/plugin-sdk"', cargo_toml)
    if not m:
        sys.exit("could not locate busbarAI checkout: no busbar-plugin-sdk path dep in Cargo.toml "
                 "and BUSBARAI_DIR is not set")
    return m.group(1)


def prep_plugin():
    """Build the Headroom cdylib from THIS checkout and pack it as an unsigned dev tarball.

    Requires the host arch/OS to match the busbar container's (Linux) — see the module docstring.
    Writes plugins/busbar-headroom.tar.gz, bind-mounted read-only at /etc/busbar/plugins by
    start_busbar() for the hook phase. config.hook.yaml sets plugins.trust.allow_unsigned: true to
    accept it; a real deployment installs busbar's release-CI-signed tarball instead.
    """
    print("building headroom-hook cdylib (cargo build --release)...", file=sys.stderr)
    sh("cargo", "build", "--release", cwd=REPO, quiet=False)

    ext = "dylib" if sys.platform == "darwin" else "so"
    lib = os.path.join(REPO, "target", "release", f"libheadroom_hook.{ext}")
    if not os.path.exists(lib):
        sys.exit(f"expected cdylib not found: {lib} (did the release build produce it?)")

    bdir = busbarai_dir()
    print(f"building busbar-plugin-pack from {bdir} ...", file=sys.stderr)
    sh("cargo", "build", "--release", "-p", "busbar-plugin-pack", cwd=bdir, quiet=False)
    pack = os.path.join(bdir, "target", "release", "busbar-plugin-pack")
    if not os.path.exists(pack):
        sys.exit(f"busbar-plugin-pack binary not found at {pack}")

    # cheap TOML scrape (avoids a tomllib dep / import ordering fuss): version = "x.y.z"
    ver_m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', open(os.path.join(REPO, "Cargo.toml")).read())
    version = ver_m.group(1) if ver_m else "0.0.0"

    shutil.rmtree(PLUGIN_DIR, ignore_errors=True)
    os.makedirs(PLUGIN_DIR, exist_ok=True)
    out = os.path.join(PLUGIN_DIR, "busbar-headroom.tar.gz")
    sh(pack, "pack",
       "--lib", lib,
       "--name", "busbar-headroom", "--alias", "headroom", "--kind", "hook",
       "--version", version, "--publisher", "busbar", "--license", "Apache-2.0",
       "--needs-prompt", "rw", "--allow-unsigned",
       "--out", out,
       quiet=False)
    if not os.path.exists(out):
        sys.exit(f"plugin pack did not produce {out}")
    print(f"packed {out}", file=sys.stderr)


def start_busbar(config, with_hook):
    rm(BUSBAR)
    args = ["docker", "run", "-d", "--name", BUSBAR, "--network", f"container:{MOCK}",
            "-e", "BENCH_MOCK_KEY=x", "-e", "BUSBAR_STATE_FILE=",
            "-v", f"{HERE}/{config}:/etc/busbar/config.yaml:ro",
            "-v", f"{HERE}/providers.mock.yaml:/etc/busbar/providers.yaml:ro"]
    if with_hook:
        # The whole hook phase, vs. baseline, is: this one extra mount + config.hook.yaml's
        # `plugins:`/`global_hooks:` blocks. No second container, no socket volume.
        args += ["-v", f"{PLUGIN_DIR}:/etc/busbar/plugins:ro"]
    args += [BUSBAR_IMAGE]
    sh(*args)
    if not wait_ready():
        print("busbar did not become ready; logs:", file=sys.stderr)
        sh("docker", "logs", BUSBAR, check=False, quiet=False)
        sys.exit(1)


def measure(config, with_hook, args):
    start_busbar(config, with_hook)
    body = openai_body(args.history_kb)
    mock_get("/reset")
    durs = load(body, args.requests, args.concurrency, args.warmup)
    stats = mock_get("/stats")
    tokens = round(stats.get("prompt_tokens_est", 0) / max(1, stats.get("requests", 1)))
    return {
        "busbar_dur_us": {q: percentile(durs, p) for q, p in (("p50", .5), ("p90", .9), ("p99", .99))},
        "samples": len(durs),
        "tokens_per_req": tokens,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--requests", type=int, default=1000)
    ap.add_argument("--concurrency", type=int, default=1)
    ap.add_argument("--warmup", type=int, default=50)
    ap.add_argument("--history-kb", type=int, default=11)
    ap.add_argument("--delay-ms", type=int, default=0)
    ap.add_argument("--skip-plugin-build", action="store_true",
                     help="reuse an existing plugins/busbar-headroom.tar.gz instead of rebuilding")
    args = ap.parse_args()

    if not args.skip_plugin_build:
        prep_plugin()
    elif not os.path.exists(os.path.join(PLUGIN_DIR, "busbar-headroom.tar.gz")):
        sys.exit("--skip-plugin-build given but plugins/busbar-headroom.tar.gz does not exist")

    try:
        start_mock(args.delay_ms)
        base = measure("config.baseline.yaml", False, args)
        hook = measure("config.hook.yaml", True, args)
    finally:
        rm(MOCK, BUSBAR)

    delta = {q: (hook["busbar_dur_us"][q] - base["busbar_dur_us"][q])
             for q in ("p50", "p90", "p99")}
    saved_pct = round(100 * (1 - hook["tokens_per_req"] / max(1, base["tokens_per_req"])), 1)
    result = {
        "config": vars(args),
        "images": {"busbar": BUSBAR_IMAGE},
        "plugin": "built from this checkout, packed unsigned (dev mode)",
        "baseline": base, "hook": hook,
        "added_busbar_dur_us": delta,
        "tokens": {"baseline": base["tokens_per_req"], "hook": hook["tokens_per_req"],
                   "saved_pct": saved_pct},
    }
    os.makedirs(f"{HERE}/results", exist_ok=True)
    with open(f"{HERE}/results/docker_ab.json", "w") as f:
        json.dump(result, f, indent=2)
    print(json.dumps(result, indent=2))
    print(f"\nADDED busbar;dur (hook cost)  p50={delta['p50']}µs "
          f"p90={delta['p90']}µs p99={delta['p99']}µs")
    print(f"tokens/req {base['tokens_per_req']} -> {hook['tokens_per_req']}  ({saved_pct}% saved)")


if __name__ == "__main__":
    main()
