//! System prompt assembly (`profile-schema.md` §8, D7): blocks in fixed order, headers for
//! blocks 3–5, trimming, empty blocks omitted, per-block hashes over the text *after* header
//! prefixing.

use kernel::{PromptBlock, PromptBlockKind};

/// Header line of block 3.
pub const AGENTS_MD_HEADER: &str = "# Project instructions (AGENTS.md)";
/// Header line of block 4.
pub const SKILLS_HEADER: &str = "# Active skills";
/// Header line of block 5.
pub const NOTEBOOK_HEADER: &str = "# Notebook";
/// Separator between non-empty blocks.
pub const BLOCK_SEPARATOR: &str = "\n\n";

/// The per-session sources of blocks 1–3 and 5 (block 4, skills, is per turn and owned by P2.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptSources {
    /// Block 1: `[prompt]` text and its source name (the model id).
    pub model: Option<(String, String)>,
    /// Block 2: `[agent].role_prompt` text and its source name (the agent name).
    pub role: Option<(String, String)>,
    /// Block 3: `AGENTS.md` text and its path.
    pub agents_md: Option<(String, String)>,
    /// Block 5: notebook text and its path (only on resume with `inject_on_resume`).
    pub notebook: Option<(String, String)>,
}

/// Trim leading and trailing newlines (§8.2).
pub fn trim_block(text: &str) -> &str {
    text.trim_matches(|c| c == '\n' || c == '\r')
}

/// Build one block: trim, prefix the header (if any) followed by one blank line, hash. Returns
/// `None` for an empty block.
pub fn block(
    kind: PromptBlockKind,
    name: &str,
    header: Option<&str>,
    text: &str,
) -> Option<PromptBlock> {
    let body = trim_block(text);
    if body.is_empty() {
        return None;
    }
    let text = match header {
        Some(h) => format!("{h}\n\n{body}"),
        None => body.to_owned(),
    };
    Some(PromptBlock::new(kind, name, text))
}

/// Assemble blocks 1, 2, 3, 5 in D7 order.
pub fn assemble(sources: &PromptSources) -> Vec<PromptBlock> {
    let mut out = Vec::new();
    if let Some((text, name)) = &sources.model {
        out.extend(block(PromptBlockKind::Model, name, None, text));
    }
    if let Some((text, name)) = &sources.role {
        out.extend(block(PromptBlockKind::Role, name, None, text));
    }
    if let Some((text, name)) = &sources.agents_md {
        out.extend(block(
            PromptBlockKind::AgentsMd,
            name,
            Some(AGENTS_MD_HEADER),
            text,
        ));
    }
    if let Some((text, name)) = &sources.notebook {
        out.extend(block(
            PromptBlockKind::Notebook,
            name,
            Some(NOTEBOOK_HEADER),
            text,
        ));
    }
    out
}

/// Join blocks with `"\n\n"`; `None` when there are no blocks (no system message is sent).
pub fn join(blocks: &[PromptBlock]) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }
    Some(
        blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join(BLOCK_SEPARATOR),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::Hash;

    #[test]
    fn blocks_are_trimmed_headed_hashed_and_empty_ones_omitted() {
        let s = PromptSources {
            model: Some(("\n\nmodel text\n".into(), "m".into())),
            role: Some(("\n".into(), "r".into())),
            agents_md: Some(("do this\n\n".into(), "/w/AGENTS.md".into())),
            notebook: None,
        };
        let blocks = assemble(&s);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, PromptBlockKind::Model);
        assert_eq!(blocks[0].text, "model text");
        assert_eq!(blocks[1].kind, PromptBlockKind::AgentsMd);
        assert_eq!(
            blocks[1].text,
            "# Project instructions (AGENTS.md)\n\ndo this"
        );
        assert_eq!(blocks[1].hash, Hash::of_bytes(blocks[1].text.as_bytes()));
        assert_eq!(
            join(&blocks).unwrap(),
            "model text\n\n# Project instructions (AGENTS.md)\n\ndo this"
        );
        assert_eq!(join(&[]), None);
    }
}
