//! Capability-atom string grammar (`profile-schema.md` §11), placeholders (§7.4), expansion and
//! normalization (§11.3). The kernel's `Capability::from_str` parses the expanded, absolute form;
//! this module owns the symbolic TOML form.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use kernel::{Capability, FsMode};

/// The six atom prefixes (also the reserved words that may not be bundle names).
pub const ATOM_PREFIXES: &[&str] = &["fs.ro", "fs.rw", "net", "proc", "tool", "spawn", "secret"];

/// Reserved words that may not be bundle names (§11.1).
pub const RESERVED_BUNDLE_NAMES: &[&str] = &["fs", "net", "proc", "tool", "spawn", "secret"];

/// `[a-z][a-z0-9_-]*`
pub fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// `[a-z][a-z0-9_]*`
pub fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `[a-z][a-z0-9-]*`
pub fn is_endpoint_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `[A-Z][A-Z0-9_]*`
pub fn is_secret_name_upper(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `ident *("." ident)` — tool names (`read`, `mcp.docs.search`).
pub fn is_tool_name(s: &str) -> bool {
    !s.is_empty() && s.split('.').all(is_ident)
}

/// Printable ASCII, no whitespace.
fn is_pchar_string(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// A parsed placeholder (§7.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placeholder {
    /// `${workdir}`
    Workdir,
    /// `${home}`
    Home,
    /// `${install:<tool>}`
    Install(String),
}

impl std::fmt::Display for Placeholder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Placeholder::Workdir => f.write_str("${workdir}"),
            Placeholder::Home => f.write_str("${home}"),
            Placeholder::Install(t) => write!(f, "${{install:{t}}}"),
        }
    }
}

/// Why a string failed the grammar or the expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtomError {
    /// `E_MALFORMED_ATOM`
    Malformed(String),
    /// `E_UNKNOWN_PLACEHOLDER` (the offending `${…}` text)
    UnknownPlaceholder(String),
    /// `E_PATH_NOT_ABSOLUTE` (the offending path)
    NotAbsolute(String),
    /// `E_PATH_DOTDOT`
    DotDot(String),
}

impl AtomError {
    /// The diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            AtomError::Malformed(_) => "E_MALFORMED_ATOM",
            AtomError::UnknownPlaceholder(_) => "E_UNKNOWN_PLACEHOLDER",
            AtomError::NotAbsolute(_) => "E_PATH_NOT_ABSOLUTE",
            AtomError::DotDot(_) => "E_PATH_DOTDOT",
        }
    }

    /// The message part of the diagnostic.
    pub fn message(&self) -> String {
        match self {
            AtomError::Malformed(s) => format!("'{s}'"),
            AtomError::UnknownPlaceholder(s) => format!("'{s}'"),
            AtomError::NotAbsolute(s) => format!("'{s}'"),
            AtomError::DotDot(s) => format!("'{s}' contains '..'"),
        }
    }
}

/// If `s` starts with `${`, parse the leading placeholder and return it with the remainder.
/// `Ok(None)` when `s` does not start with a placeholder.
pub fn leading_placeholder(s: &str) -> Result<Option<(Placeholder, &str)>, AtomError> {
    if !s.starts_with("${") {
        return Ok(None);
    }
    let Some(end) = s.find('}') else {
        return Err(AtomError::UnknownPlaceholder(s.to_owned()));
    };
    let inner = &s[2..end];
    let text = &s[..=end];
    let rest = &s[end + 1..];
    let ph = match inner.split_once(':') {
        None if inner == "workdir" => Placeholder::Workdir,
        None if inner == "home" => Placeholder::Home,
        Some(("install", arg)) if is_ident(arg) => Placeholder::Install(arg.to_owned()),
        _ => return Err(AtomError::UnknownPlaceholder(text.to_owned())),
    };
    Ok(Some((ph, rest)))
}

/// Grammar check for a path-typed profile value (symbolic form, before expansion): a placeholder
/// may appear only at the start; anything else is left to expansion (relative paths in files
/// are resolved against the containing file by the loader).
pub fn check_path_grammar(s: &str) -> Result<(), AtomError> {
    if s.is_empty() {
        return Err(AtomError::NotAbsolute(s.to_owned()));
    }
    let rest = match leading_placeholder(s)? {
        Some((_, rest)) => rest,
        None => s,
    };
    if rest.contains("${") {
        return Err(AtomError::NotAbsolute(format!(
            "{s} (placeholders are only allowed at the start)"
        )));
    }
    Ok(())
}

/// Classification of one string in a grants-style list (§11.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantItem<'a> {
    /// Has a `:` → an atom with this prefix and operand.
    Atom {
        /// One of `ATOM_PREFIXES`.
        prefix: &'a str,
        /// Everything after the first `:`.
        operand: &'a str,
    },
    /// No `:` → a bundle name.
    Bundle(&'a str),
}

