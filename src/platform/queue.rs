//! Outbound delivery queue — one per platform.
//!
//! After the agent saves a message to the database, it enqueues a delivery
//! envelope.  Each platform gets its own dedicated mpsc channel so that a
//! slow or failing platform never blocks delivery to healthy ones.
//!
//! The queue decouples message _production_ (the agent executor) from message
//! _delivery_ (platform adapters), so delivery never blocks the agent.

use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// OutboundEnvelope
// ---------------------------------------------------------------------------

/// A message ready for delivery to a platform.
///
/// The `platform` is implicit — it is determined by which queue the envelope
/// is sent to.  The envelope only carries the `resource_identifier` that
/// identifies the specific destination within the platform (chat_id,
/// terminal session id, etc.).
#[derive(Debug, Clone)]
pub struct OutboundEnvelope {
    /// Internal DB message id.
    pub message_id: i64,
    /// Resource identifier within the platform (chat_id, terminal session, etc.).
    pub resource_identifier: String,
    /// The message body.
    pub content: String,
    /// Message type discriminator.
    pub msg_type: String,
    /// Optional subtype (tool name, etc.).
    pub msg_subtype: Option<String>,
    /// Thread this message belongs to.
    pub thread_id: i64,
    /// Sequence within the thread.
    pub thread_sequence: i32,
    /// The cause (seq-0) message's external_id — used for threading replies.
    pub cause_external_id: Option<String>,
    /// If the cause message was itself a reply in a thread, this is the
    /// thread root's external_id (e.g. root_id in Mattermost).
    pub cause_root_id: Option<String>,
    /// Whether this is a summary message.
    pub is_summary: bool,
    /// Whether this thread was started by a user (vs cron/kanban/system).
    pub is_user_thread: bool,
}

// ---------------------------------------------------------------------------
// Channel type aliases
// ---------------------------------------------------------------------------

/// Sender half — each platform has its own.
pub type OutboundSender = mpsc::Sender<OutboundEnvelope>;

/// Receiver half — each platform gets one to consume its messages.
pub type OutboundReceiver = mpsc::Receiver<OutboundEnvelope>;

/// Create a new outbound delivery channel with the given buffer capacity.
pub fn outbound_channel(capacity: usize) -> (OutboundSender, OutboundReceiver) {
    mpsc::channel(capacity)
}
