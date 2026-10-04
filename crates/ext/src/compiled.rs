//! The compiled tier (D8, ADR-0001): Rust middleware and first-party tools.
//!
//! A compiled extension implements [`Extension`]: it names itself, constructs its tools for a
//! session ([`SessionSetup`]), and builds the middleware it provides by name from the config
//! table the profile resolved. A build collects its extensions in an [`ExtensionSet`], which is
//! the one place a launcher gets compiled tools, their declarations for the profiles validator,
//! and middleware from. The agent never authors anything in this tier; agent-authorable tools are
//! out-of-process ([`crate::manifest`], [`crate::loader`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kernel::cancel::CancellationToken;
use kernel::capability::Capability;
use kernel::hash::Hash;
use kernel::host::{Command, ProcessOutput};
use kernel::middleware::{Middleware, MiddlewareEntry, MiddlewareSource};
use kernel::sandbox::{SandboxBackend, SandboxError, SandboxPolicy, SessionProcess};
use kernel::tool::{Tool, ToolKind, is_valid_tool_name};
use serde_json::Value;

use crate::error::ExtError;

/// Prefixes reserved for MCP tools and out-of-process extension tools (`profile-schema.md`
/// §11.5). A compiled tool may not start with either.
pub const RESERVED_TOOL_PREFIXES: &[&str] = &["mcp.", "ext."];

/// What a compiled extension is constructed against for one session.
#[derive(Clone)]
pub struct SessionSetup {
    /// Absolute session working directory; tools declare concrete capabilities from it.
    pub workdir: PathBuf,
    /// The backend the kernel will also place in `ToolContext` (tools that start background work,
    /// like `run_script`, need an owned handle).
    pub sandbox: Arc<dyn SandboxBackend>,
}

/// A compiled extension: first-party tools and/or middleware.
///
/// Implementations are stateless descriptions; every method may be called more than once (for the
/// validator's declarations and again for the session).
pub trait Extension: Send + Sync {
    /// Unique within an [`ExtensionSet`]; `[a-z][a-z0-9_-]*` by convention.
    fn name(&self) -> &str;

    /// The tools this extension contributes to a session. Default: none.
    fn tools(&self, setup: &SessionSetup) -> Vec<Arc<dyn Tool>> {
        let _ = setup;
        Vec::new()
    }

    /// Names of the middleware this extension can build. Default: none.
    fn middleware_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Build the middleware `name` from its resolved config table (`{}` when the profile gave
    /// none). Called only for names in [`Extension::middleware_names`].
    fn middleware(&self, name: &str, config: &Value) -> Result<Arc<dyn Middleware>, ExtError> {
        let _ = config;
        Err(ExtError::UnknownMiddleware(name.to_owned()))
    }
}

/// `(name, kind, capabilities)` of one tool, as the profiles validator consumes it.
pub type ToolDeclTuple = (String, ToolKind, Vec<Capability>);

/// The compiled extensions of a build.
#[derive(Clone, Default)]
pub struct ExtensionSet {
    extensions: Vec<Arc<dyn Extension>>,
}

impl ExtensionSet {
    /// An empty set.
    pub fn new() -> ExtensionSet {
        ExtensionSet::default()
    }

    /// Add an extension. Fails on a duplicate extension name or a middleware name already
    /// provided by another extension.
    pub fn add(&mut self, ext: Arc<dyn Extension>) -> Result<(), ExtError> {
        if self.extensions.iter().any(|e| e.name() == ext.name()) {
            return Err(ExtError::DuplicateExtension(ext.name().to_owned()));
        }
        let have = self.middleware_names();
        if let Some(dup) = ext
            .middleware_names()
            .into_iter()
            .find(|n| have.contains(n))
        {
            return Err(ExtError::DuplicateMiddleware(dup));
        }
        self.extensions.push(ext);
        Ok(())
    }

