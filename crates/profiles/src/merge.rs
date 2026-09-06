//! Layer merge (`profile-schema.md` §6): scalars override, tables deep-merge, lists replace,
//! type mismatch → later wins, `[[middleware]]` keyed by name. Each leaf remembers the file that
//! last set it so post-merge diagnostics can name the right file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kernel::MiddlewareSource;
use toml::{Table, Value};

use crate::diagnostic::{Diagnostic, idx, join};
use crate::schema::{FileKind, MwEntry, ValidatedFile};

/// The merged layers with provenance.
#[derive(Clone, Debug, Default)]
pub struct Layered {
    /// The merged table (without `middleware`).
    pub table: Table,
    /// TOML path of every scalar or list → the file that set it (`None` = kernel defaults).
    pub origins: BTreeMap<String, Option<PathBuf>>,
    /// The keyed middleware union, in first-declaration order.
    pub middleware: Vec<MwEntry>,
}

impl Layered {
    /// Start from the kernel defaults (layer 0).
    pub fn from_defaults(defaults: Table) -> Self {
        let mut l = Layered::default();
        l.apply_table(&defaults, None);
        l
    }

    /// Merge one validated file (layers 1–3).
    pub fn apply(&mut self, file: &ValidatedFile) -> Vec<Diagnostic> {
        self.apply_table(&file.table, Some(&file.path));
        let mut diags = Vec::new();
        for entry in &file.middleware {
            self.merge_middleware(entry.clone(), file.kind, &mut diags);
        }
        diags
    }

    /// Merge a raw table (used for layer 4 as well).
    pub fn apply_table(&mut self, src: &Table, file: Option<&Path>) {
        let mut dst = std::mem::take(&mut self.table);
        merge_tables(&mut dst, src, "", &mut self.origins, file);
        self.table = dst;
    }

    fn merge_middleware(&mut self, entry: MwEntry, kind: FileKind, diags: &mut Vec<Diagnostic>) {
        if let Some(existing) = self.middleware.iter_mut().find(|m| m.name == entry.name) {
            if kind == FileKind::Project
                && matches!(
                    existing.source,
                    MiddlewareSource::Model | MiddlewareSource::Kernel
                )
            {
                diags.push(Diagnostic::new(
                    "E_PRIORITY_RANGE",
                    entry.file.as_deref(),
                    format!("{}.name", idx("middleware", entry.index)),
                    format!(
                        "'{}' is a {} entry and cannot be overridden from a project file",
                        entry.name,
                        match existing.source {
                            MiddlewareSource::Model => "model-profile slot (100..=199)",
                            _ => "kernel slot",
                        }
                    ),
                ));
                return;
            }
            existing.priority = entry.priority;
            existing.config = entry.config;
            existing.source = entry.source;
            existing.file = entry.file;
            existing.index = entry.index;
        } else {
            self.middleware.push(entry);
        }
    }

    /// The file that set the value at `path` (or its nearest ancestor). `None` for layer 0 or
    /// for paths nobody set.
    pub fn origin_of(&self, path: &str) -> Option<&Path> {
        let mut p = path.to_owned();
        loop {
            if let Some(o) = self.origins.get(&p) {
                return o.as_deref();
            }
            let cut = p.rfind(['.', '[']);
            match cut {
                Some(i) if i > 0 => p.truncate(i),
                _ => return None,
            }
        }
    }
}

fn merge_tables(
    dst: &mut Table,
    src: &Table,
    path: &str,
    origins: &mut BTreeMap<String, Option<PathBuf>>,
    file: Option<&Path>,
) {
    for (k, v) in src {
        if path.is_empty() && k == "middleware" {
            continue;
        }
        let kp = join(path, k);
        match (dst.get_mut(k), v) {
            (Some(Value::Table(d)), Value::Table(s)) => merge_tables(d, s, &kp, origins, file),
            _ => {
                // Scalar / list / type mismatch: later wins wholesale.
                origins.retain(|p, _| {
                    !(p.starts_with(&format!("{kp}.")) || p.starts_with(&format!("{kp}[")))
                });
                record_origins(v, &kp, origins, file);
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

fn record_origins(
    v: &Value,
    path: &str,
    origins: &mut BTreeMap<String, Option<PathBuf>>,
    file: Option<&Path>,
) {
    match v {
        Value::Table(t) => {
            origins.insert(path.to_owned(), file.map(Path::to_path_buf));
            for (k, v) in t {
                record_origins(v, &join(path, k), origins, file);
            }
        }
        _ => {
            origins.insert(path.to_owned(), file.map(Path::to_path_buf));
        }
    }
}

/// Convert a TOML value to JSON (for `config` tables and the resolved struct).
pub fn toml_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Integer(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_to_json).collect()),
        Value::Table(t) => serde_json::Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

/// Convert a JSON value to TOML (for runtime overrides); `null` becomes an empty table marker
/// and is rejected by the caller.
pub fn json_to_toml(v: &serde_json::Value) -> Option<Value> {
    Some(match v {
        serde_json::Value::Null => return None,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else {
                Value::Float(n.as_f64()?)
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(a) => {
            Value::Array(a.iter().map(json_to_toml).collect::<Option<Vec<_>>>()?)
        }
        serde_json::Value::Object(o) => Value::Table(
            o.iter()
                .map(|(k, v)| json_to_toml(v).map(|v| (k.clone(), v)))
                .collect::<Option<Table>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Table {
        s.parse().unwrap()
    }

    #[test]
    fn value_rules_of_section_6_1() {
        let mut l = Layered::from_defaults(t(r#"
            [model]
            temperature = 0.5
            max_output_tokens = 4096
            [sandbox]
            env_allow = ["PATH", "HOME"]
            timeout_s = 600
        "#));
        l.apply_table(
            &t(r#"
            [model]
            temperature = 0.3
            [sandbox]
            env_allow = ["PATH"]
            [prompt]
            file = "x"
        "#),
            Some(Path::new("/p/a.toml")),
        );
        l.apply_table(&t(r#"prompt = "inline""#), Some(Path::new("/p/b.toml")));
        assert_eq!(l.table["model"]["temperature"].as_float(), Some(0.3));
        assert_eq!(
            l.table["model"]["max_output_tokens"].as_integer(),
            Some(4096)
        );
        assert_eq!(l.table["sandbox"]["env_allow"].as_array().unwrap().len(), 1);
        assert_eq!(l.table["sandbox"]["timeout_s"].as_integer(), Some(600));
        assert_eq!(l.table["prompt"].as_str(), Some("inline"));
        assert_eq!(
            l.origin_of("model.temperature"),
            Some(Path::new("/p/a.toml"))
        );
        assert_eq!(l.origin_of("model.max_output_tokens"), None);
        assert_eq!(
            l.origin_of("sandbox.env_allow[0]"),
            Some(Path::new("/p/a.toml"))
        );
        assert_eq!(l.origin_of("prompt"), Some(Path::new("/p/b.toml")));
        assert_eq!(l.origin_of("prompt.file"), Some(Path::new("/p/b.toml")));
    }
}
