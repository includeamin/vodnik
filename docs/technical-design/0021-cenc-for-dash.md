# TDD 0021: `cenc` (AES-CTR) for DASH

- Status: Accepted; implemented, including key rotation
- Created: 2026-11-23
- Updated: 2026-11-23
- Related ADRs: None
- Related designs: [TDD 0009](0009-common-encryption-and-drm.md) (the `cbcs` encryption this extends), [TDD 0013](0013-key-rotation.md), [TDD 0018](0018-drm-bridge.md) (which named this as the missing piece)

## Summary

An asset can ask for its DASH segments to be encrypted with `cenc` (AES-CTR) instead of `cbcs`. HLS stays `cbcs`. The key, the DRM systems and the URLs are the same; only what a `/dash/` init segment and media segment contain, and what the manifest says, changes. nginx-vod-module serves DASH under `cenc`, so a device that cannot decrypt `cbcs` (older Widevine and PlayReady clients) can move to vodnik.

## Context

TDD 0009 chose `cbcs` because it is the one scheme every major DRM system accepts on both protocols. The cost is the devices that predate it. `cenc` is accepted by all DASH DRM clients. Apple's players, and so HLS, decrypt nothing but `cbcs`, so a `cenc` offering is DASH-only, and one asset is encrypted two ways depending on the path a player asks for.

**What this design could not confirm.** Which devices need `cenc` was not known when this was built: it is built on the premise that some do, and was verified against FFmpeg's decryptor, not against a device or a DRM vendor's test suite.

## Goals

- A deployment whose DASH players need `cenc` can serve them from the same assets and keys.
- HLS is unchanged.
- No new keys, systems or URLs: a mapper adds one field.
- Segments are byte-identical across replicas, as for `cbcs`.

## Non-goals

- `cenc` over HLS (`SAMPLE-AES-CTR`): few players read it.
- `cens` and `cbc1`.

## Design

### The setting

```json
"encryption": { "scheme": "cbcs", "dash_scheme": "cenc", "keys": [...], "systems": [...] }
```

`scheme` stays `cbcs`: it is what HLS uses, and what everything else is judged by. `dash_scheme` is `cbcs` (the default) or `cenc`. The DRM bridge has the same choice: `[resolver.http.vod_module.drm] dash_scheme = "cenc"`. The scheme is part of the answer's fingerprint, so it changes the URL version.

A request's protocol comes from its path: `/dash/...` is DASH, everything else HLS. The scheme is chosen where a segment or an init segment is made, from the asset's `dash_scheme` and that protocol, so a clip, an adaptive rendition and a single file all follow the same rule.

### What `cenc` is

AES-128 in counter mode (ISO/IEC 23001-7, 10.1):

- **Counter block.** The sample's 8-byte IV, then an 8-byte block counter that starts at zero.
- **Protected bytes.** The same subsample maps as `cbcs` (H.264 and HEVC slice data after each slice header; whole samples for audio), but every protected byte is encrypted, not one block in ten, and the keystream runs on continuously across a sample's protected ranges, through a block that straddles two of them.
- **Per-sample IVs.** `cbcs` has one constant IV in the init segment. `cenc` writes each sample's IV into the fragment's `senc` ahead of its subsamples, and `saiz` includes it, so a video sample's entry is `8 + 2 + 6n` bytes and an audio sample's is 8.
- **Init segment.** `schm` says `cenc`, and `tenc` is version 0 with no pattern, `default_isProtected = 1`, `default_Per_Sample_IV_Size = 8` and the key ID, with no constant IV.

### Deriving the IV

Replicas must agree, so an IV cannot be random. It is the first 8 bytes of SHA-256 over a per-track salt, the sample's offset in its source file, and its decode time. The salt is a hash of the asset's URL version, the key ID and the track. Two samples under one key therefore never share a counter block, and neither do two assets that share a key, because the version differs when the content differs. The same sample always gets the same IV, which makes a segment repeatable. A 64-bit IV from a hash can collide, with a birthday bound of about 2^32 samples under one key and one version, far above any asset's size.

### Key periods

Key rotation and clear periods (TDD 0013) work under `cenc` as they do under `cbcs`. The init segment declares the first encrypted period's key. A fragment under another key says so with a `seig` sample group, and a clear fragment says it is clear with one; each period's `pssh` boxes go in its fragments' `moof`.

The one difference is what a protected group declares. `cbcs` says "protected, 1:9 pattern, constant IV" and carries the 16-byte IV. `cenc` says "protected, no pattern, per-sample IV size 8" and carries no IV, since each sample's own IV is in `senc`. A fragment is given the group that matches the scheme it is encrypted with, so the same period is a `cbcs` entry over HLS and a `cenc` entry over DASH. The key itself is the same.

### Signalling

The DASH `ContentProtection` for `urn:mpeg:dash:mp4protection:2011` says `value="cenc"`, in every period. HLS key lines are unchanged.

## Testing

- The CTR mode against NIST SP 800-38A's vector, across ranges split at arbitrary points, and as an IV followed by a block count from zero.
- The init segment's `schm` and `tenc`, and a fragment's `senc`, `saiz` and `saio` for video and audio.
- The setting: default, `cenc`, an unknown value, a key change, a clear lead; the version it changes.
- Rotation: over DASH the clear fragment says it is clear, the init segment's key needs no group, and a new key's fragment carries a 20-byte `seig` entry (protected, IV size 8, key ID, no constant IV) while the same fragment over HLS carries the 37-byte `cbcs` entry. FFmpeg decodes each encrypted period under its own key and fails under the other's.
- End to end: DASH and HLS inits and segments of one asset carry different schemes; every sample has a distinct IV; segments are repeatable; clips and adaptive sets follow the rule.
- **FFmpeg decryption.** FFmpeg decodes the served DASH video and audio as `cenc` with every frame, fails with a wrong key and without one, and still decodes HLS as `cbcs`. This is an independent implementation of the cipher, the subsample handling and the boxes.

Three limits of that evidence. FFmpeg cannot read a clear fragment inside an encrypted track (it tries to decrypt it; `cbcs` behaves the same), so a clear lead is checked by structure, and FFmpeg decodes only what follows it. FFmpeg also takes one key for a stream and does not follow `seig` groups, so each encrypted period is decoded on its own, after the init segment, under that period's key; a player that follows the groups is not tested here. And FFmpeg is not a DRM client: the real test is a device.

## Open questions

- `cenc` for HLS, if a device ever needs it.
- Playback on the real devices that motivated this.
