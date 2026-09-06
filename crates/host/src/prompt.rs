//! `ask_user` transport (D17): a pluggable [`UserPrompter`] behind `Host::ask_user`.
//!
//! [`NoUserPrompter`] is the default (no client attached → `HostError::NoUser`);
//! [`ChannelPrompter`] hands each question to whoever holds the receiving end (the protocol
//! server in P1.9, tests here) and awaits the answer on a oneshot channel.

use std::fmt;

use async_trait::async_trait;
use kernel::{AskUserRequest, HostError, UserAnswer};
use tokio::sync::{mpsc, oneshot};

/// Asks the user a question. Implementations MUST NOT ask for permission on their own (D17): the
/// only questions are the ones tools pose through `ask_user`.
#[async_trait]
pub trait UserPrompter: Send + Sync + fmt::Debug {
    /// Pose `req` and wait for the answer. `Err(HostError::NoUser)` when nobody can answer.
    async fn ask(&self, req: AskUserRequest) -> Result<UserAnswer, HostError>;
}

/// No user is attached: every question fails with `HostError::NoUser`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUserPrompter;

#[async_trait]
impl UserPrompter for NoUserPrompter {
    async fn ask(&self, req: AskUserRequest) -> Result<UserAnswer, HostError> {
        Err(HostError::NoUser(format!(
            "no user is attached to this host (question `{}`)",
            req.question_id
        )))
    }
}

/// A question waiting for an answer, as received from a [`ChannelPrompter`]. Drop it without
/// answering and the asking tool sees `HostError::NoUser`.
#[derive(Debug)]
pub struct PendingQuestion {
    request: AskUserRequest,
    reply: oneshot::Sender<Option<String>>,
}

impl PendingQuestion {
    /// The question.
    pub fn request(&self) -> &AskUserRequest {
        &self.request
    }

    /// Answer with text. Returns `false` if the asker went away.
    pub fn answer(self, answer: impl Into<String>) -> bool {
        self.reply.send(Some(answer.into())).is_ok()
    }

    /// Decline (the tool sees `answer: None`). Returns `false` if the asker went away.
    pub fn decline(self) -> bool {
        self.reply.send(None).is_ok()
    }
}

/// Delivers questions to an `mpsc` receiver and awaits each answer. The answer's `question_id`
/// is always the request's: identity is carried by the per-question reply channel, so a client
/// cannot answer the wrong question.
#[derive(Clone, Debug)]
pub struct ChannelPrompter {
    tx: mpsc::Sender<PendingQuestion>,
}

impl ChannelPrompter {
    /// Create a prompter and the receiver its questions arrive on. `capacity` bounds the number of
    /// questions queued before `ask` waits.
    pub fn new(capacity: usize) -> (ChannelPrompter, mpsc::Receiver<PendingQuestion>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (ChannelPrompter { tx }, rx)
    }
}

#[async_trait]
impl UserPrompter for ChannelPrompter {
    async fn ask(&self, req: AskUserRequest) -> Result<UserAnswer, HostError> {
        let question_id = req.question_id.clone();
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(PendingQuestion {
                request: req,
                reply,
            })
            .await
            .map_err(|_| HostError::NoUser("the client is disconnected".to_owned()))?;
        let answer = rx.await.map_err(|_| {
            HostError::NoUser(format!(
                "the client dropped question `{question_id}` without answering"
            ))
        })?;
        Ok(UserAnswer {
            question_id,
            answer,
        })
    }
}
