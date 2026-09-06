//! Secret sources (D10): where a host looks secret *names* up. A source hands out values only to
//! the host's `SecretResolver` implementation; nothing here prints a value in `Debug`.

use std::collections::BTreeMap;
use std::fmt;

use kernel::SecretString;

/// Where a host finds secret values.
///
/// `Host::secret(name)` succeeds iff [`SecretSource::contains`] does; `resolve_secret` calls
/// [`SecretSource::get`]. Implementations MUST NOT print values in their `Debug` output.
pub trait SecretSource: Send + Sync + fmt::Debug {
    /// Short tag used as the locator prefix of issued handles (`"env"`, `"map"`).
    fn kind(&self) -> &'static str;
    /// Whether `name` is known.
    fn contains(&self, name: &str) -> bool;
    /// The value for `name`, if known.
    fn get(&self, name: &str) -> Option<SecretString>;
}

/// Names-only `Debug` for a value map.
fn fmt_names(
    f: &mut fmt::Formatter<'_>,
    ty: &str,
    values: &BTreeMap<String, SecretString>,
) -> fmt::Result {
    f.debug_struct(ty)
        .field("names", &values.keys().collect::<Vec<_>>())
        .finish()
}

/// A snapshot of the process environment, taken once at construction. `contains(name)` is true
/// iff the variable `name` existed (with a UTF-8 value) when the snapshot was taken; the
/// environment is never re-read.
pub struct EnvSecretSource {
    values: BTreeMap<String, SecretString>,
}

impl EnvSecretSource {
    /// Snapshot the current process environment. Variables whose name or value is not UTF-8 are
    /// skipped.
    pub fn from_env() -> EnvSecretSource {
        let values = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .map(|(k, v)| (k, SecretString::new(v)))
            .collect();
        EnvSecretSource { values }
    }

    /// Use an explicit snapshot instead of the live environment (launchers that scrub, tests).
    pub fn from_snapshot(snapshot: BTreeMap<String, String>) -> EnvSecretSource {
        EnvSecretSource {
            values: snapshot
                .into_iter()
                .map(|(k, v)| (k, SecretString::new(v)))
                .collect(),
        }
    }

    /// The names in the snapshot.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

impl fmt::Debug for EnvSecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_names(f, "EnvSecretSource", &self.values)
    }
}

impl SecretSource for EnvSecretSource {
    fn kind(&self) -> &'static str {
        "env"
    }

    fn contains(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    fn get(&self, name: &str) -> Option<SecretString> {
        self.values.get(name).cloned()
    }
}

/// An in-memory name → value map (tests, launchers that load a secrets file themselves).
pub struct MapSecretSource {
    values: BTreeMap<String, SecretString>,
}

impl MapSecretSource {
    /// Build from any `(name, value)` pairs.
    pub fn new<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> MapSecretSource
    where
        K: Into<String>,
        V: Into<String>,
    {
        MapSecretSource {
            values: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), SecretString::new(v)))
                .collect(),
        }
    }

    /// A source that knows no secrets.
    pub fn empty() -> MapSecretSource {
        MapSecretSource {
            values: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for MapSecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_names(f, "MapSecretSource", &self.values)
    }
}

impl SecretSource for MapSecretSource {
    fn kind(&self) -> &'static str {
        "map"
    }

    fn contains(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    fn get(&self, name: &str) -> Option<SecretString> {
        self.values.get(name).cloned()
    }
}
