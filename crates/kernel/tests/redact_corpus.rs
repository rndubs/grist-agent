//! P1.3: the `Redactor` against `event-schema.md` §4 — one positive and one near-miss per row of
//! the §4.4 pattern table, known-value rules (§4.3), replacement span (§4.2), determinism, the
//! fixed point (§4.5), values-not-keys recursion, and `with_extra_patterns`.

use kernel::redact::{MIN_SECRET_LEN, UNNAMED_SECRET};
use kernel::{Redactor, SecretString};
use regex::Regex;
use serde_json::json;

fn redact(s: &str) -> String {
    Redactor::new().redact_str(s).0
}

/// One row of the §4.4 table: a positive input, its expected output, and a near-miss that MUST
/// come back unchanged.
struct Row {
    kind: &'static str,
    positive: &'static str,
    expected: &'static str,
    near_miss: &'static str,
}

const GHP: &str = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"; // ghp_ + 36
const GHP_SHORT: &str = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi"; // ghp_ + 35
const AIZA: &str = "AIzaSyA1234567890abcdefghijklmnopqrstuv"; // AIza + 35
const AIZA_SHORT: &str = "AIzaSyA1234567890abcdefghijklmnopqrstu"; // AIza + 34

fn rows() -> Vec<Row> {
    vec![
        Row {
            kind: "pem",
            positive: "key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nAB==\n-----END RSA PRIVATE KEY-----\ndone",
            expected: "key:\n[REDACTED:pem]\ndone",
            near_miss: "-----BEGIN CERTIFICATE-----\nMIIEow\n-----END CERTIFICATE-----",
        },
        Row {
            kind: "jwt",
            positive: "t=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c;",
            expected: "t=[REDACTED:jwt];",
            near_miss: "t=eyJhbGc.eyJzdWI.SflKxw;",
        },
        Row {
            kind: "bearer",
            positive: "Authorization: Bearer abcdefghijklmnop0123 end",
            expected: "Authorization: Bearer [REDACTED:bearer] end",
            near_miss: "Authorization: Bearer",
        },
        Row {
            kind: "api_key (sk-)",
            positive: "use sk-ant-api03-abcdefghijklmnopqrstuvwxyz now",
            expected: "use [REDACTED:api_key] now",
            near_miss: "use sk-abcdefgh now",
        },
        Row {
            kind: "api_key (github)",
            positive: "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij.",
            expected: "token [REDACTED:api_key].",
            near_miss: "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi.",
        },
        Row {
            kind: "api_key (slack)",
            positive: "xoxb-1234567890-abcdefgh",
            expected: "[REDACTED:api_key]",
            near_miss: "xoxb-123456789",
        },
        Row {
            kind: "api_key (google)",
            positive: "AIzaSyA1234567890abcdefghijklmnopqrstuv",
            expected: "[REDACTED:api_key]",
            near_miss: "AIzaSyA1234567890abcdefghijklmnopqrstu",
        },
        Row {
            kind: "aws_key (id)",
            positive: "id=AKIAIOSFODNN7EXAMPLE ok",
            expected: "id=[REDACTED:aws_key] ok",
            near_miss: "id=AKIAIOSFODNN7EXAMPL ok",
        },
        Row {
            kind: "aws_key (secret)",
            positive: "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            expected: "aws_secret_access_key = [REDACTED:aws_key]",
            // 39 chars, and `:` so the env_secret row does not fire either.
            near_miss: "aws_secret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKE",
        },
        Row {
            kind: "basic_auth",
            positive: "Authorization: Basic dXNlcm5hbWU6cGFzc3dvcmQ ok",
            expected: "Authorization: Basic [REDACTED:basic_auth] ok",
            near_miss: "Authorization: Basic abc123",
        },
        Row {
            kind: "url_credential",
            positive: "postgres://alice:s3cr3tpw@db.example/x",
            expected: "postgres://alice:[REDACTED:url_credential]@db.example/x",
            near_miss: "https://example.com/a:b@c and mailto://alice@host",
        },
        Row {
            kind: "env_secret",
            positive: "OPENAI_API_KEY=abcdefgh12\nDB_PASSWORD=\"hunter2hunter2\"",
            expected: "OPENAI_API_KEY=[REDACTED:env_secret]\nDB_PASSWORD=\"[REDACTED:env_secret]\"",
            near_miss: "TOKEN=short1\nHOME=/home/user/somewhere",
        },
    ]
}

