//! Redaction (D10, `event-schema.md` §4): known secret values first, then built-in patterns.
//! One `Redactor` is shared by the `SecretResolver` (registers values), the kernel (ingress) and
//! the log writer (defense in depth).

use std::sync::RwLock;

use regex::Regex;
use serde_json::Value;

use crate::host::SecretString;

/// Replacement text is `[REDACTED:<kind>]`.
fn replacement(kind: &str) -> String {
    format!("[REDACTED:{kind}]")
}

/// Values shorter than this are not registered (too many false positives).
pub const MIN_SECRET_LEN: usize = 8;

/// The name reported by `take_short_names` for a too-short value registered without a name.
pub const UNNAMED_SECRET: &str = "<unnamed>";

struct Pattern {
    kind: &'static str,
    re: Regex,
    /// Which capture group is the secret span (0 = whole match).
    group: usize,
}

/// Scrubs payloads (D10). Constructed once by the launcher and shared by the `SecretResolver`
/// (registers values) and the kernel (redacts on ingress) and the log writer (redacts on write).
pub struct Redactor {
    known: RwLock<Vec<SecretString>>,
    patterns: Vec<Pattern>,
    /// Names whose values were too short to register (each warned once).
    short: RwLock<Vec<String>>,
}

/// How many replacements a pass made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedactionReport {
    /// Number of spans replaced.
    pub replacements: u32,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("known", &self.known.read().map(|k| k.len()).unwrap_or(0))
            .field("patterns", &self.patterns.len())
            .finish()
    }
}

