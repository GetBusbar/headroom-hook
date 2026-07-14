#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# Deterministic benchmark corpus for the headroom-hook rig. Three realistic history flavors:
#
#   * tool_log(kb)   — noisy CI/deploy tool output: timestamped INFO noise with a few
#                      load-bearing WARN/ERROR lines buried in it. The "agent pasted a log" shape.
#   * rag_dump(kb)   — retrieved doc chunks, most off-topic for the ask, a few on-topic.
#                      The "RAG stuffed the context" shape.
#   * short_chat()   — a plain 3-turn conversational exchange: nothing worth compressing.
#
# Everything is seeded/formulaic — two runs of the rig produce byte-identical inputs.
#
# Token counts are ESTIMATED as ceil(chars / 4) (the standard ~4-chars-per-token heuristic).
# The rig reports savings as ratios, so the estimator's absolute error largely cancels.

import json
import math

TOOL_LOG_QUERY = "why did the deployment fail"
RAG_QUERY = "why are payment gateway requests timing out"

_SERVICES = ["api-edge", "billing", "search", "ingest", "webhooks", "notifier", "scheduler"]
_STEPS = [
    "pulled image layer sha256:{h} in 0.{ms}s",
    "helm release {svc} upgraded to revision {i} without incident",
    "readiness probe for pod {svc}-{i} passed after 2 attempts",
    "config map {svc}-cm rendered (checksum {h})",
    "autoscaler held {svc} at 3 replicas (cpu 41%)",
    "migration step {i} applied cleanly in 84ms",
    "cache warmed for shard {i} ({ms} keys)",
]


def est_tokens(text: str) -> int:
    """~4 chars per token; ratio-stable across before/after."""
    return max(1, math.ceil(len(text) / 4))


def tool_log(target_bytes: int) -> str:
    """A deploy log of ~target_bytes: mostly routine noise, one ERROR + two WARNs buried mid-way."""
    lines, i = [], 0
    size = 0
    # Positions for the load-bearing lines are fixed fractions of the (approximate) line count.
    approx_lines = max(6, target_bytes // 78)
    marks = {
        approx_lines // 2: "ERROR: deployment canary failed with status 503 on us-east-1.",
        approx_lines // 3: "WARN: rollout paused; canary error budget at 92% consumption.",
        (2 * approx_lines) // 3: "WARN: upstream health check flapping on api-edge after rollout.",
    }
    while size < target_bytes:
        if i in marks:
            line = f"14:02:{i % 60:02d} {marks[i]}"
        else:
            t = _STEPS[i % len(_STEPS)]
            line = "14:02:%02d INFO %s" % (
                i % 60,
                t.format(
                    h=format((i * 2654435761) % 0xFFFFFF, "06x"),
                    ms=100 + (i * 37) % 900,
                    svc=_SERVICES[i % len(_SERVICES)],
                    i=i,
                ),
            )
        lines.append(line)
        size += len(line) + 1
        i += 1
    return "\n".join(lines)


_RAG_TOPICS = [
    "The invoicing subsystem batches ledger writes every 30 seconds and reconciles nightly.",
    "Search indexing uses a two-phase commit against the document store with a 5 minute SLA.",
    "The mobile client caches profile avatars for 24 hours and revalidates with ETags.",
    "Payment gateway requests traverse the egress proxy, which enforces a 10 second connect timeout.",
    "Data retention for audit events is 400 days in cold storage with quarterly compaction.",
    "The payment gateway circuit breaker opens after 5 consecutive upstream timeouts.",
    "Notification fan-out is sharded by tenant id across 16 queues with at-least-once delivery.",
    "Session tokens rotate every 12 hours; refresh happens transparently in the SDK.",
    "Gateway timeout spikes correlate with TLS re-handshakes when the connection pool is exhausted.",
    "The recommendation model retrains weekly on a 30 day sliding window of engagement data.",
]


def rag_dump(target_bytes: int) -> str:
    """Retrieved doc chunks of ~target_bytes: numbered chunks cycling topics; a minority mention
    the payment gateway/timeouts (relevant to RAG_QUERY), the rest are plausible off-topic docs."""
    chunks, i, size = [], 0, 0
    while size < target_bytes:
        base = _RAG_TOPICS[i % len(_RAG_TOPICS)]
        chunk = (
            f"[doc-{i:03d}] {base} Additional context paragraph {i}: operational details, "
            f"ownership, and escalation paths are documented in runbook section {i % 9 + 1}."
        )
        chunks.append(chunk)
        size += len(chunk) + 2
        i += 1
    return "\n\n".join(chunks)


def short_chat_messages(variant: int):
    """A short conversational exchange (variant-numbered so a corpus of them isn't identical)."""
    return [
        {"role": "user", "content": f"hey, quick question #{variant}"},
        {"role": "assistant", "content": "Sure — go ahead."},
        {"role": "user", "content": f"what's a good name for helper function number {variant}?"},
    ]


def history_messages(flavor: str, target_bytes: int):
    """Two history messages (each ~half the budget) + the short ask, as {role, content} dicts."""
    if flavor == "tool_log":
        half = tool_log(target_bytes // 2)
        ask = TOOL_LOG_QUERY
    elif flavor == "rag_dump":
        half = rag_dump(target_bytes // 2)
        ask = RAG_QUERY
    else:
        raise ValueError(flavor)
    return [
        {"role": "user", "content": half},
        {"role": "assistant", "content": half},
        {"role": "user", "content": ask},
    ]


def wire_line(messages) -> bytes:
    """Busbar's hook transform projection for a message list ({role, text} form), one NDJSON line."""
    total = sum(len(m["content"]) for m in messages)
    req = {
        "request": {
            "pool": "bench-pool",
            "ingress_protocol": "anthropic",
            "message_count": len(messages),
            "has_tools": False,
            "total_chars": total,
            "max_tokens": 64,
            "stream": False,
            "messages": [{"role": m["role"], "text": m["content"]} for m in messages],
        },
        "candidates": [],
        "context": {"pool": "bench-pool", "budget_remaining": None},
    }
    return (json.dumps(req) + "\n").encode()