    /// Builder form of [`ExtensionSet::add`].
    pub fn with(mut self, ext: Arc<dyn Extension>) -> Result<ExtensionSet, ExtError> {
        self.add(ext)?;
        Ok(self)
    }

    /// Extension names, in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.extensions.iter().map(|e| e.name()).collect()
    }

    /// Every compiled tool for a session, in registration order. Validates what the kernel would
    /// (name grammar, uniqueness) plus the reserved-prefix rule, so a bad extension fails here
    /// with its name rather than at `Kernel::create`.
    pub fn tools(&self, setup: &SessionSetup) -> Result<Vec<Arc<dyn Tool>>, ExtError> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for ext in &self.extensions {
            for tool in ext.tools(setup) {
                check_compiled_tool_name(tool.name())?;
                if !seen.insert(tool.name().to_owned()) {
                    return Err(ExtError::DuplicateTool(tool.name().to_owned()));
                }
                out.push(tool);
            }
        }
        Ok(out)
    }

    /// Declarations for the profiles validator (`profile-schema.md` §3.3), without a real backend:
    /// tools are constructed against a backend that refuses every launch.
    pub fn tool_decls(&self, workdir: &Path) -> Result<Vec<ToolDeclTuple>, ExtError> {
        let setup = SessionSetup {
            workdir: workdir.to_path_buf(),
            sandbox: Arc::new(DeclOnlyBackend),
        };
        Ok(self
            .tools(&setup)?
            .iter()
            .map(|t| (t.name().to_owned(), t.kind(), t.capabilities()))
            .collect())
    }

    /// Every middleware name the set can build.
    pub fn middleware_names(&self) -> BTreeSet<String> {
        self.extensions
            .iter()
            .flat_map(|e| e.middleware_names())
            .collect()
    }

    /// Build one resolved middleware entry for `KernelConfig.middleware`.
    pub fn middleware_entry(
        &self,
        name: &str,
        priority: i32,
        source: MiddlewareSource,
        config: &Value,
        config_hash: Option<Hash>,
    ) -> Result<MiddlewareEntry, ExtError> {
        let ext = self
            .extensions
            .iter()
            .find(|e| e.middleware_names().iter().any(|n| n == name))
            .ok_or_else(|| ExtError::UnknownMiddleware(name.to_owned()))?;
        Ok(MiddlewareEntry {
            name: name.to_owned(),
            priority,
            source,
            config_hash,
            middleware: ext.middleware(name, config)?,
        })
    }
}

/// Grammar plus the §11.5 reserved prefixes.
fn check_compiled_tool_name(name: &str) -> Result<(), ExtError> {
    if !is_valid_tool_name(name) {
        return Err(ExtError::InvalidToolName(name.to_owned()));
    }
    if RESERVED_TOOL_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(ExtError::ReservedPrefix(name.to_owned()));
    }
    Ok(())
}

/// The six base tools (`read`, `write`, `edit`, `bash`, `run_script`, `python`; P1.7) as a
/// compiled extension. Their implementations live in `sandbox::tools`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BaseTools;

impl Extension for BaseTools {
    fn name(&self) -> &str {
        "base"
    }

    fn tools(&self, setup: &SessionSetup) -> Vec<Arc<dyn Tool>> {
        sandbox::base_tools(&setup.workdir, setup.sandbox.clone())
    }
}

/// A backend used only to construct tools for their declarations; every launch fails.
struct DeclOnlyBackend;

#[async_trait]
impl SandboxBackend for DeclOnlyBackend {
    fn name(&self) -> &'static str {
        "decl-only"
    }

    async fn launch_stateless(
        &self,
        _policy: &SandboxPolicy,
        _cmd: Command,
        _cancel: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError> {
        Err(SandboxError::Launch(
            "declaration-only backend cannot launch".to_owned(),
        ))
    }

    async fn launch_session(
        &self,
        _policy: &SandboxPolicy,
        _cmd: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError> {
        Err(SandboxError::Launch(
            "declaration-only backend cannot launch".to_owned(),
        ))
    }
}