#[test]
fn every_pattern_row_has_a_positive_and_a_near_miss() {
    for row in rows() {
        let (out, rep) = Redactor::new().redact_str(row.positive);
        assert_eq!(out, row.expected, "row {}: positive", row.kind);
        assert!(rep.replacements >= 1, "row {}: reported", row.kind);
        let (miss, rep) = Redactor::new().redact_str(row.near_miss);
        assert_eq!(
            miss, row.near_miss,
            "row {}: near-miss must be unchanged",
            row.kind
        );
        assert_eq!(rep.replacements, 0, "row {}: near-miss reported", row.kind);
    }
    // Sanity on the fixed-length constants used above.
    assert_eq!(GHP.len(), 4 + 36);
    assert_eq!(GHP_SHORT.len(), 4 + 35);
    assert_eq!(AIZA.len(), 4 + 35);
    assert_eq!(AIZA_SHORT.len(), 4 + 34);
}

#[test]
fn near_miss_extra_edge_cases() {
    // JWT with one short segment; sk- with 15 body chars; AKIA + 17 (no word boundary at 16).
    assert_eq!(
        redact("eyJhbGciOiJIUzI1NiJ9.eyJzdWI.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"),
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWI.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
    );
    assert_eq!(redact("sk-abcdefghijklmno"), "sk-abcdefghijklmno");
    assert_eq!(redact("AKIAIOSFODNN7EXAMPLEX"), "AKIAIOSFODNN7EXAMPLEX");
    assert_eq!(redact("Bearer abcdefghijklmno"), "Bearer abcdefghijklmno"); // 15 chars
}

#[test]
fn table_order_is_applied_jwt_before_bearer() {
    // A bearer JWT is reported as `jwt` because that row comes first (§4.4 "order of application").
    let out = redact(
        "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
    );
    assert_eq!(out, "Bearer [REDACTED:jwt]");
}

#[test]
fn replacement_covers_only_the_matched_group() {
    // Group-1/2 rows keep everything around the secret span (§4.2).
    assert_eq!(
        redact("curl -H 'Authorization: bearer AbCdEfGhIjKlMnOpQr' https://x"),
        "curl -H 'Authorization: bearer [REDACTED:bearer]' https://x"
    );
    assert_eq!(
        redact("export MY_SECRET_TOKEN='abcdefghij' && echo"),
        "export MY_SECRET_TOKEN='[REDACTED:env_secret]' && echo"
    );
}

#[test]
fn multiple_matches_in_one_string_are_each_replaced() {
    let (out, rep) =
        Redactor::new().redact_str(&format!("{GHP} and {GHP} and AKIAIOSFODNN7EXAMPLE"));
    assert_eq!(
        out,
        "[REDACTED:api_key] and [REDACTED:api_key] and [REDACTED:aws_key]"
    );
    assert_eq!(rep.replacements, 3);
}

#[test]
fn known_values_are_replaced_before_patterns_and_reported_as_secret() {
    let r = Redactor::new();
    r.register_secret(&SecretString::new(GHP));
    let (out, rep) = r.redact_str(&format!("a {GHP} b"));
    assert_eq!(out, "a [REDACTED:secret] b");
    assert_eq!(rep.replacements, 1);
    // A value that is a substring of a pattern-shaped token: the known-value pass wins for that
    // span and the remainder never re-matches the pattern.
    r.register_secret(&SecretString::new("verysecretvalue0123456789"));
    let (out, _) = r.redact_str("sk-verysecretvalue0123456789");
    assert_eq!(out, "sk-[REDACTED:secret]");
}

#[test]
fn known_values_match_exact_case_sensitive_substrings() {
    let r = Redactor::new();
    r.register_secret(&SecretString::new("CorrectHorseBattery"));
    assert_eq!(
        r.redact_str("xCorrectHorseBatteryx").0,
        "x[REDACTED:secret]x"
    );
    assert_eq!(
        r.redact_str("correcthorsebattery").0,
        "correcthorsebattery",
        "case-sensitive"
    );
}

#[test]
fn overlapping_known_values_longest_first() {
    let r = Redactor::new();
    r.register_secret(&SecretString::new("abcdefgh"));
    r.register_secret(&SecretString::new("abcdefghijkl"));
    // The longer value is replaced whole, not split by the shorter one.
    assert_eq!(r.redact_str("=abcdefghijkl=").0, "=[REDACTED:secret]=");
    assert_eq!(r.known_count(), 2);
    r.register_secret(&SecretString::new("abcdefgh")); // duplicate is a no-op
    assert_eq!(r.known_count(), 2);
}

