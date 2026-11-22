# TDD 0020: Fallback on a miss

- Status: Accepted; implemented
- Created: 2026-11-22
- Updated: 2026-11-22
- Related ADRs: None
- Related designs: [TDD 0015](0015-vod-module-mapping-adapter.md) (names `vod_fallback_upstream_location` as a later part), [TDD 0019](0019-url-compatibility.md)

## Summary

nginx-vod-module can pass a request it cannot serve on to another server, usually another datacenter that has the file. An opt-in `[fallback]` section does the same: when an asset or its media file is missing, vodnik asks the fallback server for the same URL and returns its answer.

## Context

In nginx-vod-module the fallback is a proxy: the same request goes to another upstream, and a marker header stops two servers from handing a request back and forth. vodnik has no redirect or proxy of its own today. A miss is a `404` when the resolver says there is no such asset, and a `502` when the mapper named a file that is not there (a local file that does not exist, or an origin that answers `404`).

**What this design could not confirm.** The exact set of conditions in which nginx-vod-module falls back was taken from its documentation summary, not from its source. A miss is defined here by vodnik's own two cases, which is what a deployment's fallback is for.

## Goals

- A deployment that used the fallback keeps its other datacenter reachable through the same URLs.
- A viewer sees the fallback's answer as if this server had given it: status, headers and body, with ranges.
- Nothing changes without `[fallback]`: no layer is added to the router.
- A loop between two servers that name each other is impossible.
- A fallback that is down never makes things worse than the miss was.

## Non-goals

- Failing over on an outage (`503`) or a bad answer (`502` from the mapper). Those are not misses; retrying them is the mapper's and origin's own policy.
- A list of fallbacks tried in turn. One server; a load balancer in front of several is the answer.
- Redirecting the viewer (`302`). nginx proxies, and a player treats a redirect on a segment differently from a proxied answer.
- Fallback for requests that fail after a response has started.

## Design

### Configuration

```toml
[fallback]
enabled = true
base_url = "https://dc2.example.net"   # scheme, host and an optional path prefix; no query
on_missing_file = true                 # also pass on a media file that is not there
timeout_ms = 5000                      # connect timeout, and the longest the fallback may stay silent
loop_header = "x-vod-fallback"         # sent on every forwarded request
```

### What is a miss

The HTTP error type carries a marker for the two cases, set where the cause is known:

| Marker | When | Status without a fallback |
|---|---|---|
| `Asset` | the resolver answered `404` or `410` | `404` |
| `File` | the location resolved but the file is not there: a local path that does not exist, or an origin that answers `404` | `502`, as before |

A track, clip or subtitle that an existing asset does not have is a plain `404` with no marker: the asset is here, the other server would not have a different one.

### What happens

A layer on the viewer routes, inside the token check and outside the handlers, runs the request here first. If the response carries a marker the configuration takes, and the request is a `GET` or `HEAD` that does not already carry the loop header, the layer asks the fallback:

- URL: `base_url` followed by the path and query the viewer sent. With `[url_compat]` that is the viewer's original URL, kept before the rewrite.
- Headers: all of the viewer's, except `Host`, `Content-Length` and the connection-level ones, plus the loop header. `Range` and the validators therefore reach the fallback.
- Answer: status, headers and body are passed on as they are and streamed with a small buffer. A redirect is not followed.
- If the fallback cannot be reached or does not answer within `timeout_ms`, the original response is returned, and a `fallback_unreachable` event is logged.

The token check sits outside the layer, so a viewer without a valid token never causes a request to the fallback. A served fallback is logged as `fallback_served` with its status.

### Loop guard

Every forwarded request carries `loop_header`. A request that arrives with it is answered here and never forwarded. Two vodnik servers that name each other therefore stop after one hop. When the fallback is an nginx-vod-module, point `loop_header` at its `vod_proxy_header_name` so it recognises the request.

## Alternatives considered

- **Mapper concern: a second location in the answer.** Fits a mapper that knows both datacenters, but a deployment moving from nginx-vod-module has the second datacenter in its nginx configuration, not in its mapper.
- **Redirect.** Cheaper for this server, but a viewer follows it on every segment and the CDN cache key changes with the host.
- **Origin failover list.** Covers a missing file but not an unknown asset, and cannot serve a different path layout.

## Testing

End to end against a mock fallback server: the same URL, query and `Range` arrive; the answer, a `404` and a `302` pass through; the loop header stops a second hop; an asset that exists and a track it does not have never go; an unreachable fallback leaves the original answer; a missing local file and an origin `404` go only with `on_missing_file`; and with `[url_compat]` the fallback is asked for the original URL.

## Open questions

- A metric for fallbacks served. Logs carry the event today.
- A fallback for a failure that happens after the response has started (a segment read that fails midway). Not possible without buffering.
