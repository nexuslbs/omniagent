//! External platform plugin client.
//!
//! Manages the lifecycle of an external platform plugin subprocess:
//! spawn → initialize → outbound delivery + inbound message handling.
//!
//! Implements the [`Platform`] trait so it can be registered in the
//! [`PlatformRegistry`] just like built-in platforms.

use crate::platform::external::{
    build_deliver_request, build_initialize_request, build_react_request, parse_response,
    DeliverParams, DeliverResult, InitializeResult, PlatformPluginConfig, PluginResponse, ReactParams,
};
use crate::platform::{OutboundReceiver, Platform};
use crate::error::{Error, AppResult, ErrorContext};
use crate::err_str;
use async_trait::async_trait;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use tokio::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::select;
use tokio::sync::Notify;

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
    /// Plugin name (cached, never changes).
    name: String,
    /// Plugin configuration (mutable for hot-reload).
    config: Arc<RwLock<PlatformPluginConfig>>,
    /// The child process handle (wrapped for interior mutability).
    process: Arc<StdMutex<Option<Child>>>,
    /// Plugin name from initialize response (cached).
    plugin_name: Arc<StdMutex<Option<String>>>,
    /// Plugin capabilities from initialize response (cached).
    capabilities: Arc<StdMutex<Option<(bool, bool)>>>, // (inbound, outbound)
    /// Next request id.
    next_id: AtomicU64,
    /// Circuit breaker state.
    circuit: Arc<StdMutex<CircuitBreaker>>,
    /// Data directory for profile lookups.
    data_dir: String,
    /// Flag to signal restart to the outer loop.
    restart_flag: Arc<AtomicBool>,
    /// Flag to signal clean stop.
    stopped: Arc<AtomicBool>,
    /// Notifier for waking up the inner loop on restart/stop.
    restart_notify: Arc<Notify>,
}

impl ExternalPlatformClient {
    /// Create a new external platform client from configuration.
    pub async fn new(
        config: PlatformPluginConfig,
        data_dir: &str,
        platform_restart_signals: Arc<Mutex<HashMap<String, (Arc<AtomicBool>, Arc<Notify>)>>>,
    ) -> Self {
        let max_retries = config.max_retries;
        let name = config.name.clone();
        let restart_flag = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let restart_notify = Arc::new(Notify::new());

        // Register our restart flag and notify in the shared map so the API can signal us
        {
            let mut signals = platform_restart_signals.lock().await;
            signals.insert(name.clone(), (Arc::clone(&restart_flag), Arc::clone(&restart_notify)));
        }

        Self {
            name,
            config: Arc::new(RwLock::new(config)),
            process: Arc::new(StdMutex::new(None)),
            plugin_name: Arc::new(StdMutex::new(None)),
            capabilities: Arc::new(StdMutex::new(None)),
            next_id: AtomicU64::new(1),
            circuit: Arc::new(StdMutex::new(CircuitBreaker::new(max_retries))),
            data_dir: data_dir.to_string(),
            restart_flag,
            stopped,
            restart_notify,
        }
    }

    /// Reload plugin configuration from disk (called before respawn on restart).
    fn reload_config_from_disk(&self) {
        let configs = crate::platform::external::load_plugins_config(&self.data_dir);
        if let Some(new_config) = configs.into_iter().find(|c| c.name == self.name) {
            if let Ok(mut config_guard) = self.config.write() {
                tracing::info!(
                    "Reloaded config for platform plugin '{}' from disk",
                    self.name
                );
                *config_guard = new_config;
            }
        } else {
            tracing::warn!(
                "Platform plugin '{}' not found on disk after config update (keeping old config)",
                self.name
            );
        }
    }

    /// Request a restart — the outer loop will pick this up, kill the old
    /// subprocess, reload config from disk, and spawn a new one.
    pub fn request_restart(&self) {
        self.restart_flag.store(true, Ordering::SeqCst);
        self.restart_notify.notify_one();
    }

