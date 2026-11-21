//! Translates an nginx-vod-module mapping response into vodnik's own mapper answer
//! (`docs/technical-design/0015-vod-module-mapping-adapter.md`).
//!
//! The translation is a pure function of the mapping JSON and a few settings. Its output goes
//! through the same parsing and validation as any native answer, so this module adds one step and
//! no second set of rules. A field it cannot honor is refused with its JSON path, and no message
//! ever carries a value: mapping responses hold signed URLs and encryption keys.

use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::mapper::is_rendition_id;
use super::policy::validate_relative_path;

/// What the translation needs besides the mapping itself.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) asset_id: String,
    /// Prefix of a file `path`, removed to make it relative to `storage.media_root`.
    pub(crate) file_root: Option<String>,
    /// Where `sourceType: http` paths live.
    pub(crate) remote_base_url: Option<String>,
    /// JSON paths or field names accepted and dropped instead of refused.
    pub(crate) ignore: Vec<String>,
    /// `packaging.segment_duration_ms`; a mapping's `segmentDuration` must equal it.
    pub(crate) segment_duration_ms: u64,
    pub(crate) max_clips: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Translated {
    /// A native answer, without `ttl_seconds`.
    pub(crate) answer: Value,
    /// `false` when the mapping said `"cache": false`.
    pub(crate) cache: bool,
}

/// Bumped whenever the translation changes, so every asset reloads under a new version.
const TRANSLATOR_VERSION: u32 = 1;
/// nginx-vod-module serves only the first video and the first audio track unless told otherwise.
const DEFAULT_TRACKS: &str = "v1-a1";
/// nginx-vod-module's own limit on sequences.
const MAX_SEQUENCES: usize = 32;
const CAPTION_EXTENSIONS: [&str; 4] = ["vtt", "srt", "ttml", "dfxp"];
/// Clip types that need decoding or a second upstream, which vodnik does not have.
const UNSUPPORTED_CLIP_TYPES: [&str; 5] = [
    "rateFilter",
    "gainFilter",
    "mixFilter",
    "silence",
    "dynamic",
];

/// Fields each level reads. Fields vodnik computes itself, or has no use for, are accepted and
/// dropped; anything else is refused unless the operator listed it in `ignore`.
const SET_KEYS: [&str; 7] = [
    "sequences",
    "durations",
    "clipFrom",
    "clipTo",
    "cache",
    "playlistType",
    "segmentDuration",
];
const SET_DROPPED: [&str; 6] = [
    "discontinuity",
    "consistentSequenceMediaInfo",
    "referenceClipIndex",
    "keyFrameDurations",
    "firstKeyFrameOffset",
    "id",
];
const SEQUENCE_KEYS: [&str; 5] = ["clips", "id", "language", "label", "default"];
const SEQUENCE_DROPPED: [&str; 2] = ["bitrate", "avg_bitrate"];
const CLIP_KEYS: [&str; 8] = [
    "type",
    "path",
    "sourceType",
    "tracks",
    "clipFrom",
    "basePath",
    "paths",
    "offset",
];

/// One file of a sequence, after `concat` clips are expanded.
struct Piece {
    location: Value,
    tracks: String,
    from_ms: u64,
}

struct Window {
    clip_from: Option<u64>,
    clip_to: Option<u64>,
    durations: Option<Vec<u64>>,
}

pub(crate) fn translate(body: &[u8], settings: &Settings) -> Result<Translated, String> {
    let root: Value = serde_json::from_slice(body).map_err(|error| {
        format!(
            "mapping is not valid JSON (line {}, column {})",
            error.line(),
            error.column()
        )
    })?;
    let root = root.as_object().ok_or("mapping must be a JSON object")?;
    check_keys(root, "", &SET_KEYS, &SET_DROPPED, settings)?;
    if root
        .get("playlistType")
        .is_some_and(|kind| kind.as_str() != Some("vod"))
    {
        return Err("playlistType is not supported unless it is vod".to_owned());
    }
    if let Some(duration) = number(root, "segmentDuration", "segmentDuration")?
        && duration != settings.segment_duration_ms
    {
        return Err(format!(
            "segmentDuration must equal packaging.segment_duration_ms ({})",
            settings.segment_duration_ms
        ));
    }
    let sequences = root
        .get("sequences")
        .and_then(Value::as_array)
        .filter(|sequences| !sequences.is_empty())
        .ok_or("sequences must be a non-empty array")?;
    if sequences.len() > MAX_SEQUENCES {
        return Err(format!("sequences has more than {MAX_SEQUENCES} entries"));
    }
    let window = Window {
        clip_from: number(root, "clipFrom", "clipFrom")?,
        clip_to: number(root, "clipTo", "clipTo")?,
        durations: durations(root)?,
    };

    let mut media = Vec::new();
    let mut subtitles = Vec::new();
    for (index, sequence) in sequences.iter().enumerate() {
        let path = format!("sequences[{index}]");
        let sequence = sequence
            .as_object()
            .ok_or_else(|| format!("{path} must be an object"))?;
        check_keys(sequence, &path, &SEQUENCE_KEYS, &SEQUENCE_DROPPED, settings)?;
        let pieces = pieces(sequence, &path, settings)?;
        if let [piece] = pieces.as_slice()
            && is_caption(piece)
        {
            subtitles.push(subtitle(sequence, &path, piece)?);
        } else {
            media.push((sequence, path, pieces));
        }
    }

    let mut answer = serde_json::Map::new();
    answer.insert("asset_id".to_owned(), settings.asset_id.clone().into());
    media_answer(&media, &window, &mut answer)?;
    let has_clips = answer.contains_key("clips")
        || answer
            .get("renditions")
            .and_then(Value::as_array)
            .is_some_and(|renditions| renditions.iter().any(|entry| entry.get("clips").is_some()));
    if !subtitles.is_empty() && has_clips {
        return Err("captions cannot be combined with clips".to_owned());
    }
    if !subtitles.is_empty() {
        answer.insert("subtitles".to_owned(), subtitles.into());
    }
    let mut answer = Value::Object(answer);
    answer["version"] = version(&answer).into();
    Ok(Translated {
        answer,
        cache: root.get("cache") != Some(&Value::Bool(false)),
    })
}

