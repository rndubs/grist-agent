//! Capability atoms (D6) and the `narrower_than` partial order.
//!
//! Bundles (`meshing`, `solver`, `post`) are a `profiles` concept and are expanded to
//! atoms before anything reaches the kernel; the kernel never sees a bundle name.
//! The canonical string form (`fs.rw:/work/repo`, `net:*`, `proc:sbatch`, …) is the
//! serialized form in JSON and the TOML form in profiles (`profile-schema.md` §11).

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filesystem access mode. `Ro < Rw`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsMode {
    /// Read-only.
    Ro,
    /// Read-write.
    Rw,
}

/// Network allowlist.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetAllow {
    /// Any host.
    Any,
    /// Host names (optionally `host:port`), compared case-insensitively after ASCII lowercasing.
    Hosts(BTreeSet<String>),
}

/// A capability atom (D6). Serialized form in JSON is the canonical string (see `Display`),
/// e.g. `"fs.rw:/work/repo"`, so logs and TOML profiles read the same way.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Capability {
    /// `fs.ro:<abs-path>` / `fs.rw:<abs-path>`. Path MUST be absolute, normalized, and contain no `..`.
    Fs {
        /// Absolute, normalized path.
        path: PathBuf,
        /// Access mode.
        mode: FsMode,
    },
    /// `net:*` / `net:<host>[,<host>...]`
    Net {
        /// The allowlist.
        allow: NetAllow,
    },
    /// `proc:<program>` — program name or absolute path the tool may execute (e.g. `proc:sbatch`).
    Proc {
        /// Program name or absolute path.
        program: String,
    },
    /// `tool:<name>` — the tool may be invoked / registered.
    Tool {
        /// Tool name.
        name: String,
    },
    /// `spawn:<catalog-name>` — the agent may spawn this catalog entry as a sub-agent (P2.4).
    Spawn {
        /// Catalog entry name.
        catalog_name: String,
    },
    /// `secret:<name>` — a provider client may resolve this secret (D10). Never reaches a sandbox.
    Secret {
        /// Secret name.
        name: String,
    },
}

/// Errors from parsing a capability string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityParseError {
    /// The prefix before `:` is not one of the six atom prefixes.
    #[error("unknown capability prefix in `{0}`")]
    UnknownPrefix(String),
    /// An `fs` path that is relative, contains `..`, or otherwise cannot be normalized.
    #[error("fs path must be absolute, normalized, without `..`: `{0}`")]
    BadPath(String),
    /// The operand after the prefix is empty.
    #[error("empty name in `{0}`")]
    EmptyName(String),
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Capability::Fs { path, mode } => {
                let m = match mode {
                    FsMode::Ro => "ro",
                    FsMode::Rw => "rw",
                };
                write!(f, "fs.{m}:{}", path.display())
            }
            Capability::Net {
                allow: NetAllow::Any,
            } => f.write_str("net:*"),
            Capability::Net {
                allow: NetAllow::Hosts(hosts),
            } => {
                f.write_str("net:")?;
                for (i, h) in hosts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    f.write_str(h)?;
                }
                Ok(())
            }
            Capability::Proc { program } => write!(f, "proc:{program}"),
            Capability::Tool { name } => write!(f, "tool:{name}"),
            Capability::Spawn { catalog_name } => write!(f, "spawn:{catalog_name}"),
            Capability::Secret { name } => write!(f, "secret:{name}"),
        }
    }
}

