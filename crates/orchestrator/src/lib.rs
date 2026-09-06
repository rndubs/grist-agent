//! `orchestrator`: placement, wakers, fleet, agent-to-agent messaging, trust tiers (P3), seeded in
//! P1.9 by the launcher and the ACP protocol server (ADR-0004, D4, D11).

pub mod acp;
pub mod launcher;

pub use launcher::{
    LaunchError, LaunchOptions, Launched, SessionRecord, create_session, resume_session,
};
