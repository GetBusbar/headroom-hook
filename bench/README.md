# headroom-hook benchmarks

Every number we publish is produced by the scripts in this folder. All are stdlib
Python + release builds, no external deps, and use a deterministic corpus
([`corpus.py`](corpus.py)) so two runs produce byte-identical inputs. Results are
written to `results/` as JSON; [`report.py`](report.py) renders the Markdown tables.

## Reproduce every published number

First build the hook (release):

```sh
cargo build --release --manifest-path ../Cargo.toml
```

| Published claim | Command | Where in the output |
|---|---|---|
| Per-call compression cost (150 µs @ 2 KB … 2.9 ms @ 64 KB) | `python3 hook_bench.py` | `results/hook_direct.json` → `latency_by_history_size` |
| Token savings (2,832 → 1,422 = 49.8%; RAG 49.5%) | `python3 hook_bench.py` | `results/hook_direct.json` → `token_savings_by_content` |
| Abstain rate (100% short chats / 0% compressible) | `python3 hook_bench.py` | `results/hook_direct.json` → `abstain` |
| Added latency, busbar;dur p50/p90/p99 (base / +hook / added) | `BUSBAR_BIN=/path/to/busbar python3 busbar_ab.py --concurrency 1 --requests 1000` | `results/busbar_ab.json` → `delta[*].busbar_dur_us` |
| Upstream-confirmed token savings end to end | same as above | `delta[*].tokens_per_req_{baseline,hook}` (tallied at the mock, not hook-side) |
| Harness floor (~160 µs, why we report the delta) | `python3 floor.py` | stdout `p50_us` |

Render the tables:

```sh
python3 report.py results > results/RESULTS.md
```

## What each script does, and why

- **`hook_bench.py`** — drives the hook's Unix socket DIRECTLY, no busbar in the
  loop, speaking busbar's exact NDJSON wire (configure → ack, then transform).
  This is the cleanest measure of the hook itself: per-call cost by history size,
  token savings by content type, and abstain behavior.

- **`busbar_ab.py`** — the honest end-to-end test: one busbar, two configs that
  differ ONLY by the hook (`config.baseline.yaml` vs `config.hook.yaml`), same
  request stream through both, per ingress protocol, against a **recording** mock
  upstream. The mock tallies the tokens it actually received, which is what proves
  the compressed prompt shipped upstream rather than being hook-side accounting.
  We report the **delta** between the two phases: because both share the same
  harness round-trip floor, the floor cancels and the delta is exactly the hook's
  added cost. `--delay-ms N` models a real provider (the same delay on both paths,
  so the delta is unchanged — it just sets the denominator for "overhead as % of a
  call": 620 µs on a 2 s call is 0.03%).

- **`floor.py`** — measures the rig's own round-trip (stdlib client → mock, no
  busbar, no hook). It comes out ~160 µs, which is well ABOVE busbar's own
  tens-of-µs overhead — so this rig deliberately does NOT try to report busbar's
  solo cost as an absolute (busbar's own [benchmark](https://getbusbar.com/docs/benchmark/)
  does that from busbar's internal clock). What this rig measures precisely is the
  hook's added cost via the delta.

## Honesty notes

- Absolute per-request latencies from `busbar_ab.py` include the Python client and
  mock overhead; only the **with/without-hook delta** is a clean hook number.
- Savings are content- and setting-dependent (the corpus is noisy tool logs and
  RAG dumps at `target_ratio: 0.5`); ordinary conversation isn't compressible and
  the hook abstains on it.
- The `busbar_ab.py` path needs a busbar binary (`BUSBAR_BIN=` or `--busbar-bin`).
  busbar's release binary refuses non-loopback plaintext upstreams; the mock
  config here uses a loopback `http://127.0.0.1` upstream, which busbar 1.3 allows.