#[test]
fn short_values_are_not_registered_and_are_reported_by_name_only() {
    let r = Redactor::new();
    let short = "a".repeat(MIN_SECRET_LEN - 1);
    r.register_secret(&SecretString::new(short.clone()));
    assert!(!r.register_named_secret("OPENAI_API_KEY", &SecretString::new("tiny")));
    assert!(!r.register_named_secret("OPENAI_API_KEY", &SecretString::new("tiny"))); // once
    assert_eq!(r.known_count(), 0);
    let names = r.take_short_names();
    assert_eq!(
        names,
        vec![UNNAMED_SECRET.to_owned(), "OPENAI_API_KEY".to_owned()]
    );
    assert!(
        names
            .iter()
            .all(|n| !n.contains(&short) && !n.contains("tiny"))
    );
    assert!(r.take_short_names().is_empty(), "each reported once");
    // Exactly MIN_SECRET_LEN bytes is accepted.
    assert!(r.register_named_secret("K", &SecretString::new("b".repeat(MIN_SECRET_LEN))));
    assert_eq!(r.known_count(), 1);
    assert_eq!(
        r.redact_str(&short).0,
        short,
        "a too-short value is not redacted"
    );
}

#[test]
fn redaction_is_deterministic() {
    let r = Redactor::new();
    r.register_secret(&SecretString::new("hunter2hunter2"));
    let input = format!("{GHP} hunter2hunter2 Bearer abcdefghijklmnop0123 https://u:p4ssw0rd@h/");
    let a = r.redact_str(&input);
    let b = Redactor::new().redact_str(&input);
    let c = r.redact_str(&input);
    assert_eq!(a, c);
    // Only the registered value differs between a fresh redactor and one with a known value.
    assert_eq!(b.0.replace("hunter2hunter2", "[REDACTED:secret]"), a.0);
}

#[test]
fn redaction_is_a_fixed_point_over_the_whole_corpus() {
    let r = Redactor::new();
    r.register_secret(&SecretString::new("hunter2hunter2"));
    let mut corpus: Vec<String> = rows()
        .iter()
        .flat_map(|row| [row.positive.to_owned(), row.near_miss.to_owned()])
        .collect();
    corpus.push("hunter2hunter2 and Bearer hunter2hunter2hunter2 and TOKEN=hunter2hunter2".into());
    corpus.push("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into());
    for s in corpus {
        let (once, _) = r.redact_str(&s);
        let (twice, rep) = r.redact_str(&once);
        assert_eq!(once, twice, "input {s:?}");
        assert_eq!(rep.replacements, 0, "input {s:?}");
    }
}

#[test]
fn redact_value_walks_values_not_keys_recursively() {
    let r = Redactor::new();
    r.register_secret(&SecretString::new("hunter2hunter2"));
    let key = GHP.to_owned();
    let mut v = json!({
        "GHP_KEY_AS_KEY": {"hunter2hunter2": "value-is-clean"},
        "arr": [1, true, null, "hunter2hunter2", ["nested Bearer abcdefghijklmnop0123"]],
        "obj": {"deep": {"x": "AKIAIOSFODNN7EXAMPLE"}},
        "n": 12345678,
    });
    v["obj"]["deep"][key.as_str()] = json!("keyed");
    let rep = r.redact_value(&mut v);
    assert_eq!(rep.replacements, 3);
    assert_eq!(v["arr"][3], "[REDACTED:secret]");
    assert_eq!(v["arr"][4][0], "nested Bearer [REDACTED:bearer]");
    assert_eq!(v["obj"]["deep"]["x"], "[REDACTED:aws_key]");
    assert_eq!(
        v["obj"]["deep"][key.as_str()],
        "keyed",
        "keys are never redacted"
    );
    assert!(
        v["GHP_KEY_AS_KEY"]
            .as_object()
            .unwrap()
            .contains_key("hunter2hunter2")
    );
    assert_eq!(v["n"], 12345678);
}

#[test]
fn with_extra_patterns_adds_site_specific_rows_after_the_builtins() {
    let r = Redactor::with_extra_patterns(vec![Regex::new(r"\bSITE-[0-9]{6}\b").unwrap()]).unwrap();
    let (out, rep) = r.redact_str(&format!("SITE-123456 {GHP} SITE-12"));
    assert_eq!(out, "[REDACTED:extra] [REDACTED:api_key] SITE-12");
    assert_eq!(rep.replacements, 2);
    // The extra pattern is a fixed point too.
    assert_eq!(r.redact_str(&out).1.replacements, 0);
}
