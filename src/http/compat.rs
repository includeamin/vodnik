//! Entry URLs in nginx-vod-module's shape, which carry a file URI that may hold slashes
//! (`docs/technical-design/0019-url-compatibility.md`).
//!
//! vodnik's own routes name an asset by one path segment: `/hls/{id}/master.m3u8`. A deployment
//! that moves over keeps URLs like `/hls/2020/05/movie.mp4/master.m3u8`. This module rewrites such
//! a path, before routing, to the native route for the asset ID that stands for the URI
//! (`compat_id`). It touches nothing else: what a manifest links to is relative to the URL it was
//! fetched from, so every URL a player follows from an entry URL is rewritten the same way.

use axum::http::Uri;

use crate::compat_id;
use crate::config::is_valid_asset_id;

/// The rewrite, with the prefix an operator's proxy or `location` puts before `/hls` and `/dash`.
#[derive(Debug, Clone)]
pub(crate) struct UrlCompat {
    prefix: String,
}

impl UrlCompat {
    /// `prefix` is stripped from the front of every path, such as `/vod`; empty strips nothing.
    pub(crate) fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.trim_end_matches('/').to_owned(),
        }
    }

    /// The URI to route instead of `uri`, or `None` to route `uri` as it is: it is not an entry
    /// URL of this shape, it already names an asset the native way, or it cannot be served
    /// (and then routing it unchanged gives the usual `404`).
    pub(crate) fn rewrite(&self, uri: &Uri) -> Option<Uri> {
        let rest = uri.path().strip_prefix(&self.prefix)?.strip_prefix('/')?;
        let (hls, rest) = match rest.split_once('/')? {
            ("hls", rest) => (true, rest),
            ("dash", rest) => (false, rest),
            _ => return None,
        };
        let segments = rest.split('/').collect::<Vec<_>>();
        let suffix = suffix_len(hls, &segments)?;
        let (file, suffix) = segments.split_at(segments.len() - suffix);
        let file = file
            .iter()
            .map(|segment| percent_decode(segment))
            .collect::<Option<Vec<_>>>()?
            .join("/");
        // One segment that is already an asset ID is a native URL, and stays one.
        if is_valid_asset_id(&file) {
            return None;
        }
        let id = compat_id::encode(&file)?;
        let kind = if hls { "hls" } else { "dash" };
        let mut rewritten = format!("/{kind}/{id}/{}", suffix.join("/"));
        if let Some(query) = uri.query() {
            rewritten.push('?');
            rewritten.push_str(query);
        }
        rewritten.parse().ok()
    }
}

/// How many trailing segments of `segments` are the native part of the path: what follows the
/// asset ID in vodnik's routes. The last segment tells which route it is, and the ones before it
/// are checked against that route's fixed words. `None` when the path is not one of them.
fn suffix_len(hls: bool, segments: &[&str]) -> Option<usize> {
    let last = *segments.last()?;
    let before = |back: usize| {
        segments
            .len()
            .checked_sub(back)
            .and_then(|at| segments.get(at))
            .copied()
    };
    let len = match (last, hls) {
        ("master.m3u8", true) | ("manifest.mpd", false) => 1,
        ("iframes.m3u8", true) if before(2) == Some("video") => 2,
        ("index.m3u8", true) => {
            if before(3) == Some("subtitles") {
                3
            } else {
                2
            }
        }
        ("sub.vtt", _) if before(3) == Some("subtitles") => 3,
        ("init.mp4", _) => {
            if before(3) == Some("clips") {
                4
            } else {
                2
            }
        }
        ("media.m4s", _)
            if before(3) == Some("segments") || (hls && before(3) == Some("iframes")) =>
        {
            4
        }
        _ => return None,
    };
    // What precedes the suffix is the file URI, and there must be one.
    (segments.len() > len).then_some(len)
}

