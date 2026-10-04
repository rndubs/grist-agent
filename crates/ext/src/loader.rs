//! Loading and admitting out-of-process extensions (`docs/specs/extension-manifest.md` §4).
//!
//! [`load_all`] reads each directory's manifest and [`admit`]s it against the profile: every
//! capability the manifest requires MUST be covered by the profile's resolved grants, checked
//! with the same `kernel::derive_policy_with` the kernel runs at construction (§7.7). An
//! extension that asks for more is refused as a whole with [`ExtError::ExceedsGrants`]; nothing
//! of it is exposed. Admitted tools join the session as `ext.<extension>.<tool>`, and their
//! derived `tool:` atoms join the grants (`profile-schema.md` §3.3).
//!
//! [`probe`] is optional verification: it starts the process under its derived policy, sends
//! `initialize` and `tools/list`, and checks the answer against the manifest.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use kernel::cancel::CancellationToken;
use kernel::capability::Capability;
use kernel::host::Command;
use kernel::sandbox::{
    PolicyError, RpcRequest, RpcResponse, SandboxBackend, SandboxLimits, SandboxPolicy,
    derive_policy_with,
};
use kernel::tool::Tool;
use serde_json::{Value, json};

use crate::error::ExtError;
use crate::manifest::{Manifest, Placeholders};
use crate::process::ProcessTool;

/// The derived policy an admitted extension runs under; per-tool policies equal it, since every
/// tool of an extension declares the extension's capabilities.
pub fn policy_for(
    manifest: &Manifest,
    grants: &[Capability],
    limits: &SandboxLimits,
) -> Result<SandboxPolicy, ExtError> {
    derive_policy_with(&manifest.capabilities, grants, limits).map_err(|e| match e {
        PolicyError::Exceeds { cap } => ExtError::ExceedsGrants {
            extension: manifest.name.clone(),
            cap,
        },
        other => ExtError::Policy {
            extension: manifest.name.clone(),
            message: other.to_string(),
        },
    })
}

/// Check `manifest` against the profile's `grants` and `limits` and build its tools.
pub fn admit(
    manifest: &Manifest,
    grants: &[Capability],
    limits: &SandboxLimits,
) -> Result<Vec<Arc<ProcessTool>>, ExtError> {
    policy_for(manifest, grants, limits)?;
    Ok(manifest
        .tools
        .iter()
        .map(|spec| Arc::new(ProcessTool::new(manifest, spec)))
        .collect())
}

/// Every extension of a session, admitted.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    /// The manifests, in the order their directories were given.
    pub manifests: Vec<Manifest>,
    /// Their tools, in manifest order.
    pub tools: Vec<Arc<ProcessTool>>,
}

impl Loaded {
    /// The tools as `kernel::Tool` objects for `KernelConfig.tools`.
    pub fn kernel_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .map(|t| t.clone() as Arc<dyn Tool>)
            .collect()
    }

    /// The derived `tool:<name>` atom of every admitted tool, to add to the session grants.
    pub fn tool_atoms(&self) -> Vec<Capability> {
        self.tools
            .iter()
            .map(|t| Capability::Tool {
                name: t.name().to_owned(),
            })
            .collect()
    }
}

/// Load and admit the extensions in `dirs` (a profile's expanded `extensions.paths`). Fails on the
/// first unreadable or invalid manifest, the first extension that exceeds the grants, or two
/// directories declaring the same extension name.
pub fn load_all(
    dirs: &[PathBuf],
    ph: &Placeholders<'_>,
    grants: &[Capability],
    limits: &SandboxLimits,
) -> Result<Loaded, ExtError> {
    let mut loaded = Loaded::default();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs {
        let manifest = Manifest::load(dir, ph)?;
        if let Some(first) = seen.get(&manifest.name) {
            return Err(ExtError::DuplicateManifest {
                name: manifest.name.clone(),
                first: first.clone(),
                second: manifest.dir.clone(),
            });
        }
        seen.insert(manifest.name.clone(), manifest.dir.clone());
        loaded.tools.extend(admit(&manifest, grants, limits)?);
        loaded.manifests.push(manifest);
    }
    Ok(loaded)
}

/// What a probe learned.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeReport {
    /// `initialize` result `serverInfo`, if any.
    pub server_info: Option<Value>,
    /// Tool names from `tools/list`, in the order returned.
    pub tools: Vec<String>,
}

/// Start the extension under its derived policy (a session launch, whatever its kind), send
/// `initialize` and `tools/list`, terminate it, and check that every manifest tool is listed.
pub async fn probe(
    manifest: &Manifest,
    backend: &dyn SandboxBackend,
    grants: &[Capability],
    limits: &SandboxLimits,
) -> Result<ProbeReport, ExtError> {
    let fail = |message: String| ExtError::Probe {
        extension: manifest.name.clone(),
        message,
    };
    let policy = policy_for(manifest, grants, limits)?;
    let cmd = Command {
        program: manifest.command[0].clone(),
        args: manifest.command[1..].to_vec(),
        cwd: Some(manifest.cwd.clone()),
        env: Default::default(),
        stdin: None,
    };
    let session = backend
        .launch_session(&policy, cmd)
        .await
        .map_err(|e| fail(e.to_string()))?;
    let call = |method: &'static str, params: Value| {
        let req = RpcRequest {
            method: method.to_owned(),
            params,
        };
        let session = &session;
        async move {
            match session.call(req, CancellationToken::new()).await {
                Ok(RpcResponse::Result(v)) => Ok(v),
                Ok(RpcResponse::Error { code, message, .. }) => {
                    Err(format!("`{method}` → error {code}: {message}"))
                }
                Err(e) => Err(format!("`{method}`: {e}")),
            }
        }
    };
    let result = async {
        let init = call(
            "initialize",
            json!({
                "protocolVersion": crate::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "grist", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        let list = call("tools/list", json!({})).await?;
        let tools: Vec<String> = list
            .get("tools")
            .and_then(Value::as_array)
            .ok_or("`tools/list` result has no `tools` array")?
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        Ok::<_, String>(ProbeReport {
            server_info: init.get("serverInfo").cloned(),
            tools,
        })
    }
    .await;
    let _ = session.terminate().await;
    let report = result.map_err(fail)?;
    if let Some(missing) = manifest
        .tools
        .iter()
        .find(|t| !report.tools.contains(&t.name))
    {
        return Err(fail(format!(
            "tool `{}` is in the manifest but not in `tools/list` ({:?})",
            missing.name, report.tools
        )));
    }
    Ok(report)
}
