//! The out-of-process extension manifest (`docs/specs/extension-manifest.md` §2, ADR-0001).
//!
//! An extension is a directory holding `extension.toml`. The manifest names the extension, its
//! version, the command to spawn, the isolation kind (D5), the capability atoms the process
//! needs (D6), and the tools it serves with their JSON schemas. Parsing is strict: unknown keys
//! are errors, so a later key (dependencies, `runtime = "wasm"`) is an additive change.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use kernel::capability::{Capability, FsMode};
use kernel::hash::Hash;
use kernel::tool::{ToolKind, is_valid_tool_name};
use serde_json::Value as Json;
use toml::{Table, Value};

use crate::error::ExtError;

/// File name of the manifest inside an extension directory.
pub const MANIFEST_FILE: &str = "extension.toml";

/// The only `schema_version` this build reads.
pub const MANIFEST_SCHEMA_VERSION: i64 = 1;

/// Prefix of every out-of-process tool name: `ext.<extension>.<tool>` (`profile-schema.md` §11.5).
pub const EXT_TOOL_PREFIX: &str = "ext.";

/// Values bound to the manifest placeholders. `${ext}` is the extension directory itself.
#[derive(Clone, Copy, Debug)]
pub struct Placeholders<'a> {
    /// `${workdir}`: the absolute session working directory.
    pub workdir: &'a Path,
    /// `${home}`: the kernel user's home.
    pub home: &'a Path,
}

/// One tool the extension serves.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    /// The extension-local name (what `tools/call` receives).
    pub name: String,
    /// The description the model sees.
    pub description: String,
    /// JSON Schema (draft 2020-12) object for the input.
    pub input_schema: Json,
}

/// A parsed, validated, placeholder-expanded manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// The extension directory (canonical).
    pub dir: PathBuf,
    /// `b3` of the manifest file bytes.
    pub hash: Hash,
    /// `extension.name`; tools are exposed as `ext.<name>.<tool>`.
    pub name: String,
    /// `extension.version`, opaque.
    pub version: String,
    /// `extension.description`.
    pub description: Option<String>,
    /// `extension.kind`: a process per call (`Stateless`) or per session (`Session`), D5.
    pub kind: ToolKind,
    /// `extension.command`, expanded; element 0 is the program.
    pub command: Vec<String>,
    /// `extension.cwd`, expanded (default `${workdir}`).
    pub cwd: PathBuf,
    /// `extension.capabilities`, expanded, plus the implicit `fs.ro:<dir>`; normalized,
    /// deduplicated and sorted.
    pub capabilities: Vec<Capability>,
    /// `[[tools]]` in file order.
    pub tools: Vec<ToolSpec>,
}

impl Manifest {
    /// Read and validate `<dir>/extension.toml`.
    pub fn load(dir: &Path, ph: &Placeholders<'_>) -> Result<Manifest, ExtError> {
        let dir = dir.canonicalize().map_err(|source| ExtError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(MANIFEST_FILE);
        let bytes = std::fs::read(&path).map_err(|source| ExtError::Io {
            path: path.clone(),
            source,
        })?;
        let text = String::from_utf8(bytes.clone()).map_err(|e| ExtError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let table: Table = text.parse().map_err(|e: toml::de::Error| ExtError::Parse {
            path: path.clone(),
            message: e.message().to_owned(),
        })?;
        Parser {
            path: &path,
            dir: &dir,
            ph,
        }
        .manifest(&table, Hash::of_bytes(&bytes))
    }

    /// The kernel-facing name of local tool `tool`: `ext.<name>.<tool>`.
    pub fn tool_name(&self, tool: &str) -> String {
        format!("{EXT_TOOL_PREFIX}{}.{tool}", self.name)
    }
}

/// `[a-z][a-z0-9_-]*`.
fn is_ident(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(b'a'..=b'z'))
        && b.all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

struct Parser<'a> {
    path: &'a Path,
    dir: &'a Path,
    ph: &'a Placeholders<'a>,
}

impl Parser<'_> {
    fn invalid(&self, key: impl Into<String>, message: impl Into<String>) -> ExtError {
        ExtError::Invalid {
            path: self.path.to_path_buf(),
            key: key.into(),
            message: message.into(),
        }
    }

    fn closed(&self, t: &Table, prefix: &str, allowed: &[&str]) -> Result<(), ExtError> {
        match t.keys().find(|k| !allowed.contains(&k.as_str())) {
            Some(k) => Err(self.invalid(format!("{prefix}{k}"), "unknown key")),
            None => Ok(()),
        }
    }

