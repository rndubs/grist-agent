//! Errors of the extension API: compiled-tier registration, manifest loading, and admission.

use std::path::PathBuf;

/// Everything that can go wrong registering or loading an extension. Each variant names the
/// extension (or manifest path) so a launcher can report it without extra context.
#[derive(Debug, thiserror::Error)]
pub enum ExtError {
    /// Two compiled extensions share a name.
    #[error("duplicate extension `{0}`")]
    DuplicateExtension(String),
    /// Two tools (compiled or out-of-process) share a name.
    #[error("duplicate tool `{0}`")]
    DuplicateTool(String),
    /// Two compiled extensions provide middleware with the same name.
    #[error("duplicate middleware `{0}`")]
    DuplicateMiddleware(String),
    /// A tool name fails `kernel::tool::is_valid_tool_name`.
    #[error("invalid tool name `{0}`")]
    InvalidToolName(String),
    /// A compiled (first-party) tool uses a prefix reserved for MCP or out-of-process tools
    /// (`profile-schema.md` §11.5).
    #[error("tool `{0}` uses a reserved prefix (`mcp.` and `ext.` are not for compiled tools)")]
    ReservedPrefix(String),
    /// No compiled extension provides this middleware.
    #[error("no compiled extension provides middleware `{0}`")]
    UnknownMiddleware(String),
    /// A middleware's config was rejected by the extension that provides it.
    #[error("middleware `{name}`: {message}")]
    MiddlewareConfig {
        /// Middleware name.
        name: String,
        /// Why.
        message: String,
    },
    /// The manifest (or a schema file it names) could not be read.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest is not valid TOML, or a schema file is not valid JSON.
    #[error("{}: {message}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        message: String,
    },
    /// The manifest parsed but a key is missing, unknown, or out of range
    /// (`docs/specs/extension-manifest.md` §2).
    #[error("{}: `{key}`: {message}", path.display())]
    Invalid {
        /// The manifest.
        path: PathBuf,
        /// Dotted key, e.g. `tools[1].name`.
        key: String,
        /// Why.
        message: String,
    },
    /// Admission: a capability the extension requires is not covered by the profile's grants
    /// (`docs/specs/extension-manifest.md` §4). The extension is not loaded.
    #[error("extension `{extension}` requires `{cap}`, which the profile does not grant")]
    ExceedsGrants {
        /// Extension name.
        extension: String,
        /// The uncovered atom, canonical string form.
        cap: String,
    },
    /// Admission: the policy could not be derived for another reason (a secret-like env name in
    /// the profile's `env_allow`, for example).
    #[error("extension `{extension}`: {message}")]
    Policy {
        /// Extension name.
        extension: String,
        /// The `PolicyError`, rendered.
        message: String,
    },
    /// Two loaded manifests share an extension name.
    #[error("extension `{name}` is loaded twice ({} and {})", first.display(), second.display())]
    DuplicateManifest {
        /// Extension name.
        name: String,
        /// The first directory.
        first: PathBuf,
        /// The second directory.
        second: PathBuf,
    },
    /// The probe (`initialize` + `tools/list`) failed or disagreed with the manifest.
    #[error("extension `{extension}` probe: {message}")]
    Probe {
        /// Extension name.
        extension: String,
        /// Why.
        message: String,
    },
}