impl std::str::FromStr for Capability {
    type Err = CapabilityParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((prefix, rest)) = s.split_once(':') else {
            return Err(CapabilityParseError::UnknownPrefix(s.to_owned()));
        };
        let nonempty = |v: &str| -> Result<String, CapabilityParseError> {
            if v.is_empty() {
                Err(CapabilityParseError::EmptyName(s.to_owned()))
            } else {
                Ok(v.to_owned())
            }
        };
        match prefix {
            "fs.ro" | "fs.rw" => {
                if rest.is_empty() {
                    return Err(CapabilityParseError::EmptyName(s.to_owned()));
                }
                let path = Capability::normalize_path(Path::new(rest))?;
                let mode = if prefix == "fs.ro" {
                    FsMode::Ro
                } else {
                    FsMode::Rw
                };
                Ok(Capability::Fs { path, mode })
            }
            "net" => {
                if rest.is_empty() {
                    return Err(CapabilityParseError::EmptyName(s.to_owned()));
                }
                if rest == "*" {
                    return Ok(Capability::Net {
                        allow: NetAllow::Any,
                    });
                }
                let mut hosts = BTreeSet::new();
                for h in rest.split(',') {
                    let h = normalize_host(h);
                    if h.is_empty() || h == "*" {
                        return Err(CapabilityParseError::EmptyName(s.to_owned()));
                    }
                    hosts.insert(h);
                }
                Ok(Capability::Net {
                    allow: NetAllow::Hosts(normalize_hosts(hosts)),
                })
            }
            "proc" => {
                let program = nonempty(rest)?;
                let program = if program.starts_with('/') {
                    Capability::normalize_path(Path::new(&program))?
                        .display()
                        .to_string()
                } else {
                    program
                };
                Ok(Capability::Proc { program })
            }
            "tool" => Ok(Capability::Tool {
                name: nonempty(rest)?,
            }),
            "spawn" => Ok(Capability::Spawn {
                catalog_name: nonempty(rest)?,
            }),
            "secret" => Ok(Capability::Secret {
                name: nonempty(rest)?,
            }),
            _ => Err(CapabilityParseError::UnknownPrefix(s.to_owned())),
        }
    }
}

impl TryFrom<String> for Capability {
    type Error = CapabilityParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Capability> for String {
    fn from(c: Capability) -> Self {
        c.to_string()
    }
}

/// Lowercase, strip one trailing `.` from the hostname part (`profile-schema.md` §11.3).
fn normalize_host(h: &str) -> String {
    let h = h.trim().to_ascii_lowercase();
    match h.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{}:{port}", name.trim_end_matches('.'))
        }
        _ => h.trim_end_matches('.').to_owned(),
    }
}

/// Drop every `host:port` whose bare `host` is also present (it is implied), so equal
/// permissions have one representation and `narrower_than` is antisymmetric.
pub fn normalize_hosts(hosts: BTreeSet<String>) -> BTreeSet<String> {
    let bare: BTreeSet<&str> = hosts
        .iter()
        .filter(|h| host_parts(h).1.is_none())
        .map(String::as_str)
        .collect();
    hosts
        .iter()
        .filter(|h| match host_parts(h) {
            (name, Some(_)) => !bare.contains(name),
            _ => true,
        })
        .cloned()
        .collect()
}

/// `host[:port]` split.
fn host_parts(h: &str) -> (&str, Option<&str>) {
    match h.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (name, Some(port))
        }
        _ => (h, None),
    }
}

/// `a` is covered by `b`: equal, or `a` has a port and `b` is the same host without one.
fn host_covered(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (an, ap) = host_parts(a);
    let (bn, bp) = host_parts(b);
    an == bn && ap.is_some() && bp.is_none()
}

impl Capability {
    /// Partial order. Reflexive. Atoms of different variants are never comparable (returns false).
    ///
    /// - `Fs`: `self.path` equals or is under `other.path` component-wise (after normalization,
    ///   no `..`), AND `self.mode <= other.mode` where `Ro <= Ro`, `Ro <= Rw`, `Rw <= Rw`.
    /// - `Net`: `Hosts(a) <= Hosts(b)` iff every host of `a` is covered by a host of `b` (equal,
    ///   or `a`'s host carries a port and `b`'s is the same host without one); anything `<= Any`;
    ///   `Any <= Any` only.
    /// - `Proc`, `Tool`, `Spawn`, `Secret`: exact name equality.
    pub fn narrower_than(&self, other: &Capability) -> bool {
        match (self, other) {
            (Capability::Fs { path: a, mode: am }, Capability::Fs { path: b, mode: bm }) => {
                am <= bm && a.starts_with(b)
            }
            (
                Capability::Net { .. },
                Capability::Net {
                    allow: NetAllow::Any,
                },
            ) => true,
            (
                Capability::Net {
                    allow: NetAllow::Any,
                },
                Capability::Net { .. },
            ) => false,
            (
                Capability::Net {
                    allow: NetAllow::Hosts(a),
                },
                Capability::Net {
                    allow: NetAllow::Hosts(b),
                },
            ) => a.iter().all(|ah| b.iter().any(|bh| host_covered(ah, bh))),
            (Capability::Proc { program: a }, Capability::Proc { program: b }) => a == b,
            (Capability::Tool { name: a }, Capability::Tool { name: b }) => a == b,
            (Capability::Spawn { catalog_name: a }, Capability::Spawn { catalog_name: b }) => {
                a == b
            }
            (Capability::Secret { name: a }, Capability::Secret { name: b }) => a == b,
            _ => false,
        }
    }

