# TDD 0018: DRM bridge for nginx-vod-module deployments

- Status: Accepted; implemented for `cbcs`
- Created: 2026-11-22
- Updated: 2026-11-22
- Related ADRs: None
- Related designs: [TDD 0009](0009-common-encryption-and-drm.md) (the `cbcs` encryption this feeds), [TDD 0015](0015-vod-module-mapping-adapter.md) (the mapping dialect this extends), [TDD 0016](0016-adaptive-clips.md) (whole-asset encryption of adaptive clips)

## Summary

A deployment that encrypts with nginx-vod-module does not put keys in its mapping. nginx asks a separate **key service** for each asset's DRM info. This design lets the `vod-module` dialect of the HTTP resolver ask the same service, turn its answer into vodnik's own `encryption` object, and serve the asset encrypted, so a cutover needs no new key service.

It covers `cbcs` only. nginx-vod-module encrypts DASH with `cenc` (AES-CTR); vodnik gives the same key under `cbcs`. Whether that is enough depends on the devices: a client that cannot decrypt `cbcs` (older Widevine and PlayReady clients) cannot play the result. `cenc` output is a separate, larger piece and is not part of this design.

## Context

nginx-vod-module's DRM, from its public documentation and the integration guides of key vendors (never from its source, which is AGPL-3.0, see [the research note](../research/nginx-vod-module.md)):

- With DRM enabled, nginx requests the asset's DRM info from an upstream, with a request URI of the operator's choosing, and encrypts with what comes back.
- The answer is a JSON array. An entry has `key` and `key_id` (base64, 16 bytes each) and a `pssh` list of `{uuid, data}`. `data` is base64 and vendor-specific.
- A leading number of segments can be left clear.

**What this design could not confirm.** The upstream's own repository was not reachable when this was written, so the answer's shape comes from vendor guides that differ in small ways (one adds an `iv`; another wraps the array in an object). The translation is therefore strict about what it needs and tolerant of what it does not, and the first real answer from a deployment becomes a fixture. In particular, whether `pssh.data` is a complete `pssh` box or only its payload is not settled, so both are accepted.

## Goals

- Read a deployment's existing key service unchanged.
- Add no second set of encryption rules: the translation produces a native `encryption` object, which goes through the same validation as any mapper's.
- Pick up a key the service rotates, even when the mapping did not change.
- Never put a key in a log line, an error, or `/admin/status`.

## Non-goals

- `cens` and `cbc1`. `cenc` (AES-CTR) is [TDD 0021](0021-cenc-for-dash.md), chosen with `dash_scheme`.
- More than one key for an asset (per-track keys), and key rotation across the asset's timeline.
- Talking to a licence server. vodnik never does; it encrypts with the key it is given and tells players where the system data is.
- HLS `AES-128` from the key service.

## Design

### Configuration

```toml
[resolver.http]
dialect = "vod-module"
base_url = "https://mapping.internal.example.net"

[resolver.http.vod_module.drm]
path_template = "drm/{id}"      # after base_url; {id} is the asset ID, percent-encoded
extra_args = "key=abc"          # static query string
host_header = "keys.example"    # optional
clear_lead_segments = 0         # the first N segments are left clear
max_response_bytes = 16384
```

The table's presence turns the bridge on; without it every asset is served clear, as before. The request is `GET {base_url}/{path_template}?{extra_args}` with the same bearer token, `Accept` and `X-Request-Id` as the mapping request, and is retried and classified in the same way.

### Translation

`vod_drm::encryption_from_drm_info` is a pure function from the answer to a native `encryption` object:

| The answer has | The `encryption` object has |
| --- | --- |
| `[ {...} ]`, one entry | `scheme: "cbcs"` |
| `key_id`, `key` (base64, 16 bytes) | `keys: [{key_id, key}]`, as 32 hex digits |
| `pssh[i].uuid` (any case, hyphens optional) | `systems[i].system_id`, hyphenated lower case |
| `pssh[i].data`, a complete `pssh` box for that system | `systems[i].pssh`, the box as it is |
| `pssh[i].data`, anything else | `systems[i].pssh`, a version 0 box for that system with `data` as its payload |
| `clear_lead_segments > 0` | `clear_lead_ms = clear_lead_segments * packaging.segment_duration_ms` |

Refused, with a path and never a value: not JSON; not an array; no entry; **more than one entry** (a single key for the whole asset is all vodnik serves); a `key` or `key_id` that is not base64 of 16 bytes; a `uuid` that is not a UUID; a `data` that is not base64, is larger than 16 KiB, or is a `pssh` box for another system; more than eight `pssh` entries. An `iv` and any other field are ignored: vodnik derives its own constant IV.

The object is added to the translated mapping as the answer's `encryption`, so it covers a single file, a ladder, clips and adaptive clips alike (TDD 0016, stage 2). A clear lead is key periods and so cannot be combined with clips; that combination is refused with the existing message.

### Versions and revalidation

A mapping service has no version, so the dialect derives one (TDD 0015). The bridge adds a hash of the `encryption` object to it, so a key or `pssh` the service changes gives a new version and the asset reloads. Without this a rotated key would be "unchanged" and the old one would keep being served. The key service is asked on every revalidation, as the mapping is.

### Failures

| The key service | The result |
| --- | --- |
| Unreachable, `5xx`, `429` | retried, then `503`; covered by stale-if-error like a mapping |
| `404`, `410` | `502`: the asset has no DRM info, and is not served clear instead |
| `401`, `403` | `502`, "refused this service's credentials" |
| A body that cannot be used | `502`, the reason logged with its path |

An asset is never served clear because its key service failed.

## Security and limits

- The answer holds a content key. Errors and logs name a path and a reason, never a value; the response size is capped by `max_response_bytes`; `Debug` output of the encryption object already redacts keys.
- `/admin/status` lists key IDs only.
- The key service is called with the mapper's bearer token and `base_url`, so it is subject to the same https rule. Pointing it elsewhere is a later option if a deployment needs it.

## Observability

A rejected answer logs `drm_info_rejected` with the asset ID and the reason. The existing rejected-answer metric counts it. No new metrics.

## Testing

- **Translation, table-driven:** each row and each refusal with its exact message and no key in any message; a box kept as it is and a payload wrapped; the UUID spellings; fields that are ignored.
- **End to end through the mock services:** the asset is encrypted under the service's key (the `tenc` key ID, HLS and DASH signalling, `/admin/status` key IDs without the key); a key the service rotates reaches an asset whose mapping did not change; the clear lead is counted in segments; a missing, malformed or unavailable answer is `502`, `502` and `503`; the mapper's bearer token is sent; adaptive clips are covered.
- **Mutations:** the version no longer following the key, the clear lead not being scaled by the segment length, and a `pssh` box accepted for another system each fail a test.
- **Not testable here:** a real key service. The first real answer, sanitized, becomes a fixture, and the shadow comparison of the migration guide is the end-to-end check, including playing the `cbcs` result on the deployment's real devices.

## Rollout

Additive: without `[resolver.http.vod_module.drm]` nothing changes, and rollback is removing it.

## Open questions

- **`pssh.data` and the answer's exact shape.** Confirm against a real answer; adapt the translation if a deployment's differs.
- **`cenc`.** Done in [TDD 0021](0021-cenc-for-dash.md): `dash_scheme = "cenc"` serves DASH as AES-CTR and HLS stays `cbcs`. Key rotation and a clear lead work with it.
- **Several keys.** A deployment that uses separate video and audio keys would need `keys` with `tracks`, and a way to tell which entry is which.
- **The clear lead default.** `0` here; nginx-vod-module's own default is not known from its public documentation.
