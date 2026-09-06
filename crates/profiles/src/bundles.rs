//! The bundles file (`profile-schema.md` §5): named atom sets and the in-house install paths.

use std::collections::BTreeMap;
use std::path::Path;

use kernel::Hash;
use toml::Value;

use crate::atom::{GrantItem, classify};
use crate::diagnostic::Diagnostic;
use crate::schema::{FileKind, validate_file};

/// One `[bundles.<name>]` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    /// Optional description.
    pub description: Option<String>,
    /// Atom strings (symbolic; no bundle names).
    pub atoms: Vec<String>,
}

/// `(original list index, symbolic atom string)` from `Bundles::expand_indexed`.
pub type IndexedAtom = (usize, String);
/// `(original list index, unknown bundle name)` from `Bundles::expand_indexed`.
pub type UnknownBundle<'a> = (usize, &'a str);

/// A loaded, validated `bundles.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundles {
    /// Where it was read from.
    pub path: std::path::PathBuf,
    /// `Hash::of_bytes` of the raw file.
    pub hash: Hash,
    /// `[install]`: tool name → absolute install path.
    pub install: BTreeMap<String, String>,
    /// `[bundles.<name>]`.
    pub bundles: BTreeMap<String, Bundle>,
}

impl Bundles {
    /// Load and validate `path`.
    pub fn load(path: &Path) -> Result<Bundles, Vec<Diagnostic>> {
        let bytes = std::fs::read(path).map_err(|e| {
            vec![Diagnostic::new(
                "E_FILE_NOT_FOUND",
                Some(path),
                "",
                format!("cannot read bundles file: {e}"),
            )]
        })?;
        let vf = validate_file(FileKind::Bundles, path, &bytes)?;
        let install = vf
            .table
            .get("install")
            .and_then(Value::as_table)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let bundles = vf
            .table
            .get("bundles")
            .and_then(Value::as_table)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let b = v.as_table()?;
                        Some((
                            k.clone(),
                            Bundle {
                                description: b
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                                atoms: b
                                    .get("atoms")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(Value::as_str)
                                            .map(str::to_owned)
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            },
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Bundles {
            path: vf.path,
            hash: vf.hash,
            install,
            bundles,
        })
    }

    /// `expand_bundles` (§5.3) with provenance: every string is either an atom (passed through)
    /// or a bundle name (replaced by its atoms). Returns `(original_index, atom)` pairs in
    /// original order (callers sort/dedupe for the resolved struct); unknown bundle names are
    /// returned as `Err(vec![(index, name), …])`.
    pub fn expand_indexed<'a>(
        &self,
        list: &[&'a str],
    ) -> Result<Vec<IndexedAtom>, Vec<UnknownBundle<'a>>> {
        let mut out = Vec::new();
        let mut errs = Vec::new();
        for (i, s) in list.iter().enumerate() {
            match classify(s) {
                Ok(GrantItem::Atom { .. }) => out.push((i, (*s).to_owned())),
                Ok(GrantItem::Bundle(name)) => match self.bundles.get(name) {
                    Some(b) => out.extend(b.atoms.iter().map(|a| (i, a.clone()))),
                    None => errs.push((i, *s)),
                },
                Err(_) => errs.push((i, *s)),
            }
        }
        if errs.is_empty() { Ok(out) } else { Err(errs) }
    }

    /// `expand_bundles(list)` (§5.3): bundle names replaced by their atoms, placeholders kept
    /// symbolic, deduplicated and sorted lexically by string form.
    pub fn expand(&self, list: &[&str]) -> Result<Vec<String>, Vec<Diagnostic>> {
        match self.expand_indexed(list) {
            Ok(items) => {
                let mut atoms: Vec<String> = items.into_iter().map(|(_, a)| a).collect();
                atoms.sort();
                atoms.dedup();
                Ok(atoms)
            }
            Err(errs) => Err(errs
                .into_iter()
                .map(|(i, name)| {
                    Diagnostic::new(
                        "E_UNKNOWN_BUNDLE",
                        Some(&self.path),
                        format!("[{i}]"),
                        format!("'{name}'"),
                    )
                })
                .collect()),
        }
    }
}
