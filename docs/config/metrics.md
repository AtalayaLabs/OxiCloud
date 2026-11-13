# HTTP request metrics

Set `OXICLOUD_METRICS_LISTEN=127.0.0.1:9090` to enable the existing,
separate Prometheus listener. Scrape `http://127.0.0.1:9090/metrics` from
the same host, or configure a private address reachable by your collector.
This listener has no authentication. Leaving the variable unset omits both
the listener and the request-timing middleware.

`oxicloud_http_request_duration_seconds` is a classic Prometheus histogram
for the main HTTP listener. It includes API, DAV, web/static and probe
requests, middleware rejections, and fallback responses. The separate
metrics listener is not instrumented, so scraping does not add samples.
Existing counters, gauges, access logs and audit logs remain available.

Each sample starts at the outer request middleware and ends when the inner
service returns the response headers. It includes handler work and any
request body the handler consumes. It excludes response body streaming,
socket transfer time, SSE/WebSocket lifetimes, and time before the request
enters the router. A streamed download can produce a successful header
sample and then fail while its body is sent. Requests cancelled before a
response is produced do not contribute a sample. This is not end-to-end
client latency or a count of successfully completed downloads.

Labels are bounded by the registered routes and protocol vocabulary:

| Label | Values |
| --- | --- |
| `route` | Axum's registered template, such as `/api/files/{id}` or a DAV wildcard template. Fallbacks and services without a matched template use `unmatched`. Actual path parameters, query strings and filenames are never used. |
| `method` | `GET`, `HEAD`, `POST`, `PUT`, `DELETE`, `CONNECT`, `OPTIONS`, `TRACE`, `PATCH`, `PROPFIND`, `PROPPATCH`, `MKCOL`, `COPY`, `MOVE`, `LOCK`, `UNLOCK`, `REPORT`, `MKCALENDAR`, `ACL`, `SEARCH`; every other verb is `OTHER`. |
| `status` | The returned HTTP status code, including middleware errors. |

No additional user, tenant, request, file or share identifier labels are added.
The configured deployment prefix remains part of each route template. Bucket
upper bounds are **seconds**: `0.005`, `0.01`, `0.025`, `0.05`, `0.1`,
`0.25`, `0.5`, `1`, `2.5`, `5`, `10`, `30`, and `+Inf`. The exporter emits
`_bucket`, `_sum` and `_count` series; other metric families retain their
existing exporter configuration.

The exporter drains buffered histogram samples every five seconds, even when
no collector scrapes the endpoint. Bucket totals remain cumulative.

For example, five-minute p95 header latency per route and method, aggregated
across instances:

```promql
histogram_quantile(0.95,
  sum by (le, route, method) (
    rate(oxicloud_http_request_duration_seconds_bucket[5m])
  )
)
```

Five-minute rate of responses with 5xx headers:

```promql
sum by (route) (
  rate(oxicloud_http_request_duration_seconds_count{status=~"5.."}[5m])
)
```

Filter probe/static routes when defining an application SLI. These samples
show how long responses take to start; they do not attribute time to SQL,
blob storage or hashing. OTLP traces remain separate follow-up work.