/// Split a grant string into atom / bundle name. Does not validate the operand.
pub fn classify(s: &str) -> Result<GrantItem<'_>, AtomError> {
    match s.split_once(':') {
        Some((prefix, operand)) => {
            if !ATOM_PREFIXES.contains(&prefix) {
                return Err(AtomError::Malformed(s.to_owned()));
            }
            Ok(GrantItem::Atom { prefix, operand })
        }
        None => {
            if !is_ident(s) || RESERVED_BUNDLE_NAMES.contains(&s) {
                return Err(AtomError::Malformed(s.to_owned()));
            }
            Ok(GrantItem::Bundle(s))
        }
    }
}

fn is_host(h: &str) -> bool {
    let (name, port) = match h.rsplit_once(':') {
        Some((n, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (n, Some(p)),
        _ => (h, None),
    };
    if let Some(p) = port
        && p.len() > 5
    {
        return false;
    }
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Check the symbolic grammar of one atom string (§11.1). Bundle names are rejected here; use
/// `classify` first when bundles are allowed. Relative `fs` paths pass (they fail at expansion
/// with `E_PATH_NOT_ABSOLUTE`, §9 case 8).
pub fn check_atom_grammar(s: &str) -> Result<(), AtomError> {
    let Some((prefix, operand)) = s.split_once(':') else {
        return Err(AtomError::Malformed(s.to_owned()));
    };
    let malformed = || AtomError::Malformed(s.to_owned());
    if !ATOM_PREFIXES.contains(&prefix) || operand.is_empty() {
        return Err(malformed());
    }
    match prefix {
        "fs.ro" | "fs.rw" => {
            if !is_pchar_string(operand) {
                return Err(malformed());
            }
            let rest = match leading_placeholder(operand)? {
                Some((_, rest)) => rest,
                None => operand,
            };
            if rest.contains("${") {
                return Err(malformed());
            }
            Ok(())
        }
        "net" => {
            if operand == "*" {
                return Ok(());
            }
            if operand.split(',').all(is_host) {
                Ok(())
            } else {
                Err(malformed())
            }
        }
        "proc" => {
            if is_ident(operand) || (operand.starts_with('/') && is_pchar_string(operand)) {
                Ok(())
            } else {
                Err(malformed())
            }
        }
        "tool" => {
            if is_tool_name(operand) {
                Ok(())
            } else {
                Err(malformed())
            }
        }
        "spawn" => {
            if is_ident(operand) {
                Ok(())
            } else {
                Err(malformed())
            }
        }
        "secret" => {
            if operand
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                Ok(())
            } else {
                Err(malformed())
            }
        }
        _ => Err(malformed()),
    }
}

/// Binds placeholders to concrete values (§7.4).
#[derive(Clone, Debug)]
pub struct Expander<'a> {
    /// `${workdir}`
    pub workdir: &'a Path,
    /// `${home}`
    pub home: &'a Path,
    /// `${install:<tool>}` → `bundles.toml [install]`
    pub install: &'a BTreeMap<String, String>,
}

impl Expander<'_> {
    fn substitute(&self, ph: &Placeholder) -> Result<String, AtomError> {
        Ok(match ph {
            Placeholder::Workdir => self.workdir.to_string_lossy().into_owned(),
            Placeholder::Home => self.home.to_string_lossy().into_owned(),
            Placeholder::Install(tool) => self
                .install
                .get(tool)
                .cloned()
                .ok_or_else(|| AtomError::UnknownPlaceholder(ph.to_string()))?,
        })
    }

    /// Expand a leading placeholder (if any) and return the raw, un-normalized string.
    pub fn expand_raw(&self, s: &str) -> Result<String, AtomError> {
        match leading_placeholder(s)? {
            Some((ph, rest)) => Ok(format!("{}{}", self.substitute(&ph)?, rest)),
            None => Ok(s.to_owned()),
        }
    }

    /// Expand and normalize a path-typed value (§11.3): absolute, no `..`, collapsed.
    pub fn expand_path(&self, s: &str) -> Result<PathBuf, AtomError> {
        let raw = self.expand_raw(s)?;
        normalize_abs_path(&raw)
    }

    /// Expand a symbolic atom string into a `kernel::Capability`.
    pub fn expand_atom(&self, s: &str) -> Result<Capability, AtomError> {
        check_atom_grammar(s)?;
        let (prefix, operand) = s.split_once(':').expect("grammar checked");
        match prefix {
            "fs.ro" | "fs.rw" => {
                let path = self.expand_path(operand)?;
                let mode = if prefix == "fs.ro" {
                    FsMode::Ro
                } else {
                    FsMode::Rw
                };
                Ok(Capability::Fs { path, mode })
            }
            "proc" if operand.starts_with('/') => {
                let path = normalize_abs_path(operand)?;
                Ok(Capability::Proc {
                    program: path.to_string_lossy().into_owned(),
                })
            }
            _ => s
                .parse::<Capability>()
                .map_err(|_| AtomError::Malformed(s.to_owned())),
        }
    }
}

