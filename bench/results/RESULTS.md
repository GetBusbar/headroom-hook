# headroom-hook — measured results

Machine: `Darwin host.local 25.5.0 Darwin Kernel Version 25.5.0: Tue Jun  9 22:28:34 PDT 2026; root:xnu-12377.121.10~1/RELEASE_ARM64_T6050 arm64`

## Hook per-call cost (direct socket driver, release build, no busbar)

| history size | wire line | p50 | p90 | p99 |
|---|---|---|---|---|
| 2 KB | 2.4 KiB | 0.15 ms | 0.19 ms | 0.21 ms |
| 8 KB | 8.5 KiB | 0.38 ms | 0.40 ms | 0.44 ms |
| 16 KB | 16.7 KiB | 0.72 ms | 0.75 ms | 0.78 ms |
| 64 KB | 65.5 KiB | 2.89 ms | 3.03 ms | 3.12 ms |

## Token savings by content type (estimated tokens, ceil(chars/4))

| corpus | tokens before | tokens after | saved | abstained |
|---|---|---|---|---|
| tool_log_11kb | 2,833 | 1,423 | 49.8% | no |
| tool_log_64kb | 16,419 | 8,235 | 49.8% | no |
| rag_dump_5kb | 1,335 | 721 | 46.0% | no |
| rag_dump_16kb | 4,211 | 2,127 | 49.5% | no |
| short_chat | 22 | 22 | 0.0% | yes |

Abstain rate: **100.0%** over 100 short chats (pass-through, byte-identical request), **0.0%** over 100 compressible histories.

## Busbar-path A/B (11 KB tool-log history, 1000 requests x1, recording mock upstream)

Added latency on busbar's OWN clock (`busbar;dur`, µs), base / +hook / added:

| ingress -> egress | busbar p50/p90/p99 | +hook p50/p90/p99 | added p50/p90/p99 | tokens/req | saved |
|---|---|---|---|---|---|
| anthropic -> openai | 34/36/41 | 584/612/648 | **550/576/607** | 2,832 -> 1,422 | 49.8% |
| openai -> openai | 22/25/30 | 569/601/634 | **547/576/604** | 2,832 -> 1,422 | 49.8% |

