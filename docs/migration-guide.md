# Migrating from nginx-vod-module

This guide is for someone who runs nginx-vod-module today and wants vodnik to take its place without changing players, the mapping service, or the CDN. It follows the order of a real migration: find out what you use, translate the configuration, run both, compare, switch, and keep a way back.

**What this guide is built from.** It was written from nginx-vod-module's documented directives and mapping format, not from a real deployment's `nginx.conf`. The tables say what vodnik does with each directive, but a setting that is in your configuration and not here deserves a question before you migrate. Where a behaviour could not be confirmed it says so.

## 1. Find out what you use

Go through your `nginx.conf` and a sample of mapping responses, and mark which of these apply. Each row says where to read about vodnik's side.

| You use | In vodnik |
| --- | --- |
| Mapped mode (`vod_mode mapped`) with a mapping service | The `vod-module` dialect of the HTTP resolver: [TDD 0015](technical-design/0015-vod-module-mapping-adapter.md) |
| Local mode (`vod_mode local`), files named by the URL | `[url_compat]` with a mapping service that answers for a path, or an `[assets]` catalog; [TDD 0019](technical-design/0019-url-compatibility.md) |
| Remote mode (`vod_mode remote`) | Not a mode. A mapping answer with `sourceType: http`, or a native `http` location |
| `/hls/.../master.m3u8` and `/dash/.../manifest.mpd` entry URLs | `[url_compat]` |
| Adaptive sets, several clips per sequence, `durations`, `clipFrom`/`clipTo`, `concat` | Translated: [TDD 0015](technical-design/0015-vod-module-mapping-adapter.md) and [TDD 0016](technical-design/0016-adaptive-clips.md) |
| Language and label of audio sequences, `tracks` | Translated |
| Subtitles (`.vtt`, `.srt`, `.ttml`, `.dfxp` sequences) | Translated; see [supported input](supported-input.md) for the formats |
| `vod_drm_upstream_location` with AES-CBC / `cbcs` | The DRM bridge: [TDD 0018](technical-design/0018-drm-bridge.md) |
| `vod_drm_upstream_location` with AES-CTR / `cenc` | `dash_scheme = "cenc"`: DASH as AES-CTR, HLS as `cbcs`; [TDD 0021](technical-design/0021-cenc-for-dash.md). Key rotation and a clear lead work |
| HLS AES-128 | Native: [TDD 0012](technical-design/0012-hls-aes-128.md) |
| `vod_fallback_upstream_location` | `[fallback]`: [TDD 0020](technical-design/0020-fallback-on-miss.md) |
| Live or simulated live (`playlistType` other than `vod`) | **Not supported** |
| Filter clips (`rateFilter`, `gainFilter`, `mixFilter`), `silence`, `dynamic` clips | **Not supported**; the adapter refuses them by name |
| `.urlset` or comma-separated files in one URL | **Not supported**; give the set from the mapper as renditions |
| `notifications`, `closedCaptions`, a `shift` timing offset, DASH `SegmentList` | **Not supported**; see the [gap table](roadmap.md#gaps-not-listed-above) |

Every "not supported" row is a stop sign for that part of your catalog, not for the migration: those assets can stay on nginx-vod-module (see [step 6](#6-cut-over-slowly)) while the rest moves.

## 2. Translate the configuration

Directives that map to settings:

| nginx-vod-module | vodnik |
| --- | --- |
| `vod_upstream_location`, mapped mode | `[resolver.http] base_url`, `dialect = "vod-module"`, and `[resolver.http.vod_module] path_template` |
| `vod_upstream_extra_args` | `[resolver.http.vod_module] extra_args`; nginx variables inside it are not expanded |
| `vod_upstream_host_header` | `[resolver.http.vod_module] host_header` |
| `vod_remote_upstream_location` | `[resolver.http.vod_module] remote_base_url` |
| `vod_drm_upstream_location` | `[resolver.http.vod_module.drm] path_template` |
| `vod_drm_clear_lead_segment_count` | `[resolver.http.vod_module.drm] clear_lead_segments` |
| `vod_fallback_upstream_location` | `[fallback] base_url` |
| `vod_proxy_header_name` | `[fallback] loop_header`, when the fallback is another nginx-vod-module |
| `vod_hls_container_format` | Not a setting: vodnik always writes fragmented MP4 ([ADR 0001](adr/0001-use-fragmented-mp4-for-media-segments.md)); `mpegts` is not offered |
| `vod_segment_duration` | `packaging.segment_duration_ms`; a mapping's `segmentDuration` is accepted only when it equals this |
| `vod_align_segments_to_key_frames`, `vod_manifest_segment_durations_mode` | Not settings: segments are always cut on key frames and the playlist carries exact durations |
| `vod_bootstrap_segment_durations`, `vod_segment_count_policy`, `vod_ignore_edit_list` | No counterpart yet: see the [gap table](roadmap.md#gaps-not-listed-above) |
| The mapping response size limit | `[resolver.http] max_response_bytes` |
| Caches (`vod_metadata_cache`, `vod_response_cache`, `vod_mapping_cache`) | None to size: vodnik keeps what it needs in memory, bounded by `[limits]` and `registry.max_cached_resolutions`; put the CDN in front for responses |
| nginx `location` rules, `add_header`, CORS headers | `[cors]`, and the CDN or a proxy for the rest |
| Token or secure-link checks in nginx | `[authorization]` ([TDD 0007](technical-design/0007-viewer-authorization.md)); the formats are not the same, so tokens must be reissued |

A minimal configuration for a deployment that has a mapping service, `/vod` in front of its URLs, and a second datacenter:

```toml
[server]
listen = "0.0.0.0:8080"

[storage]
media_root = "/data/media"

[resolver]
type = "http"

[resolver.http]
dialect = "vod-module"
base_url = "https://mapping.internal.example.net"
max_response_bytes = 65536

[resolver.http.vod_module]
path_template = "vod/{id}"
file_root = "/data/media"

[url_compat]
enabled = true
path_prefix = "/vod"

[fallback]
enabled = true
base_url = "https://dc2.example.net"
```

Start it and read `/admin/status`: it reports the dialect and whether the mapping service answers. A mapping vodnik cannot honour is refused with the JSON path of the field, in the log and as a `502` for that asset, which is how you find the rest of your catalog's surprises.

## 3. What a player sees

Players keep their entry URL, and every URL inside a manifest is made by vodnik and relative to it. What changes is the content of the manifests, so test the players you have rather than assuming:

- **Segments are fragmented MP4**, in HLS as well as DASH. A player that only handles MPEG-TS in HLS needs checking first. Older devices are the usual cases.
- **Segment URLs differ.** Anything that pattern-matches nginx's `seg-1-v1-a1.ts` or `fragment-1-f1-v1-a1.m4s` (a CDN rule, a log parser, a token scheme) needs changing. The CDN's cache rules should key on the full path.
- **Durations are exact and boundaries are on key frames.** Segment counts and lengths can differ from nginx's for the same file.
- **Query strings** are passed to the mapping service and carried on the URLs vodnik makes; the asset's content version is in the URL as `?v=`.
- **Encryption** is `cbcs` for HLS. For DASH, `dash_scheme = "cenc"` gives AES-CTR as nginx-vod-module does; it was verified against FFmpeg, not a real device, so test your DASH players with it.

## 4. Test before traffic

1. Pick a sample of assets that covers every shape in step 1: a single file, an adaptive set, clips with `durations`, a file with captions, an encrypted one, one that is missing.
2. Fetch the same entry URL from both servers and compare the manifests by their structure: renditions, languages, codecs, segment counts, durations. Byte equality is not the goal, since the formats differ.
3. Play each in the players you support, from start, with seeks, and across ad or clip boundaries.
4. For encrypted assets, check each DRM system's license flow end to end, not only that the manifest has a `PSSH`.
5. Run the project's conformance checks against your own samples ([protocol conformance](conformance.md)).

## 5. Compare on real traffic

Before any viewer is served by vodnik, run both behind the same CDN and compare. This is the part that tells you whether the mapping is right for your whole catalog, not a sample.

1. Mirror a share of requests to vodnik without using its answers: a CDN or proxy "shadow" or "mirror" rule, which sends a copy and discards the response.
2. Compare, per asset: the status code, the number of renditions and tracks, the duration, and for a sample the manifest structure. A `502` from vodnik that nginx served is a mapping it could not honour, and its log line names the field.
3. Compare error rates, latency, and CPU and memory per request ([benchmarks](benchmarks.md) says what to expect).
4. Fix or exclude what differs. An asset that cannot move is added to the list that stays on nginx-vod-module.
5. Stop when the differences have been explained for a full cycle of your traffic, including its busiest hour and its rarest assets.

## 6. Cut over slowly

Move a small share of real viewers, then more, choosing by asset or by client rather than by request, so one viewer never switches servers mid-playback. Keep nginx-vod-module serving the assets that need what vodnik lacks. Two ways to do that:

- Route by path or asset list at the CDN or proxy.
- Point vodnik's `[fallback]` at nginx-vod-module for what vodnik does not have. This only helps for a miss, not for an asset vodnik refuses (a `502`), so use it for catalogs that are split, not as a safety net for unsupported mappings.

## 7. Keep a way back

- Keep nginx-vod-module configured and running at low scale until a full cycle of traffic has gone through vodnik with no surprises.
- A CDN or load balancer rule that returns the traffic is faster than a deploy. Test the rule before you need it.
- Watch the error rate, the `vod_resolver_*` and `vod_asset_loads_total` metrics, and the `request_rejected`, `upstream_failed` and `fallback_served` log events ([operating the origin](operations.md)).

## What to bring back

If a step above fails on something that is not in this guide, the most useful thing is the real input: the directive from `nginx.conf`, the mapping response that was refused, the DRM answer, the URL a player requested. Each of those changes the compatibility layer, and each was guessed at from documentation here.