    /// True iff some element of `grants` is wider than or equal to `self`.
    pub fn covered_by(&self, grants: &[Capability]) -> bool {
        grants.iter().any(|g| self.narrower_than(g))
    }

    /// Normalize an `Fs` path: reject relative paths and `..`; collapse `.` and duplicate separators.
    /// Symlinks are NOT resolved here (pure function); the sandbox resolves them at mount time.
    pub fn normalize_path(path: &Path) -> Result<PathBuf, CapabilityParseError> {
        let bad = || CapabilityParseError::BadPath(path.display().to_string());
        if !path.is_absolute() {
            return Err(bad());
        }
        let mut out = PathBuf::from("/");
        for c in path.components() {
            match c {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir | Component::Prefix(_) => return Err(bad()),
                Component::Normal(seg) => {
                    if seg.is_empty() {
                        return Err(bad());
                    }
                    out.push(seg);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cap(s: &str) -> Capability {
        s.parse().unwrap()
    }

    #[test]
    fn display_parse_round_trip_and_normalization() {
        for s in [
            "fs.ro:/work/repo",
            "fs.rw:/",
            "net:*",
            "net:a.example,b.example:443",
            "proc:sbatch",
            "proc:/usr/bin/sbatch",
            "tool:mcp.docs.search",
            "spawn:debugger",
            "secret:LITELLM_CI_API_KEY",
        ] {
            assert_eq!(cap(s).to_string(), s);
            let json = serde_json::to_string(&cap(s)).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: Capability = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cap(s));
        }
        assert_eq!(
            cap("fs.rw:/work//repo/./x/").to_string(),
            "fs.rw:/work/repo/x"
        );
        assert_eq!(
            cap("net:B.Example.,a.example").to_string(),
            "net:a.example,b.example"
        );
        // A `host:port` is implied by the bare host and is dropped on normalization.
        assert_eq!(
            cap("net:a.example:443,a.example").to_string(),
            "net:a.example"
        );
        assert_eq!(cap("proc:/usr//bin/./x").to_string(), "proc:/usr/bin/x");
    }

    #[test]
    fn parse_rejections() {
        assert!(matches!(
            "fs.rwx:/x".parse::<Capability>(),
            Err(CapabilityParseError::UnknownPrefix(_))
        ));
        assert!(matches!(
            "Fs.ro:/x".parse::<Capability>(),
            Err(CapabilityParseError::UnknownPrefix(_))
        ));
        assert!(matches!(
            "meshing".parse::<Capability>(),
            Err(CapabilityParseError::UnknownPrefix(_))
        ));
        assert!(matches!(
            "fs.ro:".parse::<Capability>(),
            Err(CapabilityParseError::EmptyName(_))
        ));
        assert!(matches!(
            "net:".parse::<Capability>(),
            Err(CapabilityParseError::EmptyName(_))
        ));
        assert!(matches!(
            "net:*,a".parse::<Capability>(),
            Err(CapabilityParseError::EmptyName(_))
        ));
        assert!(matches!(
            "proc:".parse::<Capability>(),
            Err(CapabilityParseError::EmptyName(_))
        ));
        assert!(matches!(
            "fs.ro:relative/path".parse::<Capability>(),
            Err(CapabilityParseError::BadPath(_))
        ));
        assert!(matches!(
            "fs.ro:/work/../etc".parse::<Capability>(),
            Err(CapabilityParseError::BadPath(_))
        ));
        assert!(matches!(
            "fs.ro:${workdir}/x".parse::<Capability>(),
            Err(CapabilityParseError::BadPath(_))
        ));
    }

    #[test]
    fn narrower_than_examples_from_the_spec() {
        // Fs
        assert!(cap("fs.ro:/w/src").narrower_than(&cap("fs.rw:/w")));
        assert!(cap("fs.ro:/w").narrower_than(&cap("fs.rw:/w")));
        assert!(cap("fs.rw:/w/a/b").narrower_than(&cap("fs.rw:/w")));
        assert!(!cap("fs.rw:/w").narrower_than(&cap("fs.ro:/w")));
        assert!(!cap("fs.ro:/work").narrower_than(&cap("fs.ro:/w")));
        assert!(!cap("fs.ro:/work/repo2").narrower_than(&cap("fs.ro:/work/repo")));
        assert!(!cap("fs.ro:/").narrower_than(&cap("fs.rw:/w")));
        assert!(cap("fs.rw:/w").narrower_than(&cap("fs.rw:/")));
        // Net
        assert!(cap("net:a.example").narrower_than(&cap("net:*")));
        assert!(cap("net:*").narrower_than(&cap("net:*")));
        assert!(cap("net:a.example").narrower_than(&cap("net:a.example,b.example")));
        assert!(cap("net:a.example:443").narrower_than(&cap("net:a.example")));
        assert!(!cap("net:*").narrower_than(&cap("net:a.example")));
        assert!(!cap("net:a.example").narrower_than(&cap("net:a.example:443")));
        assert!(!cap("net:sub.a.example").narrower_than(&cap("net:a.example")));
        // Exact-name variants
        assert!(cap("proc:sbatch").narrower_than(&cap("proc:sbatch")));
        assert!(!cap("proc:/usr/bin/sbatch").narrower_than(&cap("proc:sbatch")));
        assert!(cap("tool:read").narrower_than(&cap("tool:read")));
        assert!(!cap("tool:mcp.docs.search").narrower_than(&cap("tool:mcp.docs.*")));
        assert!(cap("spawn:x").narrower_than(&cap("spawn:x")));
        assert!(cap("secret:X").narrower_than(&cap("secret:X")));
        assert!(!cap("secret:X").narrower_than(&cap("secret:Y")));
        // Incomparable variants both ways.
        assert!(!cap("fs.ro:/w").narrower_than(&cap("proc:sbatch")));
        assert!(!cap("proc:sbatch").narrower_than(&cap("fs.rw:/")));
        assert!(!cap("net:*").narrower_than(&cap("fs.rw:/")));
    }

    #[test]
    fn covered_by_is_per_atom_not_union() {
        let grants = [cap("fs.ro:/w"), cap("fs.rw:/w/sub")];
        assert!(!cap("fs.rw:/w").covered_by(&grants));
        assert!(cap("fs.rw:/w/sub/x").covered_by(&grants));
        assert!(cap("fs.ro:/w/other").covered_by(&grants));
        assert!(!cap("fs.ro:/w/other").covered_by(&[]));
    }

    // ---- property tests -------------------------------------------------------------

    fn seg() -> impl Strategy<Value = String> {
        prop::sample::select(vec!["a", "b", "c", "work", "repo", "repo2", "src"])
            .prop_map(|s| s.to_owned())
    }

    fn fs_path() -> impl Strategy<Value = PathBuf> {
        prop::collection::vec(seg(), 0..4).prop_map(|segs| {
            let mut p = PathBuf::from("/");
            for s in segs {
                p.push(s);
            }
            p
        })
    }

    fn fs_mode() -> impl Strategy<Value = FsMode> {
        prop_oneof![Just(FsMode::Ro), Just(FsMode::Rw)]
    }

    fn hosts() -> impl Strategy<Value = BTreeSet<String>> {
        prop::collection::btree_set(
            prop::sample::select(vec![
                "a.example",
                "b.example",
                "a.example:443",
                "c.example:80",
                "sub.a.example",
            ])
            .prop_map(|s| s.to_owned()),
            1..4,
        )
        .prop_map(normalize_hosts)
    }

    fn net_allow() -> impl Strategy<Value = NetAllow> {
        prop_oneof![
            1 => Just(NetAllow::Any),
            4 => hosts().prop_map(NetAllow::Hosts),
        ]
    }

    fn any_cap() -> impl Strategy<Value = Capability> {
        prop_oneof![
            (fs_path(), fs_mode()).prop_map(|(path, mode)| Capability::Fs { path, mode }),
            net_allow().prop_map(|allow| Capability::Net { allow }),
            seg().prop_map(|program| Capability::Proc { program }),
            seg().prop_map(|name| Capability::Tool { name }),
            seg().prop_map(|catalog_name| Capability::Spawn { catalog_name }),
            seg().prop_map(|name| Capability::Secret { name }),
        ]
    }

    proptest! {
        #[test]
        fn reflexive(c in any_cap()) {
            prop_assert!(c.narrower_than(&c));
        }

        #[test]
        fn transitive(a in any_cap(), b in any_cap(), c in any_cap()) {
            if a.narrower_than(&b) && b.narrower_than(&c) {
                prop_assert!(a.narrower_than(&c), "{a} <= {b} <= {c}");
            }
        }

        #[test]
        fn antisymmetric_up_to_equality(a in any_cap(), b in any_cap()) {
            if a.narrower_than(&b) && b.narrower_than(&a) {
                prop_assert_eq!(a, b);
            }
        }

        #[test]
        fn round_trip(c in any_cap()) {
            let s = c.to_string();
            let back: Capability = s.parse().unwrap();
            prop_assert_eq!(&back, &c);
            prop_assert_eq!(back.to_string(), s);
        }

        #[test]
        fn rw_never_narrower_than_ro_at_same_path(p in fs_path()) {
            let rw = Capability::Fs { path: p.clone(), mode: FsMode::Rw };
            let ro = Capability::Fs { path: p, mode: FsMode::Ro };
            prop_assert!(!rw.narrower_than(&ro));
            prop_assert!(ro.narrower_than(&rw));
        }

        #[test]
        fn fs_prefix_chain_is_monotone(p in fs_path(), extra in seg(), m in fs_mode()) {
            let parent = Capability::Fs { path: p.clone(), mode: m };
            let child = Capability::Fs { path: p.join(extra), mode: m };
            prop_assert!(child.narrower_than(&parent));
            prop_assert!(!parent.narrower_than(&child));
        }

        #[test]
        fn string_prefix_is_not_component_prefix(p in fs_path(), s in seg()) {
            let base = Capability::Fs { path: p.clone(), mode: FsMode::Rw };
            let sibling = Capability::Fs { path: PathBuf::from(format!("{}{s}2", p.display().to_string().trim_end_matches('/'))), mode: FsMode::Ro };
            // `/work/repo2` is not under `/work/repo`; the only exception is p == "/", where
            // "/x2" is genuinely under "/".
            if p != Path::new("/") {
                prop_assert!(!sibling.narrower_than(&base));
            }
        }

        #[test]
        fn net_subset_is_monotone(a in hosts(), b in hosts()) {
            let mut union = a.clone();
            union.extend(b.iter().cloned());
            let na = Capability::Net { allow: NetAllow::Hosts(a) };
            let nu = Capability::Net { allow: NetAllow::Hosts(union) };
            prop_assert!(na.narrower_than(&nu));
            let any = Capability::Net { allow: NetAllow::Any };
            prop_assert!(na.narrower_than(&any));
        }

        #[test]
        fn any_is_narrower_only_than_any(h in hosts()) {
            let any = Capability::Net { allow: NetAllow::Any };
            let hosts = Capability::Net { allow: NetAllow::Hosts(h) };
            prop_assert!(!any.narrower_than(&hosts));
            prop_assert!(any.narrower_than(&any));
        }

        #[test]
        fn different_variants_are_incomparable(a in any_cap(), b in any_cap()) {
            if std::mem::discriminant(&a) != std::mem::discriminant(&b) {
                prop_assert!(!a.narrower_than(&b));
                prop_assert!(!b.narrower_than(&a));
            }
        }
    }
}
