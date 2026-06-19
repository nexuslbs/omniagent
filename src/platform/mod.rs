//! Platform abstraction layer.
//!
//! Defines the [`Platform`] trait that all message platforms (Telegram, Cron,
//! etc.) must implement, along with a [`PlatformRegistry`] to manage them.

use anyhow::Result;
use async_trait::async_trait;
use sqlx::PgPool;

pub mod telegram;

/// A platform that can receive messages from external sources and send
/// responses back to them.
///
/// Each platform implementation is responsible for:
/// - Listening for incoming messages and persisting them into the DB as
///   new messages with `status = 'pending'`.
/// - Sending agent responses back to the external source when called.
#[async_trait]
pub trait Platform: Send + Sync {
    /// Human-readable name of this platform (e.g. "telegram", "cron").
    fn name(&self) -> &str;

    /// Start the listener loop for this platform.
    ///
    /// Incoming messages should be stored in the database as new messages
    /// with `status = 'pending'`, which the agent loop will pick up.
    async fn start(&self, pool: PgPool) -> Result<()>;

    /// Send a response back to the platform for a given message.
    ///
    /// The `message_id` refers to the agent's response message stored in the
    /// database. The platform should look up the message and deliver it to
    /// the appropriate external destination.
    #[expect(dead_code)]
    async fn send_response(&self, pool: &PgPool, message_id: i64) -> Result<()>;
}

/// Registry that holds all registered platform implementations.
///
/// Call [`register`](PlatformRegistry::register) for each platform, then
/// [`start_all`](PlatformRegistry::start_all) to spawn listener tasks for
/// all of them.
pub struct PlatformRegistry {
    platforms: Vec<Box<dyn Platform>>,
}

impl PlatformRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { platforms: vec![] }
    }

    /// Register a platform implementation.
    pub fn register(&mut self, platform: Box<dyn Platform>) {
        self.platforms.push(platform);
    }

    /// Start all registered platforms as concurrent tokio tasks.
    ///
    /// Consumes the registry; each platform is moved into its own task.
    /// Returns a vector of join handles for all spawned tasks.
    pub fn start_all(self, pool: PgPool) -> Vec<tokio::task::JoinHandle<()>> {
        self.platforms
            .into_iter()
            .map(|platform| {
                let pool = pool.clone();
                let name = platform.name().to_string();
                tokio::spawn(async move {
                    tracing::info!("Starting platform: {}", name);
                    if let Err(e) = platform.start(pool).await {
                        tracing::error!("Platform '{}' exited with error: {:?}", name, e);
                    } else {
                        tracing::warn!("Platform '{}' exited without error", name);
                    }
                })
            })
            .collect()
    }
}