/// A path segment with its `%XX` escapes decoded. `None` for a bad escape, bytes that are not
/// UTF-8, or an escaped slash, which would make one segment into two.
fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte == b'%' {
            let pair = segment.get(at + 1..at + 3)?;
            if !pair.bytes().all(|digit| digit.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(pair, 16).ok()?);
            at += 3;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    (!decoded.contains('/')).then_some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(compat: &UrlCompat, uri: &str) -> Option<String> {
        compat
            .rewrite(&uri.parse().unwrap())
            .map(|uri| uri.to_string())
    }

    fn id(uri: &str) -> String {
        compat_id::encode(uri).unwrap()
    }

    #[test]
    fn every_url_a_player_follows_from_an_entry_url_is_rewritten() {
        let compat = UrlCompat::new("");
        let a = id("2020/05/movie.mp4");
        for (given, native) in [
            ("/hls/2020/05/movie.mp4/master.m3u8", "master.m3u8"),
            (
                "/hls/2020/05/movie.mp4/video/index.m3u8",
                "video/index.m3u8",
            ),
            (
                "/hls/2020/05/movie.mp4/video-720p/index.m3u8",
                "video-720p/index.m3u8",
            ),
            (
                "/hls/2020/05/movie.mp4/audio-1/init.mp4",
                "audio-1/init.mp4",
            ),
            (
                "/hls/2020/05/movie.mp4/video/clips/1/init.mp4",
                "video/clips/1/init.mp4",
            ),
            (
                "/hls/2020/05/movie.mp4/video/segments/12/media.m4s",
                "video/segments/12/media.m4s",
            ),
            (
                "/hls/2020/05/movie.mp4/video/iframes.m3u8",
                "video/iframes.m3u8",
            ),
            (
                "/hls/2020/05/movie.mp4/video/iframes/3/media.m4s",
                "video/iframes/3/media.m4s",
            ),
            (
                "/hls/2020/05/movie.mp4/subtitles/en/index.m3u8",
                "subtitles/en/index.m3u8",
            ),
            (
                "/hls/2020/05/movie.mp4/subtitles/en/sub.vtt",
                "subtitles/en/sub.vtt",
            ),
            ("/dash/2020/05/movie.mp4/manifest.mpd", "manifest.mpd"),
            (
                "/dash/2020/05/movie.mp4/video-a/init.mp4",
                "video-a/init.mp4",
            ),
            (
                "/dash/2020/05/movie.mp4/video-a/clips/0/init.mp4",
                "video-a/clips/0/init.mp4",
            ),
            (
                "/dash/2020/05/movie.mp4/audio-1/segments/4/media.m4s",
                "audio-1/segments/4/media.m4s",
            ),
            (
                "/dash/2020/05/movie.mp4/subtitles/en/sub.vtt",
                "subtitles/en/sub.vtt",
            ),
        ] {
            let kind = if given.starts_with("/hls/") {
                "hls"
            } else {
                "dash"
            };
            assert_eq!(
                rewritten(&compat, given).as_deref(),
                Some(format!("/{kind}/{a}/{native}").as_str()),
                "{given}"
            );
        }
    }

    #[test]
    fn the_query_string_is_kept() {
        let compat = UrlCompat::new("");
        assert_eq!(
            rewritten(
                &compat,
                "/hls/a/b.mp4/video/segments/1/media.m4s?v=ab12&auth=t%20k"
            )
            .as_deref(),
            Some(
                format!(
                    "/hls/{}/video/segments/1/media.m4s?v=ab12&auth=t%20k",
                    id("a/b.mp4")
                )
                .as_str()
            )
        );
    }

    #[test]
    fn a_native_asset_id_and_anything_else_is_left_alone() {
        let compat = UrlCompat::new("");
        for given in [
            "/hls/movie/master.m3u8",
            "/hls/my_movie-2/video/index.m3u8",
            "/dash/movie/manifest.mpd",
            "/health",
            "/ready",
            "/metrics",
            "/admin/status",
            "/hls/a/b/unknown.txt",
            "/hls/a/b/master.mpd",
            "/dash/a/b/master.m3u8",
            "/hls/master.m3u8",
            "/hls/",
            "/other/a/b/master.m3u8",
            "/",
        ] {
            assert_eq!(rewritten(&compat, given), None, "{given}");
        }
    }

    #[test]
    fn a_single_segment_that_is_not_an_id_is_still_a_uri() {
        let compat = UrlCompat::new("");
        assert_eq!(
            rewritten(&compat, "/hls/movie.mp4/master.m3u8").as_deref(),
            Some(format!("/hls/{}/master.m3u8", id("movie.mp4")).as_str())
        );
    }

    #[test]
    fn percent_escapes_are_decoded_once_into_the_uri() {
        let compat = UrlCompat::new("");
        assert_eq!(
            rewritten(&compat, "/hls/a%20b/c%C3%BC.mp4/master.m3u8").as_deref(),
            Some(format!("/hls/{}/master.m3u8", id("a b/cü.mp4")).as_str())
        );
    }

    #[test]
    fn a_uri_that_cannot_be_passed_on_is_not_rewritten() {
        let compat = UrlCompat::new("");
        for given in [
            "/hls/a/%2E%2E/b/master.m3u8",
            "/hls/a/../b/master.m3u8",
            "/hls/a/./b/master.m3u8",
            "/hls//a/master.m3u8",
            "/hls/a//b/master.m3u8",
            "/hls/a%2Fb/c/master.m3u8",
            "/hls/a%00/b/master.m3u8",
            "/hls/a%0A/b/master.m3u8",
            "/hls/%FF/b/master.m3u8",
            "/hls/a%+1/b/master.m3u8",
            "/hls/a%2/b/master.m3u8",
            &format!("/hls/{}/b/master.m3u8", "x".repeat(100)),
        ] {
            assert_eq!(rewritten(&compat, given), None, "{given}");
        }
    }

    #[test]
    fn a_prefix_is_stripped_first() {
        let compat = UrlCompat::new("/vod/");
        assert_eq!(
            rewritten(&compat, "/vod/hls/a/b.mp4/master.m3u8").as_deref(),
            Some(format!("/hls/{}/master.m3u8", id("a/b.mp4")).as_str())
        );
        assert_eq!(rewritten(&compat, "/hls/a/b.mp4/master.m3u8"), None);
        assert_eq!(rewritten(&compat, "/other/hls/a/b.mp4/master.m3u8"), None);
    }

    #[test]
    fn what_is_rewritten_is_a_valid_asset_id() {
        let compat = UrlCompat::new("");
        let uri = rewritten(&compat, "/hls/a/b.mp4/master.m3u8").unwrap();
        let segment = uri.split('/').nth(2).unwrap();
        assert!(is_valid_asset_id(segment), "{segment}");
    }
}
