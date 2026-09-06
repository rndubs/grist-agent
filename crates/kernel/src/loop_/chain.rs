//! Middleware chain resolution (§7.6, D7).

use std::collections::HashSet;

use super::KernelError;
use crate::event::{ChainEntry, MiddlewareChainResolvedPayload};
use crate::hash::Hash;
use crate::middleware::{MiddlewareEntry, TOOL_CALL_PARSER_NAME, TOOL_CALL_PARSER_PRIORITY};

/// Stable-sort by priority, enforce the parser slot and name uniqueness, build the
/// `middleware_chain_resolved` payload.
pub(super) fn resolve(
    mut entries: Vec<MiddlewareEntry>,
) -> Result<(Vec<MiddlewareEntry>, MiddlewareChainResolvedPayload), KernelError> {
    let mut names = HashSet::new();
    for e in &entries {
        if !names.insert(e.name.clone()) {
            return Err(KernelError::DuplicateMiddleware(e.name.clone()));
        }
        let is_parser = e.name == TOOL_CALL_PARSER_NAME;
        if (is_parser && e.priority != TOOL_CALL_PARSER_PRIORITY)
            || (!is_parser && e.priority <= TOOL_CALL_PARSER_PRIORITY)
        {
            return Err(KernelError::ReservedPriority(e.name.clone()));
        }
    }
    // `sort_by_key` is stable: equal priorities keep insertion order.
    entries.sort_by_key(|e| e.priority);
    let chain: Vec<ChainEntry> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| ChainEntry {
            index: i as u32,
            name: e.name.clone(),
            priority: e.priority,
            source: e.source,
            config_hash: e.config_hash.clone(),
        })
        .collect();
    let chain_hash = Hash::of_canonical_json(&chain)?;
    Ok((
        entries,
        MiddlewareChainResolvedPayload { chain, chain_hash },
    ))
}
