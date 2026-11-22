//! Translates the DRM info an nginx-vod-module deployment's key service returns into vodnik's own
//! `encryption` object (`docs/technical-design/0018-drm-bridge.md`).
//!
//! The service answers with a JSON array whose entry holds a `key` and a `key_id` (base64, 16
//! bytes each) and a `pssh` list of `{uuid, data}`. vodnik encrypts with `cbcs` only, so the
//! bridge gives the same key under that scheme. Like the mapping translator, this is a pure
//! function, and its errors name a path and never a value: the answer holds a content key.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Most `pssh` entries one key may have; `encryption` allows eight systems.
const MAX_SYSTEMS: usize = 8;
const MAX_PSSH_BYTES: usize = 16 * 1024;

pub(crate) fn encryption_from_drm_info(
    body: &[u8],
    clear_lead_ms: u64,
    dash_cenc: bool,
) -> Result<Value, String> {
    let info: Value = serde_json::from_slice(body).map_err(|error| {
        format!(
            "DRM info is not valid JSON (line {}, column {})",
            error.line(),
            error.column()
        )
    })?;
    let entries = info.as_array().ok_or("DRM info must be a JSON array")?;
    let entry = match entries.as_slice() {
        [] => return Err("DRM info has no key".to_owned()),
        [entry] => entry,
        many => {
            return Err(format!(
                "DRM info has {} keys; only one key for the whole asset is supported",
                many.len()
            ));
        }
    };
    let entry = entry.as_object().ok_or("[0] must be an object")?;
    let key = sixteen_bytes(entry.get("key"), "[0].key")?;
    let key_id = sixteen_bytes(entry.get("key_id"), "[0].key_id")?;
    let pssh = match entry.get("pssh") {
        None | Some(Value::Null) => &[][..],
        Some(list) => list.as_array().ok_or("[0].pssh must be an array")?,
    };
    if pssh.len() > MAX_SYSTEMS {
        return Err(format!("[0].pssh has more than {MAX_SYSTEMS} entries"));
    }
    let systems = pssh
        .iter()
        .enumerate()
        .map(|(at, system)| system_of(system, at))
        .collect::<Result<Vec<_>, _>>()?;
    let mut encryption = json!({
        "scheme": "cbcs",
        "keys": [{"key_id": hex(&key_id), "key": hex(&key)}],
        "systems": systems,
    });
    if clear_lead_ms > 0 {
        encryption["clear_lead_ms"] = clear_lead_ms.into();
    }
    if dash_cenc {
        encryption["dash_scheme"] = "cenc".into();
    }
    Ok(encryption)
}

/// A base64 field that must decode to 16 bytes.
fn sixteen_bytes(value: Option<&Value>, path: &str) -> Result<[u8; 16], String> {
    let bytes = value
        .and_then(Value::as_str)
        .and_then(|text| STANDARD.decode(text).ok())
        .ok_or_else(|| format!("{path} must be a base64 string"))?;
    <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| format!("{path} must be 16 bytes"))
}

/// One `pssh` entry as a native system. `data` is either a complete `pssh` box for the entry's
/// system, kept as it is, or the payload of one, which is wrapped in a version 0 box.
fn system_of(system: &Value, at: usize) -> Result<Value, String> {
    let path = format!("[0].pssh[{at}]");
    let uuid = system
        .get("uuid")
        .and_then(Value::as_str)
        .and_then(uuid_bytes)
        .ok_or_else(|| format!("{path}.uuid is not a UUID"))?;
    let data = system
        .get("data")
        .and_then(Value::as_str)
        .and_then(|text| STANDARD.decode(text).ok())
        .ok_or_else(|| format!("{path}.data must be a base64 string"))?;
    if data.len() > MAX_PSSH_BYTES {
        return Err(format!("{path}.data is larger than {MAX_PSSH_BYTES} bytes"));
    }
    let boxed = match box_system(&data) {
        Some(owner) if owner == uuid => data,
        Some(_) => return Err(format!("{path}.data is a pssh box for another system")),
        None => wrap(&uuid, &data),
    };
    Ok(json!({"system_id": uuid_string(&uuid), "pssh": STANDARD.encode(boxed)}))
}

/// The system a complete `pssh` box belongs to, or `None` when `bytes` is not exactly one box.
fn box_system(bytes: &[u8]) -> Option<[u8; 16]> {
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    if usize::try_from(size).ok()? != bytes.len() || bytes.get(4..8)? != b"pssh" {
        return None;
    }
    bytes.get(12..28)?.try_into().ok()
}