type Sequence<'a> = (&'a serde_json::Map<String, Value>, String, Vec<Piece>);

/// Chooses `location`, `renditions` or `clips` for the media sequences (the table of TDD 0015).
fn media_answer(
    media: &[Sequence<'_>],
    window: &Window,
    answer: &mut serde_json::Map<String, Value>,
) -> Result<(), String> {
    match media {
        [] => return Err("sequences has no media, only captions".to_owned()),
        [(sequence, path, pieces)] => {
            let overrides = overrides(sequence, path)?;
            let cut = pieces.len() > 1
                || window.clip_from.is_some()
                || window.clip_to.is_some()
                || window.durations.is_some()
                || pieces[0].from_ms > 0;
            if cut && !overrides.is_empty() {
                return Err(format!(
                    "{path}: language, label and default cannot be combined with a window"
                ));
            }
            if cut {
                answer.insert("clips".to_owned(), clips(pieces, window)?.into());
            } else if overrides.is_empty() {
                answer.insert("location".to_owned(), pieces[0].location.clone());
                answer.insert("tracks".to_owned(), pieces[0].tracks.clone().into());
            } else {
                let mut rendition = rendition("s1".to_owned(), &pieces[0]);
                rendition.extend(overrides);
                answer.insert(
                    "renditions".to_owned(),
                    vec![Value::Object(rendition)].into(),
                );
            }
        }
        many => {
            // Every sequence is cut the same way, so they must have the same number of clips.
            let (_, first_path, first) = &many[0];
            if let Some((_, path, pieces)) = many.iter().find(|(_, _, p)| p.len() != first.len()) {
                return Err(format!(
                    "{path}: has {} clips, but {first_path} has {}",
                    pieces.len(),
                    first.len()
                ));
            }
            let cut = first.len() > 1
                || window.clip_from.is_some()
                || window.clip_to.is_some()
                || window.durations.is_some()
                || many.iter().any(|(_, _, pieces)| pieces[0].from_ms > 0);
            let ids = rendition_ids(many.iter().map(|(sequence, ..)| *sequence));
            let mut renditions = Vec::new();
            for ((sequence, path, pieces), id) in many.iter().zip(ids) {
                let mut entry = if cut {
                    let mut entry = serde_json::Map::new();
                    entry.insert("id".to_owned(), id.into());
                    entry.insert("clips".to_owned(), clips(pieces, window)?.into());
                    entry
                } else {
                    rendition(id, &pieces[0])
                };
                entry.extend(overrides(sequence, path)?);
                renditions.push(Value::Object(entry));
            }
            answer.insert("renditions".to_owned(), renditions.into());
        }
    }
    Ok(())
}

/// Refuses a field the level does not read, unless it is dropped by design or ignored by the
/// operator (by its JSON path or its bare name). Only the path is ever named, never a value.
fn check_keys(
    object: &serde_json::Map<String, Value>,
    path: &str,
    accepted: &[&str],
    dropped: &[&str],
    settings: &Settings,
) -> Result<(), String> {
    for key in object.keys() {
        let full = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if accepted.contains(&key.as_str()) {
            continue;
        }
        let ignored = settings
            .ignore
            .iter()
            .any(|entry| *entry == full || entry == key);
        if dropped.contains(&key.as_str()) || ignored {
            tracing::debug!(field = %full, "mapping field dropped");
            continue;
        }
        return Err(format!("{full} is not supported"));
    }
    Ok(())
}

/// A numeric field, in milliseconds.
fn number(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<u64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{path} must be a non-negative integer")),
    }
}