    /// Request a clean stop — the outer loop will exit gracefully.
    pub fn request_stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.restart_notify.notify_one();
    }

    /// Spawn the plugin subprocess and return handles.
    async fn spawn_plugin(
        &self,
        pool: &PgPool,
    ) -> AppResult<(Child, ChildStdin, tokio::process::ChildStdout)> {
        // Clone config fields while holding the read lock, then release it
        // before any async work to keep the future Send.
        let (config_name, config_command, config_args, env_map) = {
            let config = self.config.read().map_err(|_| Error::LockPoisoned)?;
            tracing::info!(
                "Spawning platform plugin '{}': {} {}",
                config.name,
                config.command,
                config.args.join(" ")
            );
            (
                config.name.clone(),
                config.command.clone(),
                config.args.clone(),
                config.env.clone(),
            )
        };

        let mut cmd = Command::new(&config_command);
        cmd.args(&config_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        // Resolve $env:, $secret:, and ${VAR} references in env values.
        // This ensures that env vars set at runtime (e.g. by the setup handler
        // via std::env::set_var) and secrets stored in the DB are picked up
        // even after a config reload.
        let mut resolved_env = env_map;
        crate::platform::external::resolve_env_refs(&mut resolved_env, pool).await;

        for (key, value) in &resolved_env {
            cmd.env(key, value);
        }

        let mut child = cmd
            .spawn()
            .ctx(format!("Failed to spawn platform plugin '{}'", config_name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| err_str!("Failed to capture stdin for platform plugin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| err_str!("Failed to capture stdout for platform plugin"))?;

        Ok((child, stdin, stdout))
    }

    /// Initialize the plugin: send initialize request and read response.
    async fn initialize(
        &self,
        stdin: &mut ChildStdin,
        stdout: &mut tokio::process::ChildStdout,
    ) -> AppResult<InitializeResult> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = build_initialize_request(id);
        let cfg_name = self.name.clone();
        tracing::debug!("Sending initialize request to '{}'", cfg_name);

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
                    serde_json::from_value(result).ctx("Failed to parse initialize result")?;
                tracing::info!(
                    "Platform plugin '{}' initialized: name={}, inbound={}, outbound={}",
                    self.name,
                    init_result.name,
                    init_result.capabilities.inbound,
                    init_result.capabilities.outbound,
                );
                *self.plugin_name.lock().map_err(|_| Error::LockPoisoned)? =
                    Some(init_result.name.clone());
                *self
                    .capabilities
                    .lock()
                    .map_err(|_| Error::LockPoisoned)? = Some((
                    init_result.capabilities.inbound,
                    init_result.capabilities.outbound,
                ));
                Ok(init_result)
            }
            PluginResponse::Error { error, .. } => Err(err_str!(
                "Plugin '{}' initialize error ({}): {}",
                self.name,
                error.code,
                error.message
            )),
        }
    }
}

#[async_trait]
impl Platform for ExternalPlatformClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self, pool: PgPool, mut receiver: OutboundReceiver) -> AppResult<()> {
        tracing::info!("Starting external platform plugin '{}'", self.name);

        // Outer loop — respawns the subprocess when a restart is requested
        loop {
            // Check if we've been asked to stop
            if self.stopped.load(Ordering::SeqCst) {
                tracing::info!(
                    "Platform plugin '{}' received stop signal, exiting",
                    self.name
                );
                return Ok(());
            }

            // Spawn the plugin subprocess
            let (child, mut stdin, stdout) = match self.spawn_plugin(&pool).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!(
                        "Failed to spawn platform plugin '{}': {:?}",
                        self.name,
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };

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
                    self.name,
                    e
                );
                // Kill child if initialization fails
                let child_to_kill = match self.process.lock() {
                    Ok(mut guard) => guard.take(),
                    Err(_) => None,
                };
                if let Some(mut child) = child_to_kill {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }

