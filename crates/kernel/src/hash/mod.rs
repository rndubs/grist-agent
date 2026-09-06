//! Hashing (D3): BLAKE3 over RFC 8785 canonical JSON, rendered `b3:<64 lowercase hex>`.
//!
//! The canonicalizer is implemented in-crate over `serde_json::Value`
//! (`event-schema.md` §3.8 allows this when the RFC 8785 test vectors pass; see the
//! tests at the bottom). ECMAScript number formatting comes from `ryu-js`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod strict;
pub use strict::to_strict_value;

/// BLAKE3 over RFC 8785 canonical JSON, rendered as `b3:<64 lowercase hex>` (D3).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hash(String);

/// Errors from hashing and canonicalization.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    /// Non-finite float or non-string map key.
    #[error("value is not canonicalizable: {0}")]
    NotCanonicalizable(String),
    /// `serde` refused to serialize the value.
    #[error("serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The string is not `b3:` + 64 lowercase hex digits.
    #[error("malformed hash string: {0}")]
    Malformed(String),
}

impl Hash {
    /// Hash the canonical JSON (RFC 8785) of `value`.
    /// Errors if `value` contains a non-finite float or a map with non-string keys.
    pub fn of_canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Hash, HashError> {
        Ok(Hash::of_bytes(&canonical_json(value)?))
    }

    /// Hash raw bytes (used for `raw_response_hash` and artifact content addressing).
    pub fn of_bytes(bytes: &[u8]) -> Hash {
        Hash(format!("b3:{}", blake3::hash(bytes).to_hex()))
    }

    /// Parse a `b3:<hex>` string; rejects any other algorithm prefix or length.
    pub fn parse(s: &str) -> Result<Hash, HashError> {
        let hex = s
            .strip_prefix("b3:")
            .ok_or_else(|| HashError::Malformed(s.to_owned()))?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(HashError::Malformed(s.to_owned()));
        }
        Ok(Hash(s.to_owned()))
    }

    /// The `b3:<hex>` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Canonical JSON bytes (RFC 8785). Exposed so tests and `diff-logs` share one implementation.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, HashError> {
    let v = to_strict_value(value)?;
    let mut out = Vec::new();
    write_canonical(&v, &mut out)?;
    Ok(out)
}

/// Canonical JSON of an already-parsed `Value`.
pub fn canonical_json_value(v: &Value) -> Result<Vec<u8>, HashError> {
    let mut out = Vec::new();
    write_canonical(v, &mut out)?;
    Ok(out)
}

fn write_canonical(v: &Value, out: &mut Vec<u8>) -> Result<(), HashError> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => write_number(n, out)?,
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => write_object(map, out)?,
    }
    Ok(())
}

/// Members sorted by the UTF-16 code units of their keys (RFC 8785 §3.2.3), not by bytes.
fn write_object(map: &Map<String, Value>, out: &mut Vec<u8>) -> Result<(), HashError> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
    out.push(b'{');
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_string(k, out);
        out.push(b':');
        write_canonical(v, out)?;
    }
    out.push(b'}');
    Ok(())
}

/// JCS string escaping: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, other controls as
/// lowercase `\u00xx`; everything else literal UTF-8.
fn write_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// ECMAScript `Number::toString`. Integers within ±2^53 print exactly; larger ones go
/// through `f64` first, exactly as a JavaScript engine would parse them.
fn write_number(n: &serde_json::Number, out: &mut Vec<u8>) -> Result<(), HashError> {
    const SAFE: u64 = 1 << 53;
    if let Some(u) = n.as_u64() {
        if u <= SAFE {
            out.extend_from_slice(u.to_string().as_bytes());
            return Ok(());
        }
        return write_f64(u as f64, out);
    }
    if let Some(i) = n.as_i64() {
        if i.unsigned_abs() <= SAFE {
            out.extend_from_slice(i.to_string().as_bytes());
            return Ok(());
        }
        return write_f64(i as f64, out);
    }
    match n.as_f64() {
        Some(f) => write_f64(f, out),
        None => Err(HashError::NotCanonicalizable(format!("number {n}"))),
    }
}