fn durations(root: &serde_json::Map<String, Value>) -> Result<Option<Vec<u64>>, String> {
    let Some(value) = root.get("durations") else {
        return Ok(None);
    };
    value
        .as_array()
        .and_then(|list| list.iter().map(Value::as_u64).collect::<Option<Vec<_>>>())
        .filter(|list| list.iter().all(|duration| *duration > 0))
        .map(Some)
        .ok_or_else(|| "durations must be an array of positive integers".to_owned())
}

fn pieces(
    sequence: &serde_json::Map<String, Value>,
    path: &str,
    settings: &Settings,
) -> Result<Vec<Piece>, String> {
    let clips = sequence
        .get("clips")
        .and_then(Value::as_array)
        .filter(|clips| !clips.is_empty())
        .ok_or_else(|| format!("{path}.clips must be a non-empty array"))?;
    let mut pieces = Vec::new();
    for (index, clip) in clips.iter().enumerate() {
        let path = format!("{path}.clips[{index}]");
        let clip = clip
            .as_object()
            .ok_or_else(|| format!("{path} must be an object"))?;
        let kind = clip.get("type").and_then(Value::as_str);
        if let Some(kind) = kind.filter(|kind| UNSUPPORTED_CLIP_TYPES.contains(kind)) {
            return Err(format!("{path}.type \"{kind}\" is not supported"));
        }
        check_keys(clip, &path, &CLIP_KEYS, &[], settings)?;
        if clip
            .get("offset")
            .is_some_and(|offset| offset.as_u64() != Some(0))
        {
            return Err(format!("{path}.offset must be 0"));
        }
        let tracks = clip
            .get("tracks")
            .map_or(Ok(DEFAULT_TRACKS), |tracks| {
                tracks
                    .as_str()
                    .ok_or_else(|| format!("{path}.tracks must be a string"))
            })?
            .to_owned();
        let from_ms = number(clip, "clipFrom", &format!("{path}.clipFrom"))?.unwrap_or(0);
        let source_type = clip.get("sourceType").and_then(Value::as_str);
        let mut push = |raw: &str| -> Result<(), String> {
            pieces.push(Piece {
                location: location(raw, source_type, &path, settings)?,
                tracks: tracks.clone(),
                from_ms,
            });
            Ok(())
        };
        match kind {
            Some("source") => push(
                clip.get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{path}.path must be a string"))?,
            )?,
            Some("concat") => {
                let base = clip.get("basePath").and_then(Value::as_str).unwrap_or("");
                let paths = clip
                    .get("paths")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("{path}.paths must be an array"))?;
                for entry in paths {
                    let entry = entry
                        .as_str()
                        .ok_or_else(|| format!("{path}.paths must hold strings"))?;
                    push(&format!("{base}{entry}"))?;
                }
            }
            _ => return Err(format!("{path}.type is not supported")),
        }
        if pieces.len() > settings.max_clips {
            return Err(format!(
                "{path}: more than limits.max_clips ({}) clips",
                settings.max_clips
            ));
        }
    }
    Ok(pieces)
}

fn location(
    raw: &str,
    source_type: Option<&str>,
    path: &str,
    settings: &Settings,
) -> Result<Value, String> {
    let remote = match source_type {
        Some("http") => true,
        Some("file") => false,
        Some(_) => return Err(format!("{path}.sourceType is not supported")),
        None => settings.remote_base_url.is_some(),
    };
    if raw == "empty" {
        return Err(format!("{path}.path \"empty\" is not supported"));
    }
    if remote {
        let url = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_owned()
        } else {
            let base = settings.remote_base_url.as_deref().ok_or_else(|| {
                format!("{path}: an http source needs vod_module.remote_base_url")
            })?;
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                raw.trim_start_matches('/')
            )
        };
        return Ok(json!({"type": "http", "url": url}));
    }
    let relative = match &settings.file_root {
        Some(root) => raw
            .strip_prefix(root.trim_end_matches('/'))
            .map(|rest| rest.trim_start_matches('/'))
            .ok_or_else(|| format!("{path}.path is not under vod_module.file_root"))?,
        None => raw,
    };
    let relative =
        validate_relative_path(relative).map_err(|reason| format!("{path}.path: {reason}"))?;
    Ok(json!({"type": "file", "path": relative.to_string_lossy()}))
}

fn is_caption(piece: &Piece) -> bool {
    let name = piece.location["path"]
        .as_str()
        .or_else(|| piece.location["url"].as_str())
        .unwrap_or("");
    let name = name.split(['?', '#']).next().unwrap_or(name);
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        CAPTION_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

fn subtitle(
    sequence: &serde_json::Map<String, Value>,
    path: &str,
    piece: &Piece,
) -> Result<Value, String> {
    let language = sequence
        .get("language")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}.language is required for a caption"))?;
    let mut entry = json!({
        "language": language,
        "default": sequence.get("default") == Some(&Value::Bool(true)),
        "forced": false,
        "location": piece.location,
    });
    if let Some(label) = sequence.get("label").and_then(Value::as_str) {
        entry["label"] = label.into();
    }
    Ok(entry)
}

