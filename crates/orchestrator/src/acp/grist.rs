//! The `_grist/*` extension namespace (ADR-0004 decision 2; `docs/specs/protocol.md` §3). Stock
//! ACP clients ignore all of it; a grist client subscribes to the redacted event stream, cancels
//! at `Tool`/`Task` granularity, and asks for the session's log-level status.

use agent_client_protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use kernel::{CancelScope, SessionStatus, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `_grist/subscribe`: receive `_grist/event` for `session_id`. `kinds` is a bandwidth filter, not
/// a security control (ADR-0004 resolution 4); empty or absent means every kind.
#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_grist/subscribe", response = SubscribeResponse)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeRequest {
    /// The session.
    pub session_id: String,
    /// Event kinds to receive (`event-schema.md` §2 names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
}

/// `_grist/subscribe` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonRpcResponse)]
pub struct SubscribeResponse {}

/// `_grist/unsubscribe`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_grist/unsubscribe", response = SubscribeResponse)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeRequest {
    /// The session.
    pub session_id: String,
}

/// `_grist/event`: one redacted `Event` exactly as the log holds it (envelope included).
#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcNotification)]
#[notification(method = "_grist/event")]
#[serde(rename_all = "camelCase")]
pub struct EventNotification {
    /// The session.
    pub session_id: String,
    /// The event (`{seq, ts, session_id, kind, payload}`).
    pub event: Value,
}

/// `_grist/cancel`: D15 cancellation at any scope. `session/cancel` is exactly `scope: turn`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_grist/cancel", response = CancelResponse)]
#[serde(rename_all = "camelCase")]
pub struct CancelRequest {
    /// The session.
    pub session_id: String,
    /// `{"scope":"turn"}`, `{"scope":"tool","tool_use_id":…}`, or `{"scope":"task","task_id":…}`.
    pub scope: CancelScope,
}

/// `_grist/cancel` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonRpcResponse)]
pub struct CancelResponse {}

/// `_grist/status`: the kernel's view of the session.
#[derive(Clone, Debug, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_grist/status", response = StatusResponse)]
#[serde(rename_all = "camelCase")]
pub struct StatusRequest {
    /// The session.
    pub session_id: String,
}

/// `_grist/status` result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    /// `created | running | idle | suspended | done | failed`.
    pub status: SessionStatus,
    /// `State.turn`.
    pub turn: u64,
    /// Open tasks (D1).
    pub pending_task_ids: Vec<TaskId>,
    /// Open tasks whose waker lives in the kernel process.
    pub in_process_wakers: u32,
    /// Path of the session log on the daemon's host.
    pub log_path: String,
}