    fn str_at<'t>(
        &self,
        t: &'t Table,
        prefix: &str,
        key: &str,
    ) -> Result<Option<&'t str>, ExtError> {
        match t.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(self.invalid(format!("{prefix}{key}"), "must be a string")),
        }
    }

    fn req_str<'t>(&self, t: &'t Table, prefix: &str, key: &str) -> Result<&'t str, ExtError> {
        match self.str_at(t, prefix, key)? {
            Some(s) if !s.trim().is_empty() => Ok(s),
            Some(_) => Err(self.invalid(format!("{prefix}{key}"), "must not be empty")),
            None => Err(self.invalid(format!("{prefix}{key}"), "is required")),
        }
    }

    fn str_list<'t>(&self, t: &'t Table, key: &str) -> Result<Vec<&'t str>, ExtError> {
        let Some(v) = t.get(key) else {
            return Ok(Vec::new());
        };
        let Value::Array(items) = v else {
            return Err(self.invalid(key, "must be a list of strings"));
        };
        items
            .iter()
            .map(|i| {
                i.as_str()
                    .ok_or_else(|| self.invalid(key, "must be a list of strings"))
            })
            .collect()
    }

    /// Expand a leading `${ext}`, `${workdir}` or `${home}`; any other `${` is an error.
    fn expand(&self, key: &str, s: &str) -> Result<String, ExtError> {
        let bindings: [(&str, &Path); 3] = [
            ("${ext}", self.dir),
            ("${workdir}", self.ph.workdir),
            ("${home}", self.ph.home),
        ];
        let (head, rest) = match bindings.iter().find(|(p, _)| s.starts_with(p)) {
            Some((p, value)) => (value.to_string_lossy().into_owned(), &s[p.len()..]),
            None => (String::new(), s),
        };
        if rest.contains("${") {
            return Err(self.invalid(
                key,
                format!(
                    "`{s}`: only a leading ${{ext}}, ${{workdir}} or ${{home}} placeholder is allowed"
                ),
            ));
        }
        Ok(format!("{head}{rest}"))
    }

    fn manifest(&self, t: &Table, hash: Hash) -> Result<Manifest, ExtError> {
        self.closed(t, "", &["schema_version", "extension", "tools"])?;
        match t.get("schema_version") {
            Some(Value::Integer(MANIFEST_SCHEMA_VERSION)) => {}
            Some(_) => {
                return Err(self.invalid(
                    "schema_version",
                    format!("must be {MANIFEST_SCHEMA_VERSION}"),
                ));
            }
            None => return Err(self.invalid("schema_version", "is required")),
        }
        let ext = match t.get("extension") {
            Some(Value::Table(e)) => e,
            Some(_) => return Err(self.invalid("extension", "must be a table")),
            None => return Err(self.invalid("extension", "is required")),
        };
        self.closed(
            ext,
            "extension.",
            &[
                "name",
                "version",
                "description",
                "kind",
                "runtime",
                "command",
                "cwd",
                "capabilities",
            ],
        )?;

        let name = self.req_str(ext, "extension.", "name")?;
        if !is_ident(name) {
            return Err(self.invalid("extension.name", "must match [a-z][a-z0-9_-]*"));
        }
        let version = self.req_str(ext, "extension.", "version")?.to_owned();
        let description = self
            .str_at(ext, "extension.", "description")?
            .map(str::to_owned);
        let kind = match self.req_str(ext, "extension.", "kind")? {
            "stateless" => ToolKind::Stateless,
            "session" => ToolKind::Session,
            _ => {
                return Err(self.invalid("extension.kind", "must be \"stateless\" or \"session\""));
            }
        };
        match self.str_at(ext, "extension.", "runtime")? {
            None | Some("process") => {}
            Some("wasm") => {
                return Err(self.invalid(
                    "extension.runtime",
                    "\"wasm\" is reserved (ADR-0001) and not implemented",
                ));
            }
            Some(_) => return Err(self.invalid("extension.runtime", "must be \"process\"")),
        }

        let command: Vec<String> = self
            .str_list(ext, "command")
            .map_err(|_| self.invalid("extension.command", "must be a list of strings"))?
            .into_iter()
            .enumerate()
            .map(|(i, s)| self.expand(&format!("extension.command[{i}]"), s))
            .collect::<Result<_, _>>()?;
        if command.first().is_none_or(|p| p.is_empty()) {
            return Err(self.invalid("extension.command", "must name a program"));
        }
        let cwd_raw = self
            .str_at(ext, "extension.", "cwd")?
            .unwrap_or("${workdir}");
        let cwd = PathBuf::from(self.expand("extension.cwd", cwd_raw)?);
        if !cwd.is_absolute() || cwd.components().any(|c| c == Component::ParentDir) {
            return Err(self.invalid("extension.cwd", "must be an absolute path without `..`"));
        }

        let mut caps = BTreeSet::new();
        let cap_strs = self
            .str_list(ext, "capabilities")
            .map_err(|_| self.invalid("extension.capabilities", "must be a list of strings"))?;
        for (i, s) in cap_strs.into_iter().enumerate() {
            let key = format!("extension.capabilities[{i}]");
            let Some((prefix, operand)) = s.split_once(':') else {
                return Err(self.invalid(key, format!("`{s}` is not a capability atom")));
            };
            if !matches!(prefix, "fs.ro" | "fs.rw" | "net" | "proc") {
                return Err(self.invalid(
                    key,
                    format!(
                        "`{s}`: an extension may require only fs.ro, fs.rw, net and proc atoms"
                    ),
                ));
            }
            let expanded = format!("{prefix}:{}", self.expand(&key, operand)?);
            let cap: Capability = expanded
                .parse()
                .map_err(|e| self.invalid(&key, format!("`{s}`: {e}")))?;
            caps.insert(cap.to_string());
        }
        // Implicit: the process may read its own directory. Under bwrap nothing outside the
        // policy's mounts is guaranteed visible (the scratch tmpfs hides `/tmp`), so this is a
        // real requirement, and the profile must grant it like any other.
        let own = Capability::Fs {
            path: Capability::normalize_path(self.dir)
                .map_err(|e| self.invalid("extension", format!("directory: {e}")))?,
            mode: FsMode::Ro,
        };
        caps.insert(own.to_string());
        let capabilities: Vec<Capability> = caps
            .iter()
            .map(|s| s.parse().expect("round-trips"))
            .collect();
        let program = &command[0];
        let wanted: Capability = format!("proc:{program}")
            .parse()
            .map_err(|e| self.invalid("extension.command[0]", format!("`{program}`: {e}")))?;
        if !capabilities.contains(&wanted) {
            return Err(self.invalid(
                "extension.capabilities",
                format!("must include `{wanted}` (the program in `command`)"),
            ));
        }

        let tools_v = match t.get("tools") {
            Some(Value::Array(a)) if !a.is_empty() => a,
            Some(Value::Array(_)) | None => {
                return Err(self.invalid("tools", "at least one [[tools]] entry is required"));
            }
            Some(_) => return Err(self.invalid("tools", "must be an array of tables")),
        };
        let mut tools = Vec::with_capacity(tools_v.len());
        let mut names = BTreeSet::new();
        for (i, v) in tools_v.iter().enumerate() {
            let p = format!("tools[{i}].");
            let Value::Table(tt) = v else {
                return Err(self.invalid(format!("tools[{i}]"), "must be a table"));
            };
            self.closed(
                tt,
                &p,
                &["name", "description", "input_schema", "schema_file"],
            )?;
            let tname = self.req_str(tt, &p, "name")?;
            let full = format!("{EXT_TOOL_PREFIX}{name}.{tname}");
            if !is_ident(tname) || !is_valid_tool_name(&full) {
                return Err(self.invalid(
                    format!("{p}name"),
                    format!(
                        "must match [a-z][a-z0-9_-]* and `{full}` must be at most 64 characters"
                    ),
                ));
            }
            if !names.insert(tname.to_owned()) {
                return Err(self.invalid(format!("{p}name"), format!("duplicate tool `{tname}`")));
            }
            let tdesc = self.req_str(tt, &p, "description")?.to_owned();
            let schema = match (tt.get("input_schema"), self.str_at(tt, &p, "schema_file")?) {
                (Some(Value::Table(s)), None) => serde_json::to_value(s)
                    .map_err(|e| self.invalid(format!("{p}input_schema"), e.to_string()))?,
                (Some(_), None) => {
                    return Err(self.invalid(format!("{p}input_schema"), "must be a table"));
                }
                (None, Some(file)) => self.schema_file(&format!("{p}schema_file"), file)?,
                (None, None) => {
                    return Err(self.invalid(
                        format!("{p}input_schema"),
                        "one of `input_schema` or `schema_file` is required",
                    ));
                }
                (Some(_), Some(_)) => {
                    return Err(self.invalid(
                        format!("{p}schema_file"),
                        "give `input_schema` or `schema_file`, not both",
                    ));
                }
            };
            if schema.get("type") != Some(&Json::String("object".to_owned())) {
                return Err(self.invalid(
                    format!("{p}input_schema"),
                    "must be a JSON Schema object with \"type\": \"object\"",
                ));
            }
            tools.push(ToolSpec {
                name: tname.to_owned(),
                description: tdesc,
                input_schema: schema,
            });
        }

        Ok(Manifest {
            dir: self.dir.to_path_buf(),
            hash,
            name: name.to_owned(),
            version,
            description,
            kind,
            command,
            cwd,
            capabilities,
            tools,
        })
    }

    /// A schema file: relative to the extension directory, never outside it.
    fn schema_file(&self, key: &str, file: &str) -> Result<Json, ExtError> {
        let rel = Path::new(file);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(self.invalid(
                key,
                "must be a relative path inside the extension directory",
            ));
        }
        let path = self.dir.join(rel);
        // A symlink must not lead the read outside the extension directory either.
        if path
            .canonicalize()
            .is_ok_and(|real| !real.starts_with(self.dir))
        {
            return Err(self.invalid(key, "must stay inside the extension directory"));
        }
        let bytes = std::fs::read(&path).map_err(|source| ExtError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|e| ExtError::Parse {
            path,
            message: e.to_string(),
        })
    }
}
