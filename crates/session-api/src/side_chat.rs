//! Root-control traffic and isolated Side Chat lifecycle traffic.
use crate::{SessionEvent, UserPrompt};
use zevria_foundation::{SideChatDescriptor, SideChatId, SideChatStatus};

#[derive(Debug, Clone, PartialEq)]
pub enum SideChatCommand {
    Create {
        request_id: String,
        initial_prompt: Option<UserPrompt>,
        quoted_excerpt: Option<String>,
    },
    Submit {
        id: SideChatId,
        text: UserPrompt,
    },
    Cancel {
        id: SideChatId,
    },
    /// Unload the engine. The thread and its durable history remain available.
    Close {
        id: SideChatId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SideChatEvent {
    Created {
        request_id: String,
        descriptor: Box<SideChatDescriptor>,
        quoted_excerpt: Option<String>,
        awaiting_initial_turn: bool,
    },
    CreateFailed {
        request_id: String,
        error: String,
    },
    Session {
        id: SideChatId,
        event: Box<SessionEvent>,
    },
    Status {
        id: SideChatId,
        status: SideChatStatus,
    },
    /// A new submission was refused; an already accepted turn is unaffected.
    SubmitRejected {
        id: SideChatId,
        error: String,
    },
    Failed {
        id: SideChatId,
        error: String,
    },
    Unloaded {
        id: SideChatId,
    },
}
