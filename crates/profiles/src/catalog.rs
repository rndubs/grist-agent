//! The catalog (`profile-schema.md` §10): task-agent registry, name → (model profile, agent
//! profile).

use std::path::{Path, PathBuf};

use kernel::Hash;
use toml::Value;

use crate::atom::is_ident;
use crate::diagnostic::{Diagnostic, idx};
use crate::schema::{FileKind, validate_file};

/// One `[[agents]]` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The task-agent name (also the `spawn:` atom operand).
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Absolute path of the model profile.
    pub model_profile: PathBuf,
    /// Absolute path of the agent profile.
    pub agent_profile: PathBuf,
}

/// A loaded, validated `catalog.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    /// Where it was read from.
    pub path: PathBuf,
    /// `Hash::of_bytes` of the raw file.
    pub hash: Hash,
    entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// Load `path` and validate it (§10.1): unique names, both files exist, `[agent].name` of
    /// each agent file equals the entry name, an entry named `default` exists.
    pub fn load(path: &Path) -> Result<Catalog, Vec<Diagnostic>> {
        let bytes = std::fs::read(path).map_err(|e| {
            vec![Diagnostic::new(
                "E_FILE_NOT_FOUND",
                Some(path),
                "",
                format!("cannot read catalog: {e}"),
            )]
        })?;
        let vf = validate_file(FileKind::Catalog, path, &bytes)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("/"));
        let mut entries = Vec::new();
        let mut diags = Vec::new();
        for (i, a) in vf
            .table
            .get("agents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let Some(a) = a.as_table() else { continue };
            let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
            let model_profile = dir.join(
                a.get("model_profile")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            let agent_profile = dir.join(
                a.get("agent_profile")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            // `[agent].name` must equal the entry name: a light parse, the full validation
            // happens when the entry is resolved.
            if let Ok(text) = std::fs::read_to_string(&agent_profile)
                && let Ok(t) = text.parse::<toml::Table>()
                && let Some(n) = t
                    .get("agent")
                    .and_then(|a| a.get("name"))
                    .and_then(Value::as_str)
                && n != name
            {
                diags.push(Diagnostic::new(
                    "E_CATALOG_REF",
                    Some(&agent_profile),
                    "agent.name",
                    format!("'{n}' != catalog entry '{name}'"),
                ));
            }
            let _ = idx("agents", i);
            entries.push(CatalogEntry {
                name: name.to_owned(),
                description: a
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                model_profile,
                agent_profile,
            });
        }
        if !diags.is_empty() {
            return Err(diags);
        }
        Ok(Catalog {
            path: vf.path,
            hash: vf.hash,
            entries,
        })
    }

    /// All entries in file order.
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// Look up an entry by name.
    pub fn get(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// True iff `name` is a well-formed catalog name (`[a-z][a-z0-9_-]*`).
    pub fn is_valid_name(name: &str) -> bool {
        is_ident(name)
    }
}

/// Walk up from `start` until a directory containing `profiles/catalog.toml` is found (§1.3).
pub fn discover_profiles_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join("profiles");
        if candidate.join("catalog.toml").is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}