fn wrap(uuid: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let size = u32::try_from(32 + data.len()).unwrap_or(u32::MAX);
    let mut boxed = size.to_be_bytes().to_vec();
    boxed.extend_from_slice(b"pssh");
    boxed.extend_from_slice(&[0; 4]);
    boxed.extend_from_slice(uuid);
    boxed.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    boxed.extend_from_slice(data);
    boxed
}

/// 16 bytes from a UUID with or without hyphens, in either case.
fn uuid_bytes(text: &str) -> Option<[u8; 16]> {
    let digits: Vec<u8> = text.bytes().filter(|byte| *byte != b'-').collect();
    if digits.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (slot, pair) in out.iter_mut().zip(digits.chunks(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *slot = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}

fn uuid_string(uuid: &[u8; 16]) -> String {
    let hex = hex(uuid);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A short hash of an `encryption` object, mixed into an asset's version so that a re-keyed asset
/// is reloaded even though its mapping did not change.
pub(crate) fn fingerprint(encryption: &Value) -> String {
    hex(&Sha256::digest(encryption.to_string())[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = *b"0123456789abcdef";
    const KEY_ID: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    const WIDEVINE: &str = "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";
    const PLAYREADY: &str = "9a04f079-9840-4286-ab92-e65be0885f95";

    fn b64(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    fn uuid_bytes(uuid: &str) -> Vec<u8> {
        let hex = uuid.replace('-', "");
        (0..16)
            .map(|at| u8::from_str_radix(&hex[at * 2..at * 2 + 2], 16).unwrap())
            .collect()
    }

    /// A version 0 `pssh` box.
    fn pssh_box(uuid: &str, data: &[u8]) -> Vec<u8> {
        let mut boxed = u32::try_from(32 + data.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        boxed.extend_from_slice(b"pssh");
        boxed.extend_from_slice(&[0; 4]);
        boxed.extend_from_slice(&uuid_bytes(uuid));
        boxed.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        boxed.extend_from_slice(data);
        boxed
    }

    fn entry(pssh: &[(&str, String)]) -> Value {
        json!({
            "key": b64(&KEY),
            "key_id": b64(&KEY_ID),
            "pssh": pssh.iter().map(|(uuid, data)| json!({"uuid": uuid, "data": data}))
                .collect::<Vec<_>>(),
        })
    }

    fn run(info: &Value) -> Result<Value, String> {
        encryption_from_drm_info(info.to_string().as_bytes(), 0, false)
    }

    #[test]
    fn one_key_becomes_a_cbcs_encryption_with_its_systems() {
        let payload = b"widevine-data";
        let playready = pssh_box(PLAYREADY, b"playready-data");
        let info = json!([entry(&[
            (WIDEVINE, b64(payload)),
            (PLAYREADY, b64(&playready)),
        ])]);
        assert_eq!(
            run(&info).unwrap(),
            json!({
                "scheme": "cbcs",
                "keys": [{
                    "key_id": "0123456789abcdef0123456789abcdef",
                    "key": "30313233343536373839616263646566",
                }],
                "systems": [
                    // A bare payload is wrapped into a pssh box for its system.
                    {"system_id": WIDEVINE, "pssh": b64(&pssh_box(WIDEVINE, payload))},
                    // A complete box for the same system is kept as it is.
                    {"system_id": PLAYREADY, "pssh": b64(&playready)},
                ],
            })
        );
    }

    #[test]
    fn a_uuid_without_hyphens_or_in_capitals_is_the_same_system() {
        let plain = WIDEVINE.replace('-', "").to_uppercase();
        let info = json!([entry(&[(&plain, b64(b"x"))])]);
        assert_eq!(run(&info).unwrap()["systems"][0]["system_id"], WIDEVINE);
    }

    #[test]
    fn a_clear_lead_is_passed_on() {
        let info = json!([entry(&[])]).to_string();
        let got = encryption_from_drm_info(info.as_bytes(), 12_000, false).unwrap();
        assert_eq!(got["clear_lead_ms"], 12_000);
        let none = encryption_from_drm_info(info.as_bytes(), 0, false).unwrap();
        assert!(none.get("clear_lead_ms").is_none());
    }

    #[test]
    fn cenc_for_dash_is_passed_on_and_left_out_otherwise() {
        let info = json!([entry(&[])]).to_string();
        let on = encryption_from_drm_info(info.as_bytes(), 0, true).unwrap();
        assert_eq!(on["dash_scheme"], "cenc");
        assert_eq!(on["scheme"], "cbcs", "HLS stays cbcs");
        let off = encryption_from_drm_info(info.as_bytes(), 0, false).unwrap();
        assert!(off.get("dash_scheme").is_none());
    }

    #[test]
    fn a_key_without_systems_is_still_a_key() {
        let got = run(&json!([entry(&[])])).unwrap();
        assert_eq!(got["systems"], json!([]));
    }

    #[test]
    fn fields_vodnik_does_not_use_are_ignored() {
        let mut one = entry(&[(WIDEVINE, b64(b"x"))]);
        one["iv"] = json!(b64(&[7; 16]));
        one["status"] = json!("ok");
        assert_eq!(
            run(&json!([one])).unwrap()["keys"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn what_cannot_be_honored_is_refused_with_a_path_and_no_value() {
        let good = || entry(&[(WIDEVINE, b64(b"x"))]);
        let with = |key: &str, value: Value| {
            let mut one = good();
            one[key] = value;
            json!([one])
        };
        let mut without_key = good();
        without_key.as_object_mut().unwrap().remove("key");
        let mut mismatch = good();
        mismatch["pssh"][0]["data"] = json!(b64(&pssh_box(PLAYREADY, b"x")));
        for (info, expected) in [
            (json!({"key": "x"}), "DRM info must be a JSON array"),
            (json!([]), "DRM info has no key"),
            (
                json!([good(), good()]),
                "DRM info has 2 keys; only one key for the whole asset is supported",
            ),
            (json!(["text"]), "[0] must be an object"),
            (json!([without_key]), "[0].key must be a base64 string"),
            (
                with("key", json!(b64(&[1; 15]))),
                "[0].key must be 16 bytes",
            ),
            (
                with("key_id", json!(b64(&[1; 17]))),
                "[0].key_id must be 16 bytes",
            ),
            (
                with("key", json!("not base64!")),
                "[0].key must be a base64 string",
            ),
            (
                with("pssh", json!([{"uuid": "nonsense", "data": b64(b"x")}])),
                "[0].pssh[0].uuid is not a UUID",
            ),
            (
                with("pssh", json!([{"uuid": WIDEVINE}])),
                "[0].pssh[0].data must be a base64 string",
            ),
            (
                json!([mismatch]),
                "[0].pssh[0].data is a pssh box for another system",
            ),
            (
                with(
                    "pssh",
                    json!(
                        (0..9)
                            .map(|n| json!({"uuid": format!("{n:032x}"), "data": b64(b"x")}))
                            .collect::<Vec<_>>()
                    ),
                ),
                "[0].pssh has more than 8 entries",
            ),
            (
                with(
                    "pssh",
                    json!([{"uuid": WIDEVINE, "data": b64(&vec![0; MAX_PSSH_BYTES + 1])}]),
                ),
                "[0].pssh[0].data is larger than 16384 bytes",
            ),
        ] {
            assert_eq!(run(&info).unwrap_err(), expected, "{info}");
        }
        assert!(
            encryption_from_drm_info(b"{", 0, false)
                .unwrap_err()
                .starts_with("DRM info is not valid JSON")
        );
        for secret in [b64(&KEY), "30313233343536373839616263646566".to_owned()] {
            for info in [
                with("key_id", json!(b64(&[1; 17]))),
                with("key", json!(b64(&[1; 15]))),
                json!([good(), good()]),
            ] {
                assert!(!run(&info).unwrap_err().contains(&secret));
            }
        }
    }

    #[test]
    fn the_fingerprint_follows_the_key_and_the_systems() {
        let one = run(&json!([entry(&[(WIDEVINE, b64(b"x"))])])).unwrap();
        assert_eq!(fingerprint(&one), fingerprint(&one.clone()));
        let mut rekeyed = one.clone();
        rekeyed["keys"][0]["key"] = json!("ffffffffffffffffffffffffffffffff");
        assert_ne!(fingerprint(&one), fingerprint(&rekeyed));
        let mut other_system = one.clone();
        other_system["systems"][0]["pssh"] = json!(b64(&pssh_box(WIDEVINE, b"y")));
        assert_ne!(fingerprint(&one), fingerprint(&other_system));
        assert!(!fingerprint(&one).contains("30313233"));
    }
}