            // Send configure message with the plugin's config
            let config_id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let (config_name_for_log, configure_req) = {
                // Clone config map while holding the read lock, then drop
                // the guard before any async work to keep the future Send.
                let (name, config_map) = {
                    let config = self.config.read().map_err(|_| Error::LockPoisoned)?;
                    (config.name.clone(), config.config.clone())
                    // RwLockReadGuard dropped here
                };
                // Resolve all config refs ($env:, $secret:, ${VAR}) so the plugin
                // receives actual values (e.g. access_token), not literal
                // references like "$env:MATTERMOST_ACCESS_TOKEN" or "$secret:my_key".
                let mut resolved_config = config_map;
                crate::plugins_yaml::resolve_config_refs(&mut resolved_config, &pool).await;
                let req =
                    crate::platform::external::build_configure_request(config_id, &resolved_config);
                (name, req)
            };
            stdin.write_all(configure_req.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;

            // Read configure response
            {
                let mut reader = tokio::io::BufReader::new(&mut stdout);
                let mut line = String::new();
                reader.read_line(&mut line).await?;
                let response = crate::platform::external::parse_response(line.trim())?;
                match response {
                    crate::platform::external::PluginResponse::Success { .. } => {
                        tracing::info!(
                            "Plugin '{}' configured successfully",
                            config_name_for_log
                        );
                    }
                    crate::platform::external::PluginResponse::Error { error, .. } => {
                        return Err(err_str!(
                            "Plugin '{}' configure error ({}): {}",
                            config_name_for_log,
                            error.code,
                            error.message
                        ));
                    }
                }
            }

            let plugin_name = self
                .plugin_name
                .lock()
                .ok()
                .and_then(|p| p.clone())
                .unwrap_or_else(|| self.name.clone());

            tracing::info!("Platform plugin '{}' entering main loop", plugin_name);

            // ── Inner main loop ──────────────────────────────────────────
            let mut reader = BufReader::new(stdout);
            let mut line_buf = String::new();
            let mut next_id_val = 1u64;

            // Track the last sent deliver envelope info so we can update the message
            // with the platform's external_id and thread_id when the response arrives.
            let mut last_deliver_msg_id: Option<i64> = None;
            let mut last_deliver_thread_id: Option<i64> = None;
            let mut last_deliver_resource: Option<String> = None;
            let mut last_deliver_is_user_thread: Option<bool> = None;
            let mut last_deliver_thread_sequence: Option<i32> = None;

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
                            let circuit = self.circuit.lock().map_err(|_| Error::LockPoisoned)?;
                            if !circuit.is_allowed() {
                                tracing::warn!(
                                    "Circuit breaker open for plugin '{}', dropping envelope {}",
                                    plugin_name,
                                    envelope.message_id
                                );
                                continue;
                            }
                        }

                        // ── Reaction envelope: handle as react request instead of deliver ──
                        if envelope.msg_type == "reaction" {
                            let reactor_params = ReactParams {
                                resource_identifier: envelope.resource_identifier,
                                external_id: envelope.cause_external_id.unwrap_or_default(),
                                emoji: envelope.content,
                            };
                            let id = next_id_val;
                            next_id_val += 1;
                            let req = build_react_request(id, &reactor_params);

                            tracing::debug!(
                                "Sending react request to '{}' (emoji={})",
                                plugin_name,
                                reactor_params.emoji,
                            );

                            if let Err(e) = stdin.write_all(req.as_bytes()).await {
                                tracing::error!("Failed to write react to plugin '{}' stdin: {:?}", plugin_name, e);
                                if let Ok(mut circuit) = self.circuit.lock() {
                                    circuit.record_failure();
                                }
                                continue;
                            }
                            if let Err(e) = stdin.write_all(b"\n").await {
                                tracing::error!("Failed to write newline to plugin '{}' stdin: {:?}", plugin_name, e);
                                if let Ok(mut circuit) = self.circuit.lock() {
                                    circuit.record_failure();
                                }
                                continue;
                            }
                            if let Err(e) = stdin.flush().await {
                                tracing::error!("Failed to flush plugin '{}' stdin: {:?}", plugin_name, e);
                                if let Ok(mut circuit) = self.circuit.lock() {
                                    circuit.record_failure();
                                }
                                continue;
                            }
                            continue;
                        }

                        // Save the message_id and thread_id before sending so the
                        // response handler can update the external_id in the DB.
                        last_deliver_msg_id = Some(envelope.message_id);
                        last_deliver_thread_id = Some(envelope.thread_id);
                        last_deliver_resource = Some(envelope.resource_identifier.clone());
                        last_deliver_is_user_thread = Some(envelope.is_user_thread);
                        last_deliver_thread_sequence = Some(envelope.thread_sequence);

