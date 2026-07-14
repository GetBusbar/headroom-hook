# Grafana dashboard

`headroom-dashboard.json` is a ready-to-import Grafana dashboard for the Headroom
compression engine. It renders tokens saved, compression ratio, and latency straight
from Prometheus.

## It works against either endpoint

Headroom and Busbar both expose the **same `headroom_*` metric family**, so one
dashboard covers both:

| Source | Endpoint | Notes |
|---|---|---|
| Headroom proxy | `GET /metrics` | Headroom's own Prometheus exposition. |
| Busbar | `GET /metrics/hooks` | Busbar scrapes this hook and re-exposes the same `headroom_*` names, adding a `hook="headroom"` label and a per-`pool` label. |

The `Pool` and `Hook` template variables use regex matchers (`=~`), so they also match
Headroom-native series that carry no such label — the dashboard is identical whether you
point Prometheus at Headroom or at Busbar.

## Import

1. Grafana → **Dashboards → New → Import**.
2. **Upload** `headroom-dashboard.json`.
3. Pick your Prometheus datasource, then **Import**.

## Panels

Tokens saved (runtime + lifetime), request rate, compression-ratio distribution
(`histogram_quantile` over `headroom_compression_ratio_bucket`), compression latency
p50/p95/p99, tokens-saved-per-second by pool, cache hit rate (Headroom proxy only),
and requests by mode.

> This dashboard is also proposed upstream to
> [headroomlabs-ai/headroom](https://github.com/headroomlabs-ai/headroom) so it can ship
> as Headroom's own. Once merged there, it is the same file either way.
