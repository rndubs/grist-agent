//! The tool registry (§7.7): validated at construction, fixed for the kernel's lifetime.

use std::collections::HashMap;
use std::sync::Arc;

use super::KernelError;
use crate::capability::Capability;
use crate::hash::Hash;
use crate::sandbox::{SandboxLimits, SandboxPolicy, derive_policy_with};
use crate::tool::{Tool, ToolDefinition, ToolKind, is_valid_tool_name};

/// One registered tool with its derived policy.
pub(super) struct RegistryEntry {
    pub tool: Arc<dyn Tool>,
    pub kind: ToolKind,
    pub capabilities: Vec<Capability>,
    pub policy: SandboxPolicy,
    pub policy_hash: Hash,
}

/// The complete set of invocable tools.
pub(super) struct Registry {
    entries: Vec<RegistryEntry>,
    by_name: HashMap<String, usize>,
    definitions: Vec<ToolDefinition>,
    /// `derive_policy_with(grants_without_secret_atoms, grants, limits).hash()`.
    pub envelope_hash: Hash,
}

impl Registry {
    pub(super) fn build(
        tools: Vec<Arc<dyn Tool>>,
        grants: &[Capability],
        limits: &SandboxLimits,
    ) -> Result<Registry, KernelError> {
        let mut entries = Vec::with_capacity(tools.len());
        let mut by_name = HashMap::new();
        let mut definitions = Vec::with_capacity(tools.len());
        for tool in tools {
            let name = tool.name().to_owned();
            if !is_valid_tool_name(&name) {
                return Err(KernelError::InvalidToolName(name));
            }
            if by_name.contains_key(&name) {
                return Err(KernelError::DuplicateTool(name));
            }
            let capabilities = tool.capabilities();
            let policy = derive_policy_with(&capabilities, grants, limits).map_err(|source| {
                KernelError::ToolExceedsGrants {
                    tool: name.clone(),
                    source,
                }
            })?;
            let kind = tool.kind();
            if kind == ToolKind::Session && tool.session_command().is_none() {
                return Err(KernelError::MissingSessionCommand(name));
            }
            let policy_hash = policy.hash()?;
            by_name.insert(name, entries.len());
            definitions.push(tool.definition());
            entries.push(RegistryEntry {
                tool,
                kind,
                capabilities,
                policy,
                policy_hash,
            });
        }
        let envelope_caps: Vec<Capability> = grants
            .iter()
            .filter(|c| !matches!(c, Capability::Secret { .. }))
            .cloned()
            .collect();
        let envelope = derive_policy_with(&envelope_caps, grants, limits)
            .map_err(|e| KernelError::Internal(format!("envelope policy: {e}")))?;
        Ok(Registry {
            entries,
            by_name,
            definitions,
            envelope_hash: envelope.hash()?,
        })
    }

    pub(super) fn get(&self, name: &str) -> Option<&RegistryEntry> {
        self.by_name.get(name).map(|&i| &self.entries[i])
    }

    pub(super) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub(super) fn names_sorted(&self) -> Vec<String> {
        let mut names: Vec<String> = self.by_name.keys().cloned().collect();
        names.sort();
        names
    }
}
