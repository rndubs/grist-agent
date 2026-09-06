//! Validator diagnostics (`profile-schema.md` §7.3).

use std::path::{Path, PathBuf};

/// One validator diagnostic. Codes starting `E_` are errors (resolution fails); `W_` are
/// warnings (logged as `warning{class: "profile_warning"}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// `"E_UNKNOWN_KEY"`, `"W_NET_MASKED"`, …
    pub code: &'static str,
    /// The file the diagnostic points at; `None` for diagnostics that have no file
    /// (runtime overrides, merge-time findings with no originating layer).
    pub file: Option<PathBuf>,
    /// TOML path, e.g. `"agent.context_budget_tokens"`, `"middleware[2].priority"`,
    /// `"capabilities.grants[3]"`.
    pub toml_path: String,
    /// Human-readable detail.
    pub message: String,
}

impl Diagnostic {
    /// Build a diagnostic pointing at `file` (any path; `Display` shows the basename).
    pub fn new(
        code: &'static str,
        file: Option<&Path>,
        toml_path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Diagnostic {
            code,
            file: file.map(Path::to_path_buf),
            toml_path: toml_path.into(),
            message: message.into(),
        }
    }

    /// True for `E_*` codes.
    pub fn is_error(&self) -> bool {
        self.code.starts_with("E_")
    }

    /// True for `W_*` codes.
    pub fn is_warning(&self) -> bool {
        self.code.starts_with("W_")
    }

    /// The `Display` prefix `<code> at <file-basename>:<toml_path>` (what the §9 tests assert on).
    pub fn location(&self) -> String {
        let file = self
            .file
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned());
        match (file, self.toml_path.is_empty()) {
            (Some(f), false) => format!("{} at {}:{}", self.code, f, self.toml_path),
            (Some(f), true) => format!("{} at {}", self.code, f),
            (None, false) => format!("{} at {}", self.code, self.toml_path),
            (None, true) => self.code.to_owned(),
        }
    }
}

impl std::fmt::Display for Diagnostic {
    /// `<code> at <file-basename>:<toml_path>: <message>`. The file is shown by basename so the
    /// text is portable across checkouts; `file` holds the absolute path.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loc = self.location();
        if self.message.is_empty() {
            f.write_str(&loc)
        } else {
            write!(f, "{loc}: {}", self.message)
        }
    }
}

impl std::error::Error for Diagnostic {}

/// Append `[i]` to a TOML path.
pub(crate) fn idx(path: &str, i: usize) -> String {
    format!("{path}[{i}]")
}

/// Join two TOML path segments with `.`, tolerating an empty prefix.
pub(crate) fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}