/// Normalize an already-expanded path: must be absolute (`E_PATH_NOT_ABSOLUTE`), must not contain
/// `..` (`E_PATH_DOTDOT`); `.` segments and repeated separators are collapsed.
pub fn normalize_abs_path(raw: &str) -> Result<PathBuf, AtomError> {
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err(AtomError::NotAbsolute(raw.to_owned()));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(AtomError::DotDot(raw.to_owned()));
    }
    Capability::normalize_path(p).map_err(|_| AtomError::NotAbsolute(raw.to_owned()))
}

/// Render a concrete capability in the symbolic form used in messages (§9 cases 3 and 33): a
/// path under `workdir` / `home` is shown as `${workdir}/…` / `${home}/…`.
pub fn symbolize(cap: &Capability, workdir: &Path, home: &Path) -> String {
    let sym_path = |p: &Path| -> String {
        if let Ok(rest) = p.strip_prefix(workdir) {
            join_symbolic("${workdir}", rest)
        } else if let Ok(rest) = p.strip_prefix(home) {
            join_symbolic("${home}", rest)
        } else {
            p.to_string_lossy().into_owned()
        }
    };
    match cap {
        Capability::Fs { path, mode } => {
            let m = match mode {
                FsMode::Ro => "ro",
                FsMode::Rw => "rw",
            };
            format!("fs.{m}:{}", sym_path(path))
        }
        Capability::Proc { program } if program.starts_with('/') => {
            format!("proc:{}", sym_path(Path::new(program)))
        }
        other => other.to_string(),
    }
}

fn join_symbolic(prefix: &str, rest: &Path) -> String {
    if rest.as_os_str().is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}/{}", rest.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_accepts_the_spec_examples() {
        for s in [
            "fs.rw:${workdir}",
            "fs.ro:${home}/.cache/pip",
            "fs.ro:${install:mesher}",
            "fs.ro:/srv/docs",
            "net:*",
            "net:a.example,b.example:443",
            "proc:sbatch",
            "proc:/usr/bin/sbatch",
            "tool:mcp.docs.search",
            "spawn:debugger",
            "secret:LITELLM_CI_API_KEY",
            "fs.ro:data/meshes",
        ] {
            assert_eq!(check_atom_grammar(s), Ok(()), "{s}");
        }
    }

    #[test]
    fn grammar_rejects_malformed_atoms() {
        for s in [
            "fs.rwx:${workdir}",
            "net:",
            "proc:",
            "fs.ro:",
            "Fs.ro:/x",
            "net:*,a.example",
            "net:A.Example",
            "proc:Sbatch",
            "tool:Read",
            "fs.ro:/x/${workdir}",
            "meshing",
        ] {
            assert!(
                matches!(check_atom_grammar(s), Err(AtomError::Malformed(_))),
                "{s}"
            );
        }
        assert!(matches!(
            check_atom_grammar("fs.rw:${repo}"),
            Err(AtomError::UnknownPlaceholder(_))
        ));
    }

    #[test]
    fn classify_splits_bundles_from_atoms() {
        assert_eq!(classify("meshing").unwrap(), GrantItem::Bundle("meshing"));
        assert!(matches!(
            classify("fs.rw:/x").unwrap(),
            GrantItem::Atom {
                prefix: "fs.rw",
                ..
            }
        ));
        assert!(classify("fs").is_err());
        assert!(classify("Meshing").is_err());
        assert!(classify("foo:bar").is_err());
    }

    #[test]
    fn expansion_and_symbolize_round_trip() {
        let install = BTreeMap::from([("solver".to_owned(), "/opt/inhouse/solver".to_owned())]);
        let ex = Expander {
            workdir: Path::new("/work/repo"),
            home: Path::new("/home/u"),
            install: &install,
        };
        let c = ex.expand_atom("fs.rw:${workdir}//src/./x").unwrap();
        assert_eq!(c.to_string(), "fs.rw:/work/repo/src/x");
        assert_eq!(symbolize(&c, ex.workdir, ex.home), "fs.rw:${workdir}/src/x");
        let c = ex.expand_atom("fs.ro:${install:solver}").unwrap();
        assert_eq!(c.to_string(), "fs.ro:/opt/inhouse/solver");
        assert_eq!(
            symbolize(&c, ex.workdir, ex.home),
            "fs.ro:/opt/inhouse/solver"
        );
        let c = ex.expand_atom("fs.rw:${home}").unwrap();
        assert_eq!(symbolize(&c, ex.workdir, ex.home), "fs.rw:${home}");
        assert!(matches!(
            ex.expand_atom("fs.ro:data/meshes"),
            Err(AtomError::NotAbsolute(_))
        ));
        assert!(matches!(
            ex.expand_atom("fs.rw:${workdir}/../shared"),
            Err(AtomError::DotDot(_))
        ));
        assert!(matches!(
            ex.expand_atom("fs.ro:${install:thermal}"),
            Err(AtomError::UnknownPlaceholder(_))
        ));
        assert_eq!(
            ex.expand_path("${workdir}/AGENTS.md").unwrap(),
            PathBuf::from("/work/repo/AGENTS.md")
        );
    }
}