fn rendition(id: String, piece: &Piece) -> serde_json::Map<String, Value> {
    let mut entry = serde_json::Map::new();
    entry.insert("id".to_owned(), id.into());
    entry.insert("location".to_owned(), piece.location.clone());
    entry.insert("tracks".to_owned(), piece.tracks.clone().into());
    entry
}

/// The sequence's `language`, `label` and `default`, as the fields a rendition carries.
fn overrides(
    sequence: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let mut fields = serde_json::Map::new();
    for key in ["language", "label"] {
        if let Some(value) = sequence.get(key) {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{path}.{key} must be a string"))?;
            let text = if key == "language" {
                text.to_ascii_lowercase()
            } else {
                text.to_owned()
            };
            fields.insert(key.to_owned(), text.into());
        }
    }
    if sequence.get("default") == Some(&Value::Bool(true)) {
        fields.insert("default".to_owned(), true.into());
    }
    Ok(fields)
}

/// The sequence `id` when it is a usable rendition ID and unique, else its position.
fn rendition_ids<'a>(
    sequences: impl Iterator<Item = &'a serde_json::Map<String, Value>>,
) -> Vec<String> {
    let ids: Vec<Option<&str>> = sequences
        .map(|sequence| {
            sequence
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| is_rendition_id(id))
        })
        .collect();
    ids.iter()
        .enumerate()
        .map(|(index, id)| match id {
            Some(id) if ids.iter().filter(|other| *other == &Some(*id)).count() == 1 => {
                (*id).to_owned()
            }
            _ => format!("s{}", index + 1),
        })
        .collect()
}

fn clips(pieces: &[Piece], window: &Window) -> Result<Vec<Value>, String> {
    if let Some(durations) = &window.durations
        && durations.len() != pieces.len()
    {
        return Err("durations must have one entry per clip".to_owned());
    }
    if pieces.len() > 1 && window.durations.is_none() {
        return Err("durations is required when a sequence has several clips".to_owned());
    }
    if pieces.len() > 1 && (window.clip_from.is_some() || window.clip_to.is_some()) {
        return Err("clipFrom and clipTo apply to a single clip only".to_owned());
    }
    pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let from_ms = window.clip_from.unwrap_or(piece.from_ms);
            let to_ms = match &window.durations {
                Some(durations) => Some(from_ms + durations[index]),
                None => window.clip_to,
            };
            let mut clip = json!({
                "location": piece.location,
                "from_ms": from_ms,
                "tracks": piece.tracks,
            });
            if let Some(to_ms) = to_ms {
                clip["to_ms"] = to_ms.into();
            }
            Ok(clip)
        })
        .collect()
}

/// A hash of what decides the output. A URL's query is left out, so a service that signs its
/// URLs afresh on every request does not change the version.
fn version(answer: &Value) -> String {
    let mut identity = answer.clone();
    strip_queries(&mut identity);
    let mut hasher = Sha256::new();
    hasher.update(format!("vod-module/{TRANSLATOR_VERSION}\n"));
    hasher.update(identity.to_string());
    format!("vm{TRANSLATOR_VERSION}-{}", hex(&hasher.finalize()[..16]))
}

