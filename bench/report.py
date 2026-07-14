#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# Render the rig's JSON results (hook_direct.json + busbar_ab.json) as the Markdown tables the
# README embeds. Usage: python3 report.py [results-dir] > results/RESULTS.md

import json
import sys
from pathlib import Path


def main():
    results = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent / "results"
    direct = json.loads((results / "hook_direct.json").read_text())
    ab_path = results / "busbar_ab.json"
    ab = json.loads(ab_path.read_text()) if ab_path.exists() else None

    print("# headroom-hook — measured results\n")
    print(f"Machine: `{direct['uname']}`\n")

    print("## Hook per-call cost (direct socket driver, release build, no busbar)\n")
    print("| history size | wire line | p50 | p90 | p99 |")
    print("|---|---|---|---|---|")
    for row in direct["latency_by_history_size"]:
        print(
            f"| {row['history_kb']} KB | {row['wire_line_bytes'] / 1024:.1f} KiB "
            f"| {row['p50_us'] / 1000:.2f} ms | {row['p90_us'] / 1000:.2f} ms "
            f"| {row['p99_us'] / 1000:.2f} ms |"
        )

    print("\n## Token savings by content type (estimated tokens, ceil(chars/4))\n")
    print("| corpus | tokens before | tokens after | saved | abstained |")
    print("|---|---|---|---|---|")
    for row in direct["token_savings_by_content"]:
        print(
            f"| {row['case']} | {row['tokens_before_est']:,} | {row['tokens_after_est']:,} "
            f"| {row['saved_pct']}% | {'yes' if row['abstained'] else 'no'} |"
        )

    ab_stats = direct["abstain"]
    print(
        f"\nAbstain rate: **{ab_stats['short_chat_abstain_pct']}%** over "
        f"{ab_stats['short_chats']} short chats (pass-through, byte-identical request), "
        f"**{ab_stats['compressible_abstain_pct']}%** over "
        f"{ab_stats['compressible_histories']} compressible histories.\n"
    )

    if ab:
        print(
            f"## Busbar-path A/B ({ab['history_kb']} KB tool-log history, "
            f"{ab['requests']} requests x{ab['concurrency']}, recording mock upstream)\n"
        )
        print("Added latency on busbar's OWN clock (`busbar;dur`, µs), base / +hook / added:\n")
        print("| ingress -> egress | busbar p50/p90/p99 | +hook p50/p90/p99 | added p50/p90/p99 | tokens/req | saved |")
        print("|---|---|---|---|---|---|")
        for name, d in ab["delta"].items():
            bd = d["busbar_dur_us"]
            b, h, a = bd["base"], bd["with_hook"], bd["hook_added"]
            print(
                f"| {name.replace('_to_', ' -> ')} "
                f"| {b['p50']}/{b['p90']}/{b['p99']} "
                f"| {h['p50']}/{h['p90']}/{h['p99']} "
                f"| **{a['p50']}/{a['p90']}/{a['p99']}** "
                f"| {d['tokens_per_req_baseline']:,.0f} -> {d['tokens_per_req_hook']:,.0f} "
                f"| {d['tokens_saved_pct']}% |"
            )
        print()


if __name__ == "__main__":
    main()
