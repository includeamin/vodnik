# TDD 0019: URL compatibility for nginx-vod-module entry URLs

- Status: Accepted; implemented for single-file URIs
- Created: 2026-11-22
- Updated: 2026-11-22
- Related ADRs: None
- Related designs: [TDD 0015](0015-vod-module-mapping-adapter.md) (the dialect this extends; its open question on file URIs with slashes), [TDD 0018](0018-drm-bridge.md)

## Summary

A player that moves from nginx-vod-module keeps its hard-coded entry URL, such as `/hls/2020/05/movie.mp4/master.m3u8`, and still reaches an asset, with no rewrite at a proxy. An opt-in `[url_compat]` section makes vodnik accept a file URI with slashes and dots in place of an asset ID, and the `vod-module` dialect asks the mapping service for that URI as nginx-vod-module would: with the slashes unescaped.

## Context

vodnik's routes name an asset by one path segment (`/hls/{id}/master.m3u8`), and an ID is 1 to 128 letters, digits, `-` and `_`. An nginx-vod-module URL names a file by a URI of any shape, and the mapping service is asked for that URI as a path.

Only the **entry URL** needs to match. Every URL inside a manifest is made by vodnik, and a manifest links to its siblings by relative path, so a player that fetched `/hls/2020/05/movie.mp4/master.m3u8` follows `video/index.m3u8` to `/hls/2020/05/movie.mp4/video/index.m3u8`. The compatible routes therefore have to cover the whole tree under a multi-segment prefix, not only the entry.

**What this design could not confirm.** nginx-vod-module's own URL documentation was not reachable when this was written, so the entry forms it covers (`master.m3u8` and `manifest.mpd` under `/hls` and `/dash`) are the ones its deployments are known to use, not a verified list. The forms below are the gaps.

## Goals

- A deployment's players keep their entry URLs.
- The mapping service is asked for the same path nginx-vod-module asked it for.
- Nothing changes for a deployment that does not turn it on, and no route is added to the table.
- A URL cannot be used to reach what the mapping service would not serve: no `..`, empty segments or control characters.

## Non-goals

- `.urlset` and comma-separated lists of files in one URL (several qualities from one URL). A set comes from the mapper as renditions.
- nginx's own segment, playlist and fragment URLs (`seg-1-v1-a1.ts`, `fragment-1-f1-v1-a1.m4s`, `index-v1-a1.m3u8`). A player follows the URLs in our manifests and never builds these.
- An entry that is a media playlist (`index.m3u8`) rather than a master.
- Query-string conventions of nginx deployments beyond passing the query through.
- Viewer tokens issued for the original URI: a token's asset claim names the asset ID the server uses, which for such an URL is the compatible ID below.

## Design

### Configuration

```toml
[url_compat]
enabled = true
path_prefix = "/vod"    # what a proxy or location puts before /hls and /dash; empty strips nothing
```

It needs `[resolver.http]` with `dialect = "vod-module"`, because only that dialect can pass a file URI on to a service. Enabling it elsewhere fails startup. `path_prefix` is empty or a plain absolute path.

### The rewrite

The router is wrapped so that the request path is rewritten **before** routing. With `[url_compat]` off there is no wrapper. With it on, for a path that, after the prefix, starts `/hls/` or `/dash/`:

1. The path is split from the right into the file URI and a **native suffix**, one of vodnik's own route shapes: `master.m3u8`, `{track}/index.m3u8`, `video/iframes.m3u8`, `video/iframes/{n}/media.m4s`, `subtitles/{lang}/index.m3u8`, `subtitles/{lang}/sub.vtt`, `{track}/init.mp4`, `{track}/clips/{n}/init.mp4`, `{track}/segments/{n}/media.m4s` for HLS and `manifest.mpd`, `sub.vtt`, `init.mp4`, clip init and segment forms for DASH. The last segment decides the shape and the fixed words before it are checked.
2. Each segment of the file URI is percent-decoded once; a bad escape, non-UTF-8 bytes or an escaped slash refuses the rewrite.
3. A URI that is one segment and already a valid asset ID is left alone: it is a native URL.
4. Otherwise the URI is turned into a **compatible ID** and the path becomes `/hls/{id}/{suffix}`, with the query string kept.

A request that is not rewritten is routed as it is, which gives the usual `404`.

### The compatible ID

An asset ID cannot hold a slash, so the file URI is carried as `vm--` plus the base64url form of the URI. That is a valid ID like any other (letters, digits, `-`, `_`), so the registry, the caches, the metrics and the authorization check need no change. It limits a URI to **93 bytes**; a longer one is not rewritten and is a `404`.

The URI is accepted only if every segment is non-empty and not `.` or `..` and nothing is a control character. This is checked when the ID is made and **again when it is decoded**, because anyone can type an ID of that shape into a URL.

`vm--` is reserved when `[url_compat]` is on: an ID that starts with it and does not decode is a `404`.

### The mapping request

With `[url_compat]` on, the `vod-module` dialect turns a compatible ID back into the URI and puts it in `{id}` of `path_template` (and of the key service's, TDD 0018) with each segment percent-escaped and the slashes kept. An ID of any other kind is escaped whole, as before. So `/hls/2020/05/movie.mp4/master.m3u8` with `path_template = "vod/{id}"` asks `GET /vod/2020/05/movie.mp4`.

## Security and limits

- The rewrite never reaches outside the file URI the mapping service is asked about: no `.`, `..`, empty segment, NUL or control character, no escaped slash, at most 93 bytes.
- The ID is decoded and checked again where it is used, so a hand-typed `vm--` ID cannot smuggle a path the rewrite would have refused.
- Nothing is added to the router when the feature is off.

## Observability

Nothing new. After the rewrite a request is counted under the native route it was routed to, and `/admin/status` lists a compatible asset by its ID (`vm--...`), which is opaque.

## Testing

- **Rewrite, table-driven:** every route shape under a multi-segment URI for HLS and DASH; the query kept; native IDs and unrelated paths untouched; a single dotted name; escapes decoded once; every refusal (`..`, `.`, empty segment, escaped slash, NUL, control character, bad escape, non-UTF-8, too long); the prefix.
- **The ID:** round trip; the 93-byte limit; a typed ID checked like a made one.
- **End to end through the mock mapping service:** a URL with slashes serves the asset and **every URL a player follows from it**, resolved relative to the URL it came from, for HLS and DASH; the mapping service sees real slashes, a kept dot and re-escaped escapes; a prefix; a refused URI never reaches the service; with the feature off the same URLs are `404`.
- **Mutations:** a decode that skips the check, a native ID rewritten too, slashes escaped in the mapping request, and a dropped query string each fail a test.
- **Not testable here:** a real deployment's URLs. A sample of real entry URLs, sanitized, should be run through the rewrite, and the shadow comparison of the migration guide is the end-to-end check.

## Rollout

Additive and off by default; rollback is removing the section.

## Open questions

- **Which entry forms a deployment really uses.** `index.m3u8` as an entry, `.urlset`, comma lists and nginx's own segment URLs are not covered; each would be a small addition once a real URL shows it is needed.
- **The 93-byte limit.** It comes from the 128-byte ID. A longer URI would need an ID that is a hash with the URI kept beside it, at the cost of state.
- **Tokens.** A deployment that signs URLs with the original URI needs the token claim to name the compatible ID, or a way to verify against the URI.