/// Mixes the size and modification time of every local file the answer names into its version,
/// so a replaced file is noticed at the next revalidation, as nginx notices when its caches
/// expire. A file that cannot be read is left out: loading it fails later with the real reason.
pub(crate) async fn mix_in_file_identity(answer: &mut Value, media_root: &std::path::Path) {
    let mut paths = Vec::new();
    collect_file_paths(answer, &mut paths);
    let root = media_root.to_owned();
    let stats = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter_map(|path| {
                let metadata = std::fs::metadata(root.join(&path)).ok()?;
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| elapsed.as_nanos());
                Some(format!("{path}\n{}\n{modified}\n", metadata.len()))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let seen = !stats.is_empty();
    let mut hasher = Sha256::new();
    for line in &stats {
        hasher.update(line);
    }
    if seen && let Some(Value::String(version)) = answer.get_mut("version") {
        version.push('-');
        version.push_str(&hex(&hasher.finalize()[..8]));
    }
}

fn collect_file_paths(value: &Value, paths: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("file")
                && let Some(path) = map.get("path").and_then(Value::as_str)
            {
                paths.push(path.to_owned());
            }
            map.values()
                .for_each(|inner| collect_file_paths(inner, paths));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|inner| collect_file_paths(inner, paths)),
        _ => {}
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn strip_queries(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(url)) = map.get_mut("url")
                && let Ok(mut parsed) = Url::parse(url)
            {
                parsed.set_query(None);
                parsed.set_fragment(None);
                *url = parsed.into();
            }
            map.values_mut().for_each(strip_queries);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_queries),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn settings() -> Settings {
        Settings {
            asset_id: "movie".to_owned(),
            file_root: Some("/data/media".to_owned()),
            remote_base_url: None,
            ignore: Vec::new(),
            segment_duration_ms: 6000,
            max_clips: 64,
        }
    }

    fn run(mapping: &Value) -> Result<Translated, String> {
        translate(mapping.to_string().as_bytes(), &settings())
    }

    fn answer(mapping: &Value) -> Value {
        let mut answer = run(mapping).expect("translates").answer;
        answer.as_object_mut().unwrap().remove("version");
        answer
    }

    fn source(path: &str) -> Value {
        json!({"type": "source", "path": path})
    }

    fn single(path: &str) -> Value {
        json!({"sequences": [{"clips": [source(path)]}]})
    }

    #[test]
    fn a_lone_file_becomes_a_location_with_the_first_video_and_audio_track() {
        assert_eq!(
            answer(&single("/data/media/movie/main.mp4")),
            json!({
                "asset_id": "movie",
                "location": {"type": "file", "path": "movie/main.mp4"},
                "tracks": "v1-a1",
            })
        );
    }

    #[test]
    fn a_clips_own_tracks_replace_the_default() {
        let mapping = json!({"sequences": [{"clips": [
            {"type": "source", "path": "/data/media/a.mp4", "tracks": "v1-a2"}]}]});
        assert_eq!(answer(&mapping)["tracks"], "v1-a2");
    }

    #[test]
    fn a_remote_path_is_joined_to_the_remote_base() {
        let mut s = settings();
        s.remote_base_url = Some("https://origin.example.net".to_owned());
        let mapping = json!({"sequences": [{"clips": [
            {"type": "source", "sourceType": "http", "path": "/v/a.mp4"}]}]});
        let got = translate(mapping.to_string().as_bytes(), &s)
            .unwrap()
            .answer;
        assert_eq!(
            got["location"],
            json!({"type": "http", "url": "https://origin.example.net/v/a.mp4"})
        );
        let absolute = json!({"sequences": [{"clips": [
            {"type": "source", "sourceType": "http", "path": "https://cdn.example.net/a.mp4"}]}]});
        let got = translate(absolute.to_string().as_bytes(), &s)
            .unwrap()
            .answer;
        assert_eq!(
            got["location"],
            json!({"type": "http", "url": "https://cdn.example.net/a.mp4"})
        );
    }

    #[test]
    fn several_sequences_become_renditions_named_by_id_or_position() {
        let mapping = json!({"sequences": [
            {"id": "v720", "clips": [{"type": "source", "path": "/data/media/m/720.mp4", "tracks": "v1"}]},
            {"clips": [{"type": "source", "path": "/data/media/m/en.mp4", "tracks": "a1"}],
             "language": "eng", "label": "English", "default": true},
        ]});
        assert_eq!(
            answer(&mapping),
            json!({
                "asset_id": "movie",
                "renditions": [
                    {"id": "v720", "location": {"type": "file", "path": "m/720.mp4"}, "tracks": "v1"},
                    {"id": "s2", "location": {"type": "file", "path": "m/en.mp4"}, "tracks": "a1",
                     "language": "eng", "label": "English", "default": true},
                ],
            })
        );
    }

    #[test]
    fn a_lone_sequence_with_a_language_becomes_one_rendition() {
        let mapping = json!({"sequences": [{"language": "fra",
            "clips": [source("/data/media/a.mp4")]}]});
        assert_eq!(
            answer(&mapping)["renditions"],
            json!([{"id": "s1", "location": {"type": "file", "path": "a.mp4"},
                    "tracks": "v1-a1", "language": "fra"}])
        );
    }

    #[test]
    fn clips_take_their_window_from_durations_and_clip_from() {
        let mapping = json!({"durations": [10000, 60000], "sequences": [{"clips": [
            source("/data/media/ad.mp4"),
            {"type": "source", "path": "/data/media/movie.mp4", "clipFrom": 5000}]}]});
        assert_eq!(
            answer(&mapping)["clips"],
            json!([
                {"location": {"type": "file", "path": "ad.mp4"}, "from_ms": 0, "to_ms": 10000, "tracks": "v1-a1"},
                {"location": {"type": "file", "path": "movie.mp4"}, "from_ms": 5000, "to_ms": 65000, "tracks": "v1-a1"},
            ])
        );
    }

    #[test]
    fn a_set_level_window_cuts_a_single_file() {
        let mapping = json!({"clipFrom": 1000, "clipTo": 9000, "sequences": [{"clips": [
            source("/data/media/a.mp4")]}]});
        assert_eq!(
            answer(&mapping)["clips"],
            json!([{"location": {"type": "file", "path": "a.mp4"}, "from_ms": 1000, "to_ms": 9000, "tracks": "v1-a1"}])
        );
    }

    #[test]
    fn a_concat_clip_is_one_clip_per_path_under_its_base() {
        let mapping = json!({"durations": [1000, 2000], "sequences": [{"clips": [
            {"type": "concat", "basePath": "/data/media/parts", "paths": ["/a.mp4", "/b.mp4"]}]}]});
        let got = answer(&mapping);
        assert_eq!(got["clips"][0]["location"]["path"], "parts/a.mp4");
        assert_eq!(got["clips"][1]["location"]["path"], "parts/b.mp4");
        assert_eq!(got["clips"][1]["to_ms"], 2000);
    }

    #[test]
    fn a_caption_sequence_becomes_a_subtitle() {
        let mapping = json!({"sequences": [
            {"clips": [source("/data/media/m/main.mp4")]},
            {"language": "eng", "label": "English", "clips": [source("/data/media/m/en.vtt")]},
        ]});
        assert_eq!(
            answer(&mapping),
            json!({
                "asset_id": "movie",
                "location": {"type": "file", "path": "m/main.mp4"},
                "tracks": "v1-a1",
                "subtitles": [{"language": "eng", "label": "English", "default": false,
                               "forced": false, "location": {"type": "file", "path": "m/en.vtt"}}],
            })
        );
    }

    #[test]
    fn the_version_follows_what_decides_the_output() {
        let version = |mapping: &Value| run(mapping).unwrap().answer["version"].clone();
        let a = single("/data/media/a.mp4");
        assert_eq!(version(&a), version(&a));
        assert_ne!(version(&a), version(&single("/data/media/b.mp4")));
        let tracked = json!({"sequences": [{"clips": [
            {"type": "source", "path": "/data/media/a.mp4", "tracks": "a1"}]}]});
        assert_ne!(version(&a), version(&tracked));
        let two = |first: &str, second: &str| {
            json!({"sequences": [
                {"id": "x", "clips": [source(first)]}, {"id": "y", "clips": [source(second)]}]})
        };
        assert_ne!(
            version(&two("/data/media/a.mp4", "/data/media/b.mp4")),
            version(&two("/data/media/b.mp4", "/data/media/a.mp4"))
        );
    }

    #[test]
    fn caching_can_be_turned_off_by_the_mapping() {
        let mut mapping = single("/data/media/a.mp4");
        assert!(run(&mapping).unwrap().cache);
        mapping["cache"] = json!(false);
        assert!(!run(&mapping).unwrap().cache);
    }

    fn refused(mapping: &Value) -> String {
        run(mapping).expect_err("is refused")
    }

    fn with(mut mapping: Value, key: &str, value: Value) -> Value {
        mapping[key] = value;
        mapping
    }

    fn with_clip(key: &str, value: Value) -> Value {
        let mut mapping = single("/data/media/a.mp4");
        mapping["sequences"][0]["clips"][0][key] = value;
        mapping
    }

    #[test]
    fn a_field_the_adapter_cannot_honor_is_refused_with_its_json_path() {
        let base = single("/data/media/a.mp4");
        for (mapping, expected) in [
            (
                with(base.clone(), "playlistType", json!("live")),
                "playlistType is not supported unless it is vod",
            ),
            (
                with(base.clone(), "firstClipTime", json!(1)),
                "firstClipTime is not supported",
            ),
            (
                with(base.clone(), "notifications", json!([])),
                "notifications is not supported",
            ),
            (
                with(base.clone(), "closedCaptions", json!([])),
                "closedCaptions is not supported",
            ),
            (
                with(base.clone(), "clipIds", json!([1])),
                "clipIds is not supported",
            ),
            (
                with(base, "segmentDuration", json!(4000)),
                "segmentDuration must equal packaging.segment_duration_ms (6000)",
            ),
            (
                with_clip("encryptionKey", json!("00112233445566778899aabbccddeeff")),
                "sequences[0].clips[0].encryptionKey is not supported",
            ),
            (
                with_clip("encryptionScheme", json!("cenc")),
                "sequences[0].clips[0].encryptionScheme is not supported",
            ),
            (
                with_clip("type", json!("rateFilter")),
                "sequences[0].clips[0].type \"rateFilter\" is not supported",
            ),
            (
                with_clip("type", json!("silence")),
                "sequences[0].clips[0].type \"silence\" is not supported",
            ),
            (
                with_clip("type", json!("mystery")),
                "sequences[0].clips[0].type is not supported",
            ),
            (
                with_clip("path", json!("empty")),
                "sequences[0].clips[0].path \"empty\" is not supported",
            ),
            (
                with_clip("path", json!("/elsewhere/a.mp4")),
                "sequences[0].clips[0].path is not under vod_module.file_root",
            ),
            (
                with_clip("path", json!("/data/media/../secret.mp4")),
                "sequences[0].clips[0].path: location path must be relative and free of `.` and `..`",
            ),
            (
                with_clip("surprise", json!(1)),
                "sequences[0].clips[0].surprise is not supported",
            ),
        ] {
            assert_eq!(refused(&mapping), expected);
        }
        let concat = json!({"durations": [1000], "sequences": [{"clips": [
            {"type": "concat", "paths": ["/data/media/a.mp4"], "offset": 500}]}]});
        assert_eq!(refused(&concat), "sequences[0].clips[0].offset must be 0");
    }

    #[test]
    fn a_refusal_never_carries_a_value() {
        let mapping = with_clip("encryptionKey", json!("00112233445566778899aabbccddeeff"));
        assert!(!refused(&mapping).contains("00112233"));
        let signed = with_clip("path", json!("https://x.example/a.mp4?sig=SECRETSIG"));
        assert!(!refused(&signed).contains("SECRETSIG"));
        let mut s = settings();
        s.file_root = None;
        let body = with_clip("sourceType", json!("SECRETKIND")).to_string();
        assert!(
            !translate(body.as_bytes(), &s)
                .unwrap_err()
                .contains("SECRETKIND")
        );
    }

    #[test]
    fn fields_vodnik_computes_are_accepted_and_dropped() {
        let mut mapping = single("/data/media/a.mp4");
        for (key, value) in [
            ("discontinuity", json!(false)),
            ("consistentSequenceMediaInfo", json!(true)),
            ("referenceClipIndex", json!(0)),
            ("keyFrameDurations", json!([2000])),
            ("firstKeyFrameOffset", json!(0)),
            ("id", json!("x")),
            ("segmentDuration", json!(6000)),
            ("playlistType", json!("vod")),
        ] {
            mapping[key] = value;
        }
        mapping["sequences"][0]["bitrate"] = json!({"v": 1});
        mapping["sequences"][0]["avg_bitrate"] = json!({"v": 1});
        assert_eq!(answer(&mapping), answer(&single("/data/media/a.mp4")));
    }

    #[test]
    fn the_operator_can_accept_dropping_a_refused_field() {
        let mapping = with(single("/data/media/a.mp4"), "notifications", json!([]));
        let mut s = settings();
        s.ignore = vec!["notifications".to_owned()];
        assert!(translate(mapping.to_string().as_bytes(), &s).is_ok());
        s.ignore = vec!["sequences[0].clips[0].surprise".to_owned()];
        let by_path = with_clip("surprise", json!(1));
        assert!(translate(by_path.to_string().as_bytes(), &s).is_ok());
        // It relaxes a refused field, never an unsupported clip type.
        s.ignore = vec!["type".to_owned()];
        let filter = with_clip("type", json!("rateFilter"));
        assert!(translate(filter.to_string().as_bytes(), &s).is_err());
    }

    #[test]
    fn windows_that_cannot_be_honored_are_refused() {
        let two = |extra: Value| {
            let mut mapping = json!({"sequences": [{"clips": [
                source("/data/media/a.mp4"), source("/data/media/b.mp4")]}]});
            for (key, value) in extra.as_object().unwrap() {
                mapping[key] = value.clone();
            }
            mapping
        };
        assert_eq!(
            refused(&two(json!({}))),
            "durations is required when a sequence has several clips"
        );
        assert_eq!(
            refused(&two(json!({"durations": [1000]}))),
            "durations must have one entry per clip"
        );
        assert_eq!(
            refused(&two(json!({"durations": [1000, 0]}))),
            "durations must be an array of positive integers"
        );
        assert_eq!(
            refused(&two(json!({"durations": [1000, 1000], "clipFrom": 5}))),
            "clipFrom and clipTo apply to a single clip only"
        );
        let overridden = json!({"clipFrom": 1000, "sequences": [{"language": "eng",
            "clips": [source("/data/media/a.mp4")]}]});
        assert_eq!(
            refused(&overridden),
            "sequences[0]: language, label and default cannot be combined with a window"
        );
    }

    #[test]
    fn sequences_of_several_clips_become_renditions_that_are_sequences() {
        let mapping = json!({"durations": [10000, 60000], "sequences": [
            {"id": "v720", "clips": [
                {"type": "source", "path": "/data/media/ad/720.mp4", "tracks": "v1"},
                {"type": "source", "path": "/data/media/m/720.mp4", "tracks": "v1", "clipFrom": 5000}]},
            {"language": "eng", "label": "English", "default": true, "clips": [
                {"type": "source", "path": "/data/media/ad/en.mp4", "tracks": "a1"},
                {"type": "source", "path": "/data/media/m/en.mp4", "tracks": "a1", "clipFrom": 5000}]},
        ]});
        assert_eq!(
            answer(&mapping),
            json!({
                "asset_id": "movie",
                "renditions": [
                    {"id": "v720", "clips": [
                        {"location": {"type": "file", "path": "ad/720.mp4"}, "from_ms": 0, "to_ms": 10000, "tracks": "v1"},
                        {"location": {"type": "file", "path": "m/720.mp4"}, "from_ms": 5000, "to_ms": 65000, "tracks": "v1"}]},
                    {"id": "s2", "language": "eng", "label": "English", "default": true, "clips": [
                        {"location": {"type": "file", "path": "ad/en.mp4"}, "from_ms": 0, "to_ms": 10000, "tracks": "a1"},
                        {"location": {"type": "file", "path": "m/en.mp4"}, "from_ms": 5000, "to_ms": 65000, "tracks": "a1"}]},
                ],
            })
        );
    }

    #[test]
    fn a_window_on_a_ladder_trims_every_rendition() {
        let mapping = json!({"clipFrom": 1000, "clipTo": 9000, "sequences": [
            {"id": "a", "clips": [source("/data/media/a.mp4")]},
            {"id": "b", "clips": [source("/data/media/b.mp4")]}]});
        let got = answer(&mapping);
        for (rendition, file) in [(0, "a.mp4"), (1, "b.mp4")] {
            assert_eq!(
                got["renditions"][rendition]["clips"],
                json!([{"location": {"type": "file", "path": file}, "from_ms": 1000, "to_ms": 9000, "tracks": "v1-a1"}])
            );
        }
        let by_duration = json!({"durations": [4000], "sequences": [
            {"id": "a", "clips": [source("/data/media/a.mp4")]},
            {"id": "b", "clips": [source("/data/media/b.mp4")]}]});
        assert_eq!(
            answer(&by_duration)["renditions"][1]["clips"][0]["to_ms"],
            4000
        );
    }

    #[test]
    fn sequences_with_different_numbers_of_clips_are_refused_naming_the_second() {
        let mapping = json!({"durations": [1000, 1000], "sequences": [
            {"clips": [source("/data/media/a.mp4"), source("/data/media/b.mp4")]},
            {"clips": [source("/data/media/c.mp4")]}]});
        assert_eq!(
            refused(&mapping),
            "sequences[1]: has 1 clips, but sequences[0] has 2"
        );
    }

    #[test]
    fn captions_beside_clips_are_refused() {
        let mapping = json!({"durations": [1000, 1000], "sequences": [
            {"clips": [source("/data/media/a.mp4"), source("/data/media/b.mp4")]},
            {"language": "eng", "clips": [source("/data/media/en.vtt")]}]});
        assert_eq!(refused(&mapping), "captions cannot be combined with clips");
    }

    #[test]
    fn counts_are_limited() {
        let many = json!({"sequences": (0..33)
            .map(|n| json!({"id": format!("r{n}"), "clips": [source("/data/media/a.mp4")]}))
            .collect::<Vec<_>>()});
        assert_eq!(refused(&many), "sequences has more than 32 entries");
        let mut s = settings();
        s.max_clips = 1;
        let two = json!({"durations": [1, 1], "sequences": [{"clips": [
            source("/data/media/a.mp4"), source("/data/media/b.mp4")]}]});
        assert!(
            translate(two.to_string().as_bytes(), &s)
                .unwrap_err()
                .contains("max_clips")
        );
    }

    #[test]
    fn a_body_that_is_not_a_mapping_is_refused() {
        assert!(
            translate(b"[]", &settings())
                .unwrap_err()
                .contains("object")
        );
        assert!(
            translate(b"{", &settings())
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert_eq!(
            refused(&json!({"sequences": []})),
            "sequences must be a non-empty array"
        );
    }

    #[tokio::test]
    async fn a_replaced_file_changes_the_version_and_an_untouched_one_does_not() {
        let directory = crate::testutil::ScratchDir::new("vod-module-identity");
        std::fs::write(directory.path().join("a.mp4"), b"first").unwrap();
        let version = |answer: &Value| answer["version"].as_str().unwrap().to_owned();
        let identity = || async {
            let mut answer = run(&single("/data/media/a.mp4")).unwrap().answer;
            mix_in_file_identity(&mut answer, directory.path()).await;
            version(&answer)
        };
        let before = identity().await;
        assert_eq!(before, identity().await);
        std::fs::write(directory.path().join("a.mp4"), b"second, longer").unwrap();
        assert_ne!(before, identity().await);
        // A file that is not there leaves the version as the translation made it.
        let mut answer = run(&single("/data/media/missing.mp4")).unwrap().answer;
        let plain = version(&answer);
        mix_in_file_identity(&mut answer, directory.path()).await;
        assert_eq!(plain, version(&answer));
    }
}