fn write_f64(f: f64, out: &mut Vec<u8>) -> Result<(), HashError> {
    if !f.is_finite() {
        return Err(HashError::NotCanonicalizable(format!(
            "non-finite float {f}"
        )));
    }
    if f == 0.0 {
        // ES prints both zeros as "0".
        out.push(b'0');
        return Ok(());
    }
    let mut buf = ryu_js::Buffer::new();
    out.extend_from_slice(buf.format_finite(f).as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canon(v: &Value) -> String {
        String::from_utf8(canonical_json_value(v).unwrap()).unwrap()
    }

    /// RFC 8785 §3.2.3 example: keys sorted by UTF-16 code units, so the emoji
    /// (surrogate pair D83D DE00) sorts before U+FB33.
    #[test]
    fn rfc8785_key_ordering_vector() {
        let input = r#"{
            "\u20ac": "Euro Sign",
            "\r": "Carriage Return",
            "\ufb33": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "\ud83d\ude00": "Emoji: Grinning Face",
            "\u0080": "Control",
            "\u00f6": "Latin Small Letter O With Diaeresis"
        }"#;
        let v: Value = serde_json::from_str(input).unwrap();
        let expected = "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\"\u{f6}\":\"Latin Small Letter O With Diaeresis\",\"\u{20ac}\":\"Euro Sign\",\"\u{1f600}\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}";
        assert_eq!(canon(&v), expected);
    }

    /// RFC 8785 §3.2.4 (numbers and strings) example.
    #[test]
    fn rfc8785_numbers_and_strings_vector() {
        let input = r#"{
            "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
            "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
            "literals": [null, true, false]
        }"#;
        let v: Value = serde_json::from_str(input).unwrap();
        let expected = "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"\u{20ac}$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}";
        assert_eq!(canon(&v), expected);
    }

    #[test]
    fn es_number_formatting_vectors() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1"),
            (0.1, "0.1"),
            (-0.0, "0"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (9007199254740992.0, "9007199254740992"),
            (5e-324, "5e-324"),
        ];
        for (f, want) in cases {
            assert_eq!(canon(&json!(f)), *want, "{f}");
        }
    }

    /// `u64::MAX` is emitted as `18446744073709552000`, as the RFC specifies (it does
    /// not round-trip); values up to 2^53 print exactly.
    #[test]
    fn u64_max_is_lossy_as_the_rfc_specifies() {
        assert_eq!(canon(&json!(u64::MAX)), "18446744073709552000");
        assert_eq!(canon(&json!(9007199254740993u64)), "9007199254740992");
        assert_eq!(canon(&json!(9007199254740992u64)), "9007199254740992");
        assert_eq!(canon(&json!(i64::MIN)), "-9223372036854776000");
        assert_eq!(canon(&json!(-42)), "-42");
    }

    #[test]
    fn non_finite_floats_are_rejected_on_both_paths() {
        #[derive(Serialize)]
        struct S {
            x: f64,
            nested: Vec<f64>,
        }
        // Path 1: a struct with a non-finite float fails at strict serialization (serde_json's
        // own `to_value` would have turned it into `null`).
        let err = Hash::of_canonical_json(&S {
            x: f64::NAN,
            nested: vec![],
        });
        assert!(
            matches!(err, Err(HashError::NotCanonicalizable(_))),
            "{err:?}"
        );
        let err = Hash::of_canonical_json(&S {
            x: 1.0,
            nested: vec![1.0, f64::INFINITY],
        });
        assert!(
            matches!(err, Err(HashError::NotCanonicalizable(_))),
            "{err:?}"
        );
        // Path 2: the writer itself refuses a non-finite number.
        assert!(matches!(
            write_f64(f64::NEG_INFINITY, &mut Vec::new()),
            Err(HashError::NotCanonicalizable(_))
        ));
        // Finite values pass.
        assert!(
            Hash::of_canonical_json(&S {
                x: 1.5,
                nested: vec![0.0]
            })
            .is_ok()
        );
    }

    #[test]
    fn non_string_map_keys_are_rejected() {
        let m: std::collections::BTreeMap<u32, &str> = [(1, "a")].into_iter().collect();
        assert!(matches!(
            Hash::of_canonical_json(&m),
            Err(HashError::NotCanonicalizable(_))
        ));
        let ok: std::collections::BTreeMap<String, u32> =
            [("a".to_owned(), 1)].into_iter().collect();
        assert!(Hash::of_canonical_json(&ok).is_ok());
    }

    #[test]
    fn hash_string_shape_and_parse() {
        let h = Hash::of_bytes(b"hello");
        assert!(h.as_str().starts_with("b3:"));
        assert_eq!(h.as_str().len(), 67);
        assert_eq!(Hash::parse(h.as_str()).unwrap(), h);
        assert!(Hash::parse("sha256:abcd").is_err());
        assert!(Hash::parse("b3:abcd").is_err());
        let upper = format!("b3:{}", "A".repeat(64));
        assert!(Hash::parse(&upper).is_err());
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{}\"", h.as_str()));
        let back: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn known_blake3_vector() {
        // BLAKE3 of the empty input.
        assert_eq!(
            Hash::of_bytes(b"").as_str(),
            "b3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn canonical_hash_is_order_independent() {
        let a = json!({"b": 1, "a": [1, 2.0, {"z": null, "y": "s"}]});
        let b = json!({"a": [1, 2, {"y": "s", "z": null}], "b": 1.0});
        assert_eq!(
            Hash::of_canonical_json(&a).unwrap(),
            Hash::of_canonical_json(&b).unwrap()
        );
        assert_eq!(canon(&a), "{\"a\":[1,2,{\"y\":\"s\",\"z\":null}],\"b\":1}");
    }
}
