# Grafana dashboard

`headroom-dashboard.json` is a ready-to-import Grafana dashboard for the Headroom
compression engine. It renders tokens saved, request throughput, and processing
overhead straight from Prometheus.

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

Tokens saved, input tokens, request rate, average processing overhead
(`headroom_overhead_ms_sum` / `headroom_overhead_ms_count`) with reported min/max,
tokens-saved-per-second by pool, and request rate by pool. Built entirely on
Headroom's real `/metrics` names — counters plus the `headroom_overhead_ms_*`
millisecond summary; the proxy emits no histograms, so none are used.

> This dashboard is also proposed upstream to
> [headroomlabs-ai/headroom](https://github.com/headroomlabs-ai/headroom) so it can ship
> as Headroom's own. Once merged there, it is the same file either way.