/// The built-in patterns of `event-schema.md` §4.4, in table order.
fn builtin_patterns() -> Vec<Pattern> {
    let p = |kind: &'static str, re: &str, group: usize| Pattern {
        kind,
        re: Regex::new(re).expect("built-in pattern compiles"),
        group,
    };
    vec![
        p(
            "pem",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            0,
        ),
        p(
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b",
            0,
        ),
        p("bearer", r"(?i)\bbearer\s+([A-Za-z0-9\-._~+/]{16,}=*)", 1),
        p(
            "api_key",
            r"\bsk-(?:[A-Za-z0-9_-]{2,}-)?[A-Za-z0-9_-]{16,}\b",
            0,
        ),
        p("api_key", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b", 0),
        p("api_key", r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", 0),
        p("api_key", r"\bAIza[0-9A-Za-z_-]{35}\b", 0),
        p("aws_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0),
        p(
            "aws_key",
            r#"(?i)\baws_secret_access_key\b\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})"#,
            1,
        ),
        p(
            "basic_auth",
            r"(?i)\bbasic\s+([A-Za-z0-9+/]{16,}={0,2})\b",
            1,
        ),
        p("url_credential", r"://([^/\s:@]+):([^/\s@]+)@", 2),
        p(
            "env_secret",
            r#"(?i)\b([A-Z0-9_]*(?:API_KEY|SECRET|TOKEN|PASSWORD|PASSWD|CREDENTIALS?)[A-Z0-9_]*)\s*=\s*["']?([^\s"']{8,})"#,
            2,
        ),
    ]
}

impl Redactor {
    /// Built-in patterns from `event-schema.md` §4.
    pub fn new() -> Redactor {
        Redactor {
            known: RwLock::new(Vec::new()),
            patterns: builtin_patterns(),
            short: RwLock::new(Vec::new()),
        }
    }

    /// Built-in patterns plus site-specific extras (whole match replaced as `[REDACTED:extra]`).
    pub fn with_extra_patterns(patterns: Vec<Regex>) -> Result<Redactor, regex::Error> {
        let mut r = Redactor::new();
        r.patterns.extend(patterns.into_iter().map(|re| Pattern {
            kind: "extra",
            re,
            group: 0,
        }));
        Ok(r)
    }

    /// Register a known secret value. Values shorter than 8 bytes are ignored (and reported by
    /// `take_short_names` so the kernel can log `warning{class: "secret_too_short"}`). The
    /// too-short value itself is never recorded (it would end up in a warning payload); an
    /// unnamed registration is reported as `UNNAMED_SECRET`, once.
    pub fn register_secret(&self, value: &SecretString) {
        let v = value.expose();
        if v.len() < MIN_SECRET_LEN {
            if let Ok(mut s) = self.short.write()
                && !s.iter().any(|n| n == UNNAMED_SECRET)
            {
                s.push(UNNAMED_SECRET.to_owned());
            }
            return;
        }
        if let Ok(mut known) = self.known.write() {
            if known.iter().any(|k| k.expose() == v) {
                return;
            }
            known.push(value.clone());
            // Longest first so an overlapping shorter value never splits a longer one.
            known.sort_by_key(|k| std::cmp::Reverse(k.expose().len()));
        }
    }

    /// Register a known secret with a name, for the `secret_too_short` warning.
    pub fn register_named_secret(&self, name: &str, value: &SecretString) -> bool {
        if value.expose().len() < MIN_SECRET_LEN {
            if let Ok(mut s) = self.short.write()
                && !s.iter().any(|n| n == name)
            {
                s.push(name.to_owned());
            }
            return false;
        }
        self.register_secret(value);
        true
    }

    /// Names of secrets that were too short to register since the last call; each once. Values
    /// are never reported.
    pub fn take_short_names(&self) -> Vec<String> {
        self.short
            .write()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default()
    }

    /// Number of registered values.
    pub fn known_count(&self) -> usize {
        self.known.read().map(|k| k.len()).unwrap_or(0)
    }

    /// Walks every string in `v` (values, not keys), replaces known values first, then patterns.
    pub fn redact_value(&self, v: &mut Value) -> RedactionReport {
        let mut report = RedactionReport::default();
        self.walk(v, &mut report);
        report
    }

    fn walk(&self, v: &mut Value, report: &mut RedactionReport) {
        match v {
            Value::String(s) => {
                let (out, r) = self.redact_str(s);
                if r.replacements > 0 {
                    *s = out;
                    report.replacements += r.replacements;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.walk(item, report);
                }
            }
            Value::Object(map) => {
                for (_, item) in map.iter_mut() {
                    self.walk(item, report);
                }
            }
            _ => {}
        }
    }

    /// Redact one string.
    pub fn redact_str(&self, s: &str) -> (String, RedactionReport) {
        let mut report = RedactionReport::default();
        let mut out = s.to_owned();
        if let Ok(known) = self.known.read() {
            for k in known.iter() {
                let secret = k.expose();
                if out.contains(secret) {
                    let n = out.matches(secret).count() as u32;
                    out = out.replace(secret, &replacement("secret"));
                    report.replacements += n;
                }
            }
        }
        for p in &self.patterns {
            let mut next = String::with_capacity(out.len());
            let mut last = 0;
            let mut hit = false;
            for caps in p.re.captures_iter(&out) {
                let Some(m) = caps.get(p.group) else { continue };
                // Skip spans that are already a redaction marker.
                if m.as_str().starts_with("[REDACTED:") {
                    continue;
                }
                hit = true;
                next.push_str(&out[last..m.start()]);
                next.push_str(&replacement(p.kind));
                last = m.end();
                report.replacements += 1;
            }
            if hit {
                next.push_str(&out[last..]);
                out = next;
            }
        }
        (out, report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_values_first_then_patterns() {
        let r = Redactor::new();
        r.register_secret(&SecretString::new("sk-verysecretvalue0123456789"));
        let (out, rep) = r.redact_str("key is sk-verysecretvalue0123456789 ok");
        assert_eq!(out, "key is [REDACTED:secret] ok");
        assert_eq!(rep.replacements, 1);
        // Unregistered but pattern-shaped.
        let (out, _) = r.redact_str("Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
        assert_eq!(out, "Authorization: Bearer [REDACTED:bearer]");
    }

    #[test]
    fn short_values_are_not_registered() {
        let r = Redactor::new();
        r.register_secret(&SecretString::new("short"));
        r.register_secret(&SecretString::new("tiny"));
        assert_eq!(r.known_count(), 0);
        // The value itself is never recorded (it would leak into the warning payload).
        assert_eq!(r.take_short_names(), vec![UNNAMED_SECRET.to_owned()]);
        assert!(r.take_short_names().is_empty());
        let (out, _) = r.redact_str("short text");
        assert_eq!(out, "short text");
    }

    #[test]
    fn redacts_values_not_keys_recursively() {
        let r = Redactor::new();
        let mut v = json!({
            "sk-abcdefghijklmnopqrstuvwxyz": "key-shaped key stays",
            "nested": [{"token": "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123"}],
            "n": 1,
            "b": true
        });
        let rep = r.redact_value(&mut v);
        assert_eq!(rep.replacements, 1);
        assert_eq!(v["nested"][0]["token"], "[REDACTED:api_key]");
        assert!(
            v.as_object()
                .unwrap()
                .contains_key("sk-abcdefghijklmnopqrstuvwxyz")
        );
    }

    #[test]
    fn redaction_is_a_fixed_point() {
        let r = Redactor::new();
        r.register_secret(&SecretString::new("hunter2hunter2"));
        let input = "Bearer aaaaaaaaaaaaaaaaaaaa and hunter2hunter2 and https://u:p4ssw0rd@h/";
        let (once, _) = r.redact_str(input);
        let (twice, rep) = r.redact_str(&once);
        assert_eq!(once, twice);
        assert_eq!(rep.replacements, 0);
    }
}
