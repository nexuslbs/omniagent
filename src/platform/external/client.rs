//! External platform plugin client.
//!
//! Manages the lifecycle of an external platform plugin subprocess:
//! spawn → initialize → outbound delivery + inbound message handling.
//!
//! Implements the [`Platform`] trait so it can be registered in the
//! [`PlatformRegistry`] just like built-in platforms.

use crate::platform::external::{
    build_deliver_request, build_initialize_request, parse_response, DeliverParams,
    InitializeResult, PlatformPluginConfig, PluginResponse,
};
use crate::platform::{OutboundReceiver, Platform};
use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::select;

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Tracks consecutive failures for a platform plugin.
#[derive(Debug)]
struct CircuitBreaker {
    consecutive_failures: u32,
    max_retries: u32,
    open: bool,
}

impl CircuitBreaker {
    fn new(max_retries: u32) -> Self {
        Self {
            consecutive_failures: 0,
            max_retries,
            open: false,
        }
    }

    fn is_allowed(&self) -> bool {
        !self.open
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    fn record_failure(&mut self) -> bool {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.max_retries {
            self.open = true;
            tracing::warn!(
                "Circuit breaker opened for platform plugin after {} consecutive failures",
                self.consecutive_failures
            );
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// ExternalPlatformClient
// ---------------------------------------------------------------------------

/// A platform client that communicates with an external plugin subprocess.
///
/// The client spawns a subprocess, initializes it via the plugin protocol,
/// then enters a main loop that forwards outbound envelopes to the plugin
/// and handles inbound message notifications from the plugin.
pub struct ExternalPlatformClient {
    /// Plugin configuration.
    config: PlatformPluginConfig,
    /// The child process handle (wrapped for interior mutability).
    process: Arc<Mutex<Option<Child>>>,
    /// Plugin name from initialize response (cached).
    plugin_name: Arc<Mutex<Option<String>>>,
    /// Plugin capabilities from initialize response (cached).
    capabilities: Arc<Mutex<Option<(bool, bool)>>>, // (inbound, outbound)
    /// Next request id.
    next_id: AtomicU64,
    /// Circuit breaker state.
    circuit: Arc<Mutex<CircuitBreaker>>,
}

impl ExternalPlatformClient {
    /// Create a new external platform client from configuration.
    pub fn new(config: PlatformPluginConfig) -> Self {
        let max_retries = config.max_retries;
        Self {
            config,
            process: Arc::new(Mutex::new(None)),
            plugin_name: Arc::new(Mutex::new(None)),
            capabilities: Arc::new(Mutex::new(None)),
            next_id: AtomicU64::new(1),
            circuit: Arc::new(Mutex::new(CircuitBreaker::new(max_retries))),
        }
    }

    /// Spawn the plugin subprocess and return handles.
    async fn spawn_plugin(&self) -> Result<(Child, ChildStdin, tokio::process::ChildStdout)> {
        tracing::info!(
            "Spawning platform plugin '{}': {} {}",
            self.config.name,
            self.config.command,
            self.config.args.join(" ")
        );

        let mut command = Command::new(&self.config.command);
        command
            .args(&self.config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        for (key, value) in &self.config.env {
            let resolved = crate::platform::external::resolve_env_vars(value);
            command.env(key, resolved);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to spawn platform plugin '{}'", self.config.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdin for platform plugin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout for platform plugin"))?;

        Ok((child, stdin, stdout))
    }

    /// Initialize the plugin: send initialize request and read response.
    async fn initialize(
        &self,
        stdin: &mut ChildStdin,
        stdout: &mut tokio::process::ChildStdout,
    ) -> Result<InitializeResult> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_initialize_request(id);
        tracing::debug!("Sending initialize request to '{}'", self.config.name);

        // Write request
        stdin.write_all(req.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        // Read response
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let response = parse_response(line.trim())?;

        match response {
            PluginResponse::Success { result, .. } => {
                let init_result: InitializeResult =
                    serde_json::from_value(result).context("Failed to parse initialize result")?;
                tracing::info!(
                    "Platform plugin '{}' initialized: name={}, inbound={}, outbound={}",
                    self.config.name,
                    init_result.name,
                    init_result.capabilities.inbound,
                    init_result.capabilities.outbound,
                );
                *self.plugin_name.lock().unwrap() = Some(init_result.name.clone());
                *self.capabilities.lock().unwrap() = Some((
                    init_result.capabilities.inbound,
                    init_result.capabilities.outbound,
                ));
                Ok(init_result)
            }
            PluginResponse::Error { error, .. } => Err(anyhow::anyhow!(
                "Plugin '{}' initialize error ({}): {}",
                self.config.name,
                error.code,
                error.message
            )),
        }
    }
}

#[async_trait]
impl Platform for ExternalPlatformClient {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn start(&self, pool: PgPool, mut receiver: OutboundReceiver) -> Result<()> {
        tracing::info!("Starting external platform plugin '{}'", self.config.name);

        // Spawn the plugin subprocess
        let (child, mut stdin, stdout) = self.spawn_plugin().await?;

        // Store child handle for later cleanup
        {
            let mut process_guard = self.process.lock().unwrap();
            *process_guard = Some(child);
        }

        // Initialize the plugin using local handles (no locks held across await)
        let mut stdout = stdout; // make mutable
        if let Err(e) = self.initialize(&mut stdin, &mut stdout).await {
            tracing::error!(
                "Failed to initialize platform plugin '{}': {:?}",
                self.config.name,
                e
            );
            return Err(e);
        }

        let plugin_name = self
            .plugin_name
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.config.name.clone());

        tracing::info!("Platform plugin '{}' entering main loop", plugin_name);

        // Main loop: use select! to multiplex between:
        // 1. Outbound envelopes from the agent
        // 2. Lines from plugin stdout (responses + inbound notifications)
        let mut reader = BufReader::new(stdout);
        let mut line_buf = String::new();
        let mut next_id_val = 1u64;

        loop {
            line_buf.clear();

            select! {
                // Outbound envelope from the agent
                envelope = receiver.recv() => {
                    let envelope = match envelope {
                        Some(e) => e,
                        None => {
                            tracing::info!("Outbound receiver closed for '{}'", plugin_name);
                            break;
                        }
                    };

                    // Check circuit breaker
                    {
                        let circuit = self.circuit.lock().unwrap();
                        if !circuit.is_allowed() {
                            tracing::warn!(
                                "Circuit breaker open for plugin '{}', dropping envelope {}",
                                plugin_name,
                                envelope.message_id
                            );
                            continue;
                        }
                    }

                    // Build deliver params from envelope
                    let params = DeliverParams {
                        resource_identifier: envelope.resource_identifier.clone(),
                        content: envelope.content.clone(),
                        msg_type: envelope.msg_type.clone(),
                        msg_subtype: envelope.msg_subtype.clone(),
                        thread_id: envelope.thread_id,
                        cause_external_id: envelope.cause_external_id.clone(),
                        is_summary: envelope.is_summary,
                        is_user_thread: envelope.is_user_thread,
                    };

                    let id = next_id_val;
                    next_id_val += 1;
                    let req = build_deliver_request(id, &params);

                    tracing::debug!(
                        "Sending deliver request to '{}' (msg_type={}, id={})",
                        plugin_name,
                        params.msg_type,
                        envelope.message_id
                    );

                    // Write request (no lock held across await since stdin is local)
                    if let Err(e) = stdin.write_all(req.as_bytes()).await {
                        tracing::error!("Failed to write to plugin '{}' stdin: {:?}", plugin_name, e);
                        let mut circuit = self.circuit.lock().unwrap();
                        circuit.record_failure();
                        continue;
                    }
                    if let Err(e) = stdin.write_all(b"\n").await {
                        tracing::error!("Failed to write newline to plugin '{}' stdin: {:?}", plugin_name, e);
                        let mut circuit = self.circuit.lock().unwrap();
                        circuit.record_failure();
                        continue;
                    }
                    if let Err(e) = stdin.flush().await {
                        tracing::error!("Failed to flush plugin '{}' stdin: {:?}", plugin_name, e);
                        let mut circuit = self.circuit.lock().unwrap();
                        circuit.record_failure();
                        continue;
                    }
                }

                // Line from plugin stdout (response or inbound notification)
                result = reader.read_line(&mut line_buf) => {
                    match result {
                        Ok(0) => {
                            tracing::info!("Platform plugin '{}' stdout closed (EOF)", plugin_name);
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line_buf.trim().to_string();
                            if trimmed.is_empty() {
                                continue;
                            }

                            // Try to parse as a response first
                            if let Ok(response) = parse_response(&trimmed) {
                                match response {
                                    PluginResponse::Success { .. } => {
                                        let mut circuit = self.circuit.lock().unwrap();
                                        circuit.record_success();
                                    }
                                    PluginResponse::Error { error, .. } => {
                                        tracing::warn!(
                                            "Plugin '{}' returned error ({}): {}",
                                            plugin_name,
                                            error.code,
                                            error.message
                                        );
                                        let mut circuit = self.circuit.lock().unwrap();
                                        circuit.record_failure();
                                    }
                                }
                                continue;
                            }

                            // Try to parse as a notification (no id field)
                            if let Ok(notif) = serde_json::from_str::<crate::platform::external::PluginNotification>(&trimmed) {
                                match notif.method.as_str() {
                                    "inbound_message" => {
                                        if let Some(params) = notif.params {
                                            if let Ok(inbound) = serde_json::from_value::<crate::platform::external::InboundMessage>(params) {
                                                tracing::info!(
                                                    "Received inbound message from '{}' via '{}': {}",
                                                    inbound.resource_identifier,
                                                    plugin_name,
                                                    inbound.text.chars().take(50).collect::<String>()
                                                );

                                                match crate::db::types::get_channel_by_platform_and_resource(
                                                    &pool,
                                                    &plugin_name,
                                                    &inbound.resource_identifier,
                                                ).await {
                                                    Ok(Some(channel)) => {
                                                        if let Ok(thread) = crate::db::types::create_thread(
                                                            &pool,
                                                            "user",
                                                            channel.id,
                                                            &channel.current_profile,
                                                            channel.current_provider.as_deref(),
                                                            channel.current_model.as_deref(),
                                                            None,
                                                            None,
                                                        ).await {
                                                            let msg = crate::models::MessageNew {
                                                                thread_id: thread.id,
                                                                role: "cause".to_string(),
                                                                content: inbound.text,
                                                                thread_sequence: 0,
                                                                external_id: Some(inbound.external_id),
                                                                metadata: inbound.metadata,
                                                                embedding: None,
                                                                summary_text: None,
                                                                is_summary: false,
                                                                msg_type: "message".to_string(),
                                                                msg_subtype: None,
                                                                processing_time_ms: None,
                                                                token_usage: None,
                                                            };
                                                            if let Err(e) = crate::db::types::create_cause_and_set_pending(&pool, &msg).await {
                                                                tracing::error!(
                                                                    "Failed to insert inbound message from '{}': {:?}",
                                                                    plugin_name,
                                                                    e
                                                                );
                                                            }
                                                        } else {
                                                            tracing::error!("Failed to create thread for inbound message from '{}'", plugin_name);
                                                        }
                                                    }
                                                    Ok(None) => {
                                                        tracing::warn!(
                                                            "No channel for platform '{}', resource '{}'",
                                                            plugin_name,
                                                            inbound.resource_identifier
                                                        );
                                                    }
                                                    Err(e) => {
                                                        tracing::error!(
                                                            "Error looking up channel: {:?}",
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "notify" => {
                                        // Just log notifications for now
                                        if let Some(params) = notif.params {
                                            if let Ok(notify) = serde_json::from_value::<crate::platform::external::NotifyMessage>(params) {
                                                tracing::info!(
                                                    "Notification from '{}' to '{}': {}",
                                                    plugin_name,
                                                    notify.resource_identifier,
                                                    notify.content.chars().take(50).collect::<String>()
                                                );
                                            }
                                        }
                                    }
                                    _ => {
                                        tracing::debug!(
                                            "Unknown notification from '{}': {}",
                                            plugin_name,
                                            notif.method
                                        );
                                    }
                                }
                            } else {
                                tracing::debug!(
                                    "Unrecognized output from '{}' (first 100 chars): {}",
                                    plugin_name,
                                    trimmed.chars().take(100).collect::<String>()
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Error reading from plugin '{}' stdout: {:?}", plugin_name, e);
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup: stdin/stdout are dropped when they go out of scope,
        // which closes the pipes. Kill the child process (outside the lock).
        let child_to_kill = {
            let mut process_guard = self.process.lock().unwrap();
            process_guard.take()
        };
        if let Some(mut child) = child_to_kill {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }

        tracing::info!("External platform plugin '{}' stopped", plugin_name);
        Ok(())
    }

    async fn send_response(&self, _pool: &PgPool, _message_id: i64) -> Result<()> {
        tracing::debug!(
            "send_response called on external platform '{}' — no-op",
            self.config.name
        );
        Ok(())
    }
}

impl Drop for ExternalPlatformClient {
    fn drop(&mut self) {
        if let Ok(mut process_guard) = self.process.lock() {
            if let Some(mut child) = process_guard.take() {
                let _ = child.start_kill();
            }
        }
    }
}
