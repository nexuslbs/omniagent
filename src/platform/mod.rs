//! Platform abstraction layer.
//!
//! Defines the [`Platform`] trait that all message platforms (Telegram, Cron,
//! etc.) must implement, along with a [`PlatformRegistry`] to manage them.
//!
//! Each registered platform gets its own outbound delivery queue (mpsc
//! channel) so that a slow or failing platform never blocks delivery to
//! healthy ones.

use async_trait::async_trait;
use sqlx::PgPool;
use std::collections::HashMap;

use crate::error::AppResult;

pub mod external;
pub mod queue;

use queue::outbound_channel;
pub use queue::{OutboundEnvelope, OutboundReceiver, OutboundSender};

/// A platform that can receive messages from external sources and send
/// responses back to them.
#[async_trait]
pub trait Platform: Send + Sync {
    fn name(&self) -> &str;

    /// Start the listener loop for this platform.
    ///
    /// `receiver` is the platform's dedicated outbound delivery queue.
    async fn start(&self, pool: PgPool, receiver: OutboundReceiver) -> AppResult<()>;

    #[allow(dead_code)]
    async fn send_response(&self, pool: &PgPool, message_id: i64) -> AppResult<()>;
}

/// Registry that holds all registered platform implementations.
///
/// Each platform gets its own mpsc delivery channel.  The senders are
/// accessible by platform name so the agent executor can enqueue messages.
pub struct PlatformRegistry {
    platforms: Vec<Box<dyn Platform>>,
    /// Per-platform outbound senders, keyed by platform name.
    senders: HashMap<String, OutboundSender>,
    /// Receivers collected during registration; consumed by `start_all()`.
    receivers: Vec<OutboundReceiver>,
}

impl PlatformRegistry {
    pub fn new() -> Self {
        Self {
            platforms: vec![],
            senders: HashMap::new(),
            receivers: vec![],
        }
    }

    /// Register a platform implementation and create its dedicated queue.
    pub fn register(&mut self, platform: Box<dyn Platform>) {
        let name = platform.name().to_string();
        let (tx, rx) = outbound_channel(1024);
        self.senders.insert(name.clone(), tx);
        self.receivers.push(rx);
        self.platforms.push(platform);
    }

    /// Get a clone of the outbound sender for a given platform.
    ///
    /// Returns `None` if the platform is not registered.
    #[allow(dead_code)]
    pub fn sender_for(&self, platform_name: &str) -> Option<OutboundSender> {
        self.senders.get(platform_name).cloned()
    }

    /// Clone all platform senders for use by the agent executor.
    pub fn clone_senders(&self) -> HashMap<String, OutboundSender> {
        self.senders.clone()
    }

    /// Start all registered platforms as concurrent tokio tasks.
    ///
    /// Consumes the registry; each platform is moved into its own task
    /// with its dedicated receiver.
    pub fn start_all(self, pool: PgPool) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        for (platform, receiver) in self.platforms.into_iter().zip(self.receivers.into_iter()) {
            let name = platform.name().to_string();
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                tracing::info!("Starting platform: {}", name);
                if let Err(e) = platform.start(pool, receiver).await {
                    tracing::error!("Platform '{}' exited with error: {:?}", name, e);
                } else {
                    tracing::warn!("Platform '{}' exited without error", name);
                }
            }));
        }

        handles
    }
}

/// Enqueue a notification envelope to a platform's outbound queue.
///
/// This sends a message directly to the platform without going through
/// the database. The receiver will handle it as a notification
/// (msg_type = "notification").
pub fn enqueue_notification(
    senders: &HashMap<String, OutboundSender>,
    platform_name: &str,
    resource_identifier: &str,
    content: &str,
) {
    let sender = match senders.get(platform_name) {
        Some(s) => s,
        None => {
            tracing::warn!(
                "enqueue_notification: no sender for platform '{}'",
                platform_name
            );
            return;
        }
    };

    let envelope = OutboundEnvelope {
        message_id: 0,
        resource_identifier: resource_identifier.to_string(),
        content: content.to_string(),
        msg_type: "notification".to_string(),
        msg_subtype: None,
        thread_id: 0,
        thread_sequence: 0,
        cause_external_id: None,
        cause_root_id: None,
        is_summary: false,
        is_user_thread: false,
    };

    if let Err(e) = sender.try_send(envelope) {
        tracing::warn!(
            "enqueue_notification: failed to send to '{}': {:?}",
            platform_name,
            e
        );
    }
}