                        // Build deliver params from envelope
                        let params = DeliverParams {
                            resource_identifier: envelope.resource_identifier.clone(),
                            content: envelope.content.clone(),
                            msg_type: envelope.msg_type.clone(),
                            msg_subtype: envelope.msg_subtype.clone(),
                            thread_id: envelope.thread_id,
                            cause_external_id: envelope.cause_external_id.clone(),
                            cause_root_id: envelope.cause_root_id.clone(),
                            is_summary: envelope.is_summary,
                            is_user_thread: envelope.is_user_thread,
                            thread_sequence: envelope.thread_sequence,
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
                            if let Ok(mut circuit) = self.circuit.lock() {
                                circuit.record_failure();
                            }
                            continue;
                        }
                        if let Err(e) = stdin.write_all(b"\n").await {
                            tracing::error!("Failed to write newline to plugin '{}' stdin: {:?}", plugin_name, e);
                            if let Ok(mut circuit) = self.circuit.lock() {
                                circuit.record_failure();
                            }
                            continue;
                        }
                        if let Err(e) = stdin.flush().await {
                            tracing::error!("Failed to flush plugin '{}' stdin: {:?}", plugin_name, e);
                            if let Ok(mut circuit) = self.circuit.lock() {
                                circuit.record_failure();
                            }
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
                                        PluginResponse::Success { result, .. } => {
                                            if let Ok(mut circuit) = self.circuit.lock() {
                                                circuit.record_success();
                                            }
                                            // If this was a deliver response with an external_id,
                                            // save it back to the message in the database.
                                            if let Ok(dr) = serde_json::from_value::<DeliverResult>(result) {
                                                if let Some(ext_id) = dr.external_id {
                                                    if let Some(msg_id) = last_deliver_msg_id.take() {
                                                        let res = last_deliver_resource.take();
                                                        let is_user = last_deliver_is_user_thread.take().unwrap_or(false);
                                                        let seq = last_deliver_thread_sequence.take().unwrap_or(1);
                                                        let _ = last_deliver_thread_id.take();
                                                        sqlx::query(
                                                            "UPDATE messages SET external_id = $1 WHERE id = $2 AND external_id IS NULL"
                                                        )
                                                        .bind(&ext_id)
                                                        .bind(msg_id)
                                                        .execute(&pool)
                                                        .await
                                                        .map_err(|e| {
                                                            tracing::warn!(
                                                                "Failed to update message {} external_id: {:?}",
                                                                msg_id, e
                                                            );
                                                            e
                                                        }).ok();
                                                        // For system-originated threads (kanban, cron, etc.),
                                                        // immediately send a +1 reaction to acknowledge receipt
                                                        // but only for the seq-0 (first) message in the thread.
                                                        if let Some(resource) = res {
                                                            if !is_user && seq == 0 {
                                                                let _ = send_react(
                                                                    &mut stdin,
                                                                    &mut next_id_val,
                                                                    &resource,
                                                                    &ext_id,
                                                                    ":+1:",
                                                                ).await;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        PluginResponse::Error { error, .. } => {
                                            tracing::warn!(
                                                "Plugin '{}' returned error ({}): {}",
                                                plugin_name,
                                                error.code,
                                                error.message
                                            );
                                            if let Ok(mut circuit) = self.circuit.lock() {
                                                circuit.record_failure();
                                            }
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

                                                    // Handle $new or /new BEFORE channel lookup — creates a fresh channel
                                                    if inbound.text.starts_with("$new") || inbound.text.starts_with("//new") {
                                                        let reply = match crate::commands::handle_new_external(
                                                            &pool,
                                                            &plugin_name,
                                                            &inbound.resource_identifier,
                                                        ).await {
                                                            Ok(ch) => format!(
                                                                "Created new channel '{}' (id={}). You can now send messages.",
                                                                ch.name, ch.id
                                                            ),
                                                            Err(e) => format!("Error creating channel: {}", e),
                                                        };
                                                        send_external_reply(
                                                            &mut stdin,
                                                            &mut next_id_val,
                                                            &inbound,
                                                            &reply,
                                                        ).await;
                                                        continue;
                                                    }

                                                    match crate::db::types::get_channel_by_platform_and_resource(
                                                        &pool,
                                                        &plugin_name,
                                                        &inbound.resource_identifier,
                                                    ).await {
                                                        Ok(Some(channel)) => {
                                                            // Check for /model command
                                                            if inbound.text.starts_with("//model") {
                                                                let reply = handle_external_model_command(
                                                                    &pool,
                                                                    &self.data_dir,
                                                                    channel.id,
                                                                    &inbound.text,
                                                                ).await;
                                                                send_external_reply(
                                                                    &mut stdin,
                                                                    &mut next_id_val,
                                                                    &inbound,
                                                                    &reply,
                                                                ).await;
                                                                continue;
                                                            }

                                                            // Check for /channel command
                                                            if inbound.text.starts_with("//channel") {
                                                                let reply = handle_external_channel_command(
                                                                    &pool,
                                                                    &plugin_name,
                                                                    &inbound.text,
                                                                    &channel,
                                                                    &inbound.resource_identifier,
                                                                ).await;
                                                                send_external_reply(
                                                                    &mut stdin,
                                                                    &mut next_id_val,
                                                                    &inbound,
                                                                    &reply,
                                                                ).await;
                                                                continue;
                                                            }

                                                            // Check for /profile command
                                                            if inbound.text.starts_with("//profile") {
                                                                let reply = handle_external_profile_command(
                                                                    &pool,
                                                                    &inbound.text,
                                                                    &channel,
                                                                    &self.data_dir,
                                                                ).await;
                                                                send_external_reply(
                                                                    &mut stdin,
                                                                    &mut next_id_val,
                                                                    &inbound,
                                                                    &reply,
                                                                ).await;
                                                                continue;
                                                            }

                                                            if let Ok((thread, _msg)) = crate::db::types::create_thread_with_cause(
                                                                &pool,
                                                                &self.data_dir,
                                                                "user",
                                                                channel.id,
                                                                &channel.current_profile,
                                                                crate::db::types::ThreadCauseParams {
                                                                    provider: channel.current_provider.clone(),
                                                                    model: channel.current_model.clone(),
                                                                    task_id: None,
                                                                    schedule_task_id: None,
                                                                    content: inbound.text.clone(),
                                                                    external_id: Some(inbound.external_id.clone()),
                                                                    parent_external_id: inbound.metadata.get("root_id")
                                                                        .and_then(|v| v.as_str())
                                                                        .filter(|s| !s.is_empty())
                                                                        .map(|s| s.to_string()),
                                                                    metadata: {
                                                                        let mut meta = inbound.metadata.clone();
                                                                        if let Some(ref t) = channel.template {
                                                                            if !t.is_empty() {
                                                                                meta["template"] = serde_json::json!(t);
                                                                            }
                                                                        }
                                                                        meta
                                                                    },
                                                                    msg_type: "Cause".to_string(),
                                                                    msg_subtype: Some(plugin_name.clone()),
                                                                    task_planning_mode: String::new(),
                                                                },
                                                            ).await {
                                                                // success — message and thread created
                                                                // Send :o: if the thread was auto-skipped (closed channel),
                                                                // :+1: otherwise (normal acknowledgment)
                                                                let react_emoji = if thread.status == "skipped" {
                                                                    ":o:"
                                                                } else {
                                                                    ":+1:"
                                                                };
                                                                let _ = send_react(
                                                                    &mut stdin,
                                                                    &mut next_id_val,
                                                                    &inbound.resource_identifier,
                                                                    &inbound.external_id,
                                                                    react_emoji,
                                                                ).await;
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
                                        "message_deleted" => {
                                            // When a message is deleted on the platform, if it was the seq-0
                                            // (cause) message of an agent thread, stop that thread.
                                            if let Some(params) = notif.params {
                                                let resource = params.get("resource_identifier").and_then(|v| v.as_str());
                                                let ext_id = params.get("external_id").and_then(|v| v.as_str());
                                                if let (Some(resource), Some(ext_id)) = (resource, ext_id) {
                                                    tracing::info!(
                                                        "Message deleted on '{}' resource '{}': external_id={}",
                                                        plugin_name, resource, ext_id
                                                    );
                                                    match handle_message_deleted(&pool, &plugin_name, resource, ext_id).await {
                                                        Ok(Some(thread_id)) => {
                                                            tracing::info!(
                                                                "message_deleted: stopped thread {} (seq-0 was {})",
                                                                thread_id, ext_id
                                                            );
                                                        }
                                                        Ok(None) => {
                                                            tracing::debug!(
                                                                "message_deleted: no seq-0 thread for external_id={} on '{}'",
                                                                ext_id, resource
                                                            );
                                                        }
                                                        Err(e) => {
                                                            tracing::error!(
                                                                "Error handling message_deleted for {}: {:?}",
                                                                ext_id, e
                                                            );
                                                        }
                                                    }
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

                    // Restart/stop signal from the API
                    _ = self.restart_notify.notified() => {
                        if self.stopped.load(Ordering::SeqCst) {
                            tracing::info!(
                                "Platform plugin '{}' received stop signal from notifier",
                                plugin_name
                            );
                        } else {
                            tracing::info!(
                                "Platform plugin '{}' received restart signal from notifier",
                                plugin_name
                            );
                        }
                        break;
                    }
                }
            }

            // ── Inner loop ended — clean up child process ────────────────
            // stdin/stdout are dropped when they go out of scope,
            // which closes the pipes. Kill the child process.
            let child_to_kill = match self.process.lock() {
                Ok(mut guard) => guard.take(),
                Err(_) => {
                    tracing::warn!("process lock poisoned during cleanup");
                    None
                }
            };
            if let Some(mut child) = child_to_kill {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }

            // Check if we should restart (reload config from disk and respawn)
            if self.restart_flag.swap(false, Ordering::SeqCst) {
                tracing::info!(
                    "Platform plugin '{}' restart triggered — reloading config and respawning",
                    self.name
                );
                // Reload config from disk (picks up new YAML/env values)
                self.reload_config_from_disk();
                // Reset circuit breaker for fresh start
                if let Ok(mut circuit) = self.circuit.lock() {
                    *circuit = CircuitBreaker::new(
                        self.config
                            .read()
                            .map(|c| c.max_retries)
                            .unwrap_or(3),
                    );
                }
                // Reset next_id for fresh subprocess
                self.next_id.store(1, Ordering::SeqCst);
                continue;
            }

            // Normal exit (not a restart)
            tracing::info!("External platform plugin '{}' stopped", plugin_name);
            return Ok(());
        }
    }

    async fn send_response(&self, _pool: &PgPool, _message_id: i64) -> AppResult<()> {
        tracing::debug!(
            "send_response called on external platform '{}' — no-op",
            self.name
        );
        Ok(())
    }
}

/// Handle a `/model` command received via an external platform plugin.
/// Returns the reply text to deliver back to the user.
async fn handle_external_model_command(
    pool: &sqlx::PgPool,
    data_dir: &str,
    channel_id: i64,
    text: &str,
) -> String {
    let parsed = match crate::commands::parse_model_command(text) {
        Ok(cmd) => cmd,
        Err(e) => {
            return format!("Error: {}", e);
        }
    };

    match parsed.action {
        crate::commands::ModelAction::Show => {
            let channel = match crate::db::types::get_channel_by_id(pool, channel_id).await {
                Ok(Some(ch)) => ch,
                _ => return "Channel not found.".to_string(),
            };
            crate::commands::format_model_status(
                channel.current_provider.as_deref(),
                channel.current_model.as_deref(),
            )
        }
        crate::commands::ModelAction::Set { provider, model } => {
            // Validate provider if provided
            if let Some(ref p) = provider {
                if !p.is_empty() {
                    if let Err(e) = crate::commands::validate_provider(data_dir, p) {
                        return format!("Error: {}", e);
                    }
                }
            }

            let update_provider = provider.as_deref();
            let update_model = model.as_deref();
            if let Err(e) = crate::db::types::update_channel_model(
                pool,
                channel_id,
                update_provider,
                update_model,
            )
            .await
            {
                return format!("Error updating channel: {}", e);
            }

            let provider_display = update_provider.unwrap_or("(unchanged)");
            let model_display = update_model.unwrap_or("(unchanged)");
            format!(
                "Channel updated — provider: {}, model: {}",
                provider_display, model_display
            )
        }
        crate::commands::ModelAction::Reset { provider, model } => {
            let update_provider = if provider { Some("") } else { None };
            let update_model = if model { Some("") } else { None };
            if let Err(e) = crate::db::types::update_channel_model(
                pool,
                channel_id,
                update_provider,
                update_model,
            )
            .await
            {
                return format!("Error resetting channel: {}", e);
            }

            let parts = vec![
                if provider { "provider" } else { "" },
                if model { "model" } else { "" },
            ];
            let parts: Vec<&str> = parts.into_iter().filter(|s| !s.is_empty()).collect();
            format!(
                "Channel {} reset — will fall back to profile/env defaults.",
                parts.join(" and ")
            )
        }
    }
}

/// Helper: send a text reply back to the external platform for an inbound message.
async fn send_external_reply(
    stdin: &mut tokio::process::ChildStdin,
    next_id_val: &mut u64,
    inbound: &crate::platform::external::InboundMessage,
    reply: &str,
) {
    let deliver_params = crate::platform::external::DeliverParams {
        resource_identifier: inbound.resource_identifier.clone(),
        content: reply.to_string(),
        msg_type: "message".to_string(),
        msg_subtype: None,
        thread_id: 0,
        cause_external_id: Some(inbound.external_id.clone()),
        cause_root_id: None,
        is_summary: false,
        is_user_thread: false,
        thread_sequence: 0,
        };
    let id = *next_id_val;
    *next_id_val += 1;
    let req = crate::platform::external::build_deliver_request(id, &deliver_params);
    if let Err(e) = stdin.write_all(req.as_bytes()).await {
        tracing::error!("Failed to write reply to plugin: {:?}", e);
    }
    if let Err(e) = stdin.write_all(b"\n").await {
        tracing::error!("Failed to write newline: {:?}", e);
    }
}

/// Handle a `/channel` command received via an external platform plugin.
async fn handle_external_channel_command(
    pool: &sqlx::PgPool,
    plugin_name: &str,
    text: &str,
    current_channel: &crate::db::types::Channel,
    resource_identifier: &str,
) -> String {
    let parsed = match crate::commands::parse_channel_command(text) {
        Ok(cmd) => cmd,
        Err(e) => return format!("Error: {}", e),
    };

    match parsed {
        crate::commands::ChannelCommand::Show => {
            format!(
                "Current channel: {} (id={}, profile={}, platform={})",
                current_channel.name,
                current_channel.id,
                current_channel.current_profile,
                current_channel.platform.as_deref().unwrap_or("unknown"),
            )
        }
        crate::commands::ChannelCommand::List => {
            let channels = match crate::commands::handle_channel_list(pool, plugin_name).await {
                Ok(chs) => chs,
                Err(e) => return format!("Error listing channels: {}", e),
            };
            if channels.is_empty() {
                return format!("No channels for platform '{}'.", plugin_name);
            }
            let mut result = format!("Channels for platform '{}':\n", plugin_name);
            for (i, ch) in channels.iter().enumerate() {
                let current_mark = if ch.resource_identifier.as_deref() == Some(resource_identifier)
                {
                    " ← current"
                } else {
                    ""
                };
                result.push_str(&format!(
                    "  {}. {} (id={}){}\n",
                    i + 1,
                    ch.name,
                    ch.id,
                    current_mark,
                ));
            }
            result
        }
        crate::commands::ChannelCommand::Switch(ref name) => {
            let channel =
                match crate::db::types::get_channel_by_platform_name(pool, plugin_name, name).await
                {
                    Ok(Some(ch)) => ch,
                    Ok(None) => {
                        return format!(
                            "Channel '{}' not found for platform '{}'.",
                            name, plugin_name
                        )
                    }
                    Err(e) => return format!("Error looking up channel: {}", e),
                };
            // Claim the channel by updating resource_identifier
            if let Err(e) =
                crate::db::types::claim_channel_resource(pool, channel.id, resource_identifier)
                    .await
            {
                return format!("Error claiming channel: {}", e);
            }
            format!(
                "Switched to channel '{}' (id={}).",
                channel.name, channel.id
            )
        }
    }
}

/// Handle a `/profile` command received via an external platform plugin.
async fn handle_external_profile_command(
    pool: &sqlx::PgPool,
    text: &str,
    current_channel: &crate::db::types::Channel,
    data_dir: &str,
) -> String {
    let parsed = match crate::commands::parse_profile_command(text) {
        Ok(cmd) => cmd,
        Err(e) => return format!("Error: {}", e),
    };

    match parsed {
        crate::commands::ProfileCommand::Show => {
            let profile_registry = crate::profile::ProfileRegistry::new(data_dir);
            let profile_names = profile_registry.list_names();
            let mut result = format!(
                "Current profile: {}\nAvailable profiles: {}",
                current_channel.current_profile,
                profile_names.join(", "),
            );
            if let Some(profile) = profile_registry.get(&current_channel.current_profile) {
                result.push_str(&format!(
                    "\n  Provider: {}",
                    profile.provider.as_deref().unwrap_or("(not set)")
                ));
                result.push_str(&format!(
                    "\n  Model:    {}",
                    profile.model.as_deref().unwrap_or("(not set)")
                ));
            }
            result
        }
        crate::commands::ProfileCommand::Set(ref name) => {
            let profile_registry = crate::profile::ProfileRegistry::new(data_dir);
            if profile_registry.get(name).is_none() {
                return format!(
                    "Unknown profile '{}'. Available profiles: {}",
                    name,
                    profile_registry.list_names().join(", ")
                );
            }
            if let Err(e) =
                crate::commands::handle_profile_set(pool, current_channel.id, name).await
            {
                return format!("Error setting profile: {}", e);
            }
            format!("Profile set to '{}'.", name)
        }
        crate::commands::ProfileCommand::Reset => {
            if let Err(e) =
                crate::commands::handle_profile_set(pool, current_channel.id, "default").await
            {
                return format!("Error resetting profile: {}", e);
            }
            "Profile reset to 'default'.".to_string()
        }
    }
}

/// Send a reaction to a platform message via the plugin's stdin.
async fn send_react(
    stdin: &mut ChildStdin,
    next_id: &mut u64,
    resource_identifier: &str,
    external_id: &str,
    emoji: &str,
) {
    let id = *next_id;
    *next_id += 1;
    let params = ReactParams {
        resource_identifier: resource_identifier.to_string(),
        external_id: external_id.to_string(),
        emoji: emoji.to_string(),
    };
    let req = build_react_request(id, &params);
    if let Err(e) = stdin.write_all(req.as_bytes()).await {
        tracing::warn!("Failed to send react request: {:?}", e);
        return;
    }
    if let Err(e) = stdin.write_all(b"\n").await {
        tracing::warn!("Failed to send react newline: {:?}", e);
        return;
    }
    let _ = stdin.flush().await;
}

/// Handle a `message_deleted` notification from a platform plugin.
///
/// Looks for a thread whose seq-0 (cause) message has the given `external_id`
/// and belongs to a channel matching the given platform + resource_identifier.
///
/// - If the thread is `pending` or `processing`, marks it as `skipped` (terminal).
/// - If already terminal, does nothing.
///
/// Returns `Ok(Some(thread_id))` if a matching thread was found and acted upon,
/// `Ok(None)` if no matching seq-0 message exists.
async fn handle_message_deleted(
    pool: &PgPool,
    platform: &str,
    resource_identifier: &str,
    external_id: &str,
) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
    // Use sqlx::query_as directly because client.rs already uses raw sqlx::query patterns.
    #[derive(sqlx::FromRow)]
    struct ThreadStatusRow {
        id: i64,
        status: String,
    }

    let row: Option<ThreadStatusRow> = sqlx::query_as::<_, ThreadStatusRow>(
        r#"
        SELECT t.id, t.status
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        JOIN channels ch ON ch.id = t.channel_id
        WHERE m.external_id = $1
          AND m.thread_sequence = 0
          AND ch.platform = $2
          AND ch.resource_identifier = $3
        LIMIT 1
        "#,
    )
    .bind(external_id)
    .bind(platform)
    .bind(resource_identifier)
    .fetch_optional(pool)
    .await?;

    let info = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    match info.status.as_str() {
        "pending" | "processing" => {
            // Skip the thread — marks it as skipped + terminal
            sqlx::query(
                r#"
                UPDATE threads
                SET status = 'skipped',
                    ended_at = NOW(),
                    terminal = true,
                    iterations = COALESCE(
                        (SELECT MAX(iteration_number) FROM messages WHERE thread_id = $1),
                        0
                    )
                WHERE id = $1
                  AND status IN ('pending', 'processing')
                "#,
            )
            .bind(info.id)
            .execute(pool)
            .await?;

            tracing::info!(
                "message_deleted: skipped thread {} (was {}) due to seq-0 message deletion",
                info.id,
                info.status
            );
            Ok(Some(info.id))
        }
        _ => {
            tracing::debug!(
                "message_deleted: thread {} has status '{}' — no action needed (non seq-0 or already terminal)",
                info.id,
                info.status
            );
            Ok(Some(info.id))
        }
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
