//! Asset IDs for file URIs that an nginx-vod-module deployment uses in its URLs
//! (`docs/technical-design/0019-url-compatibility.md`).
//!
//! An nginx URL names a file by a URI that may hold slashes and dots, such as
//! `2020/05/movie.mp4`. A vodnik asset ID may not, because it becomes a path segment of every
//! URL and a key of every cache. The two are joined by a reversible ID: the reserved prefix
//! `vm--` and the base64url form of the URI, which is made of letters, digits, `-` and `_` like
//! any other ID. The HTTP layer makes one from a request, and the `vod-module` dialect of the
//! mapper client turns it back into the path of its mapping request.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Marks an ID that stands for a file URI.
pub(crate) const PREFIX: &str = "vm--";
/// The longest ID the server accepts (`is_valid_asset_id`).
const MAX_ID_BYTES: usize = 128;

/// Whether `uri` is a file URI this server will pass on: segments separated by single slashes,
/// none empty, `.` or `..`, and no control character, so it cannot climb out of what the mapper
/// serves or smuggle a header or a NUL into the mapping request.
pub(crate) fn is_valid_uri(uri: &str) -> bool {
    !uri.is_empty()
        && uri
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
        && !uri.chars().any(char::is_control)
}

/// The ID for `uri`, or `None` when the URI is not acceptable or its ID would be longer than an
/// ID may be (a URI of up to 93 bytes fits).
pub(crate) fn encode(uri: &str) -> Option<String> {
    if !is_valid_uri(uri) {
        return None;
    }
    let id = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(uri));
    (id.len() <= MAX_ID_BYTES).then_some(id)
}

/// The URI an ID stands for, or `None` for an ID that is not one or whose URI is not acceptable.
/// An ID can be typed by anyone, so the URI is checked again here.
pub(crate) fn decode(id: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(id.strip_prefix(PREFIX)?).ok()?;
    let uri = String::from_utf8(bytes).ok()?;
    is_valid_uri(&uri).then_some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::is_valid_asset_id;

    #[test]
    fn a_uri_becomes_a_valid_id_and_comes_back() {
        for uri in [
            "a/b.mp4",
            "2020/05/movie.mp4",
            "movie.mp4",
            "naïve/ü.mp4",
            "a b/c",
        ] {
            let id = encode(uri).expect(uri);
            assert!(id.starts_with("vm--"), "{id}");
            assert!(is_valid_asset_id(&id), "{id}");
            assert_eq!(decode(&id).as_deref(), Some(uri));
        }
        assert_eq!(encode("a/b").unwrap(), "vm--YS9i");
    }

    #[test]
    fn what_cannot_be_passed_on_has_no_id() {
        for uri in [
            "", "/a", "a/", "a//b", "a/./b", "a/../b", "..", ".", "a/\u{0}b", "a/\nb", "a/\u{7f}",
        ] {
            assert_eq!(encode(uri), None, "{uri:?}");
        }
    }

    #[test]
    fn the_longest_uri_that_fits_is_93_bytes() {
        assert!(encode(&"x".repeat(93)).is_some());
        assert_eq!(encode(&"x".repeat(94)), None);
    }

    #[test]
    fn a_typed_id_is_checked_like_a_made_one() {
        use base64::Engine;
        let typed = |uri: &str| format!("vm--{}", URL_SAFE_NO_PAD.encode(uri));
        assert_eq!(decode(&typed("a/../b")), None);
        assert_eq!(decode(&typed("a//b")), None);
        assert_eq!(decode(&typed("a\u{0}")), None);
        assert_eq!(decode("vm--!!"), None, "not base64");
        assert_eq!(decode("movie"), None, "not an id of this kind");
        assert_eq!(decode("vm--"), None, "no uri");
        assert_eq!(decode("vm--_w"), None, "not UTF-8");
    }
}
