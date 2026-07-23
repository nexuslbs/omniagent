//! External provider plugin subprocess client.
//!
//! Manages the lifecycle of an external provider plugin subprocess:
//! spawn → initialize → complete.
//!
//! Communicates via JSON-lines over stdin/stdout.

use crate::error::{AppResult, Error, ErrorContext};
use crate::provider::external::{
    build_complete_request, build_initialize_request, CompleteParams, CompleteResult,
    ProviderResponse,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// A provider client that communicates with an external plugin subprocess.
pub struct ExternalProviderClient {
    name: String,
    command: String,
    args: Vec<String>,
    process: Arc<StdMutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdout: Arc<Mutex<Option<ChildStdout>>>,
    next_id: AtomicU64,
    initialized: AtomicBool,
    models: Arc<StdMutex<Vec<String>>>,
}

impl ExternalProviderClient {
    pub fn new(name: &str, command: &str, args: &[String]) -> Self {
        Self {
            name: name.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            process: Arc::new(StdMutex::new(None)),
            stdin: Arc::new(Mutex::new(None)),
            stdout: Arc::new(Mutex::new(None)),
            next_id: AtomicU64::new(1),
            initialized: AtomicBool::new(false),
            models: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    /// Spawn the subprocess and perform initialize handshake.
    pub async fn start(&self) -> AppResult<()> {
        let mut child = Command::new(&self.command)
            .args(&self.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .ctx(format!("Failed to spawn provider plugin '{}'", self.name))?;

        let child_stdin = child.stdin.take().ok_or_else(|| {
            Error::Message(format!("Failed to open stdin for provider '{}'", self.name))
        })?;
        let child_stdout = child.stdout.take().ok_or_else(|| {
            Error::Message(format!(
                "Failed to open stdout for provider '{}'",
                self.name
            ))
        })?;

        // Store process, stdin, stdout handles
        {
            let mut guard = self.process.lock().expect("ExternalProvider lock poisoned");
            *guard = Some(child);
        }
        {
            let mut guard = self.stdin.lock().await;
            *guard = Some(child_stdin);
        }
        {
            let mut guard = self.stdout.lock().await;
            *guard = Some(child_stdout);
        }

        // Send initialize request via stored stdin
        let init_req = build_initialize_request(self.next_id.fetch_add(1, Ordering::SeqCst)) + "\n";
        {
            let mut guard = self.stdin.lock().await;
            if let Some(stdin) = guard.as_mut() {
                stdin.write_all(init_req.as_bytes()).await.ctx(format!(
                    "Failed to write initialize to provider '{}'",
                    self.name
                ))?;
                stdin.flush().await.ctx(format!(
                    "Failed to flush stdin for provider '{}'",
                    self.name
                ))?;
            }
        }

        // Read response via stored stdout (tokio::sync::Mutex is Send)
        let init_result = {
            let mut guard = self.stdout.lock().await;
            let stdout = guard.as_mut().ok_or_else(|| {
                Error::Message(format!("Stdout not available for provider '{}'", self.name))
            })?;
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).await.ctx(format!(
                "Failed to read initialize response from provider '{}'",
                self.name
            ))?;
            if line.trim().is_empty() {
                return Err(Error::Message(format!(
                    "Empty initialize response from provider '{}'",
                    self.name
                )));
            }
            let resp: ProviderResponse = serde_json::from_str(line.trim()).ctx(format!(
                "Failed to parse initialize response from provider '{}'",
                self.name
            ))?;
            match resp {
                ProviderResponse::Success { id: _, result } => result,
                ProviderResponse::Error { id: _, error } => {
                    return Err(Error::Message(format!(
                        "Provider '{}' initialize error (code {}): {}",
                        self.name, error.code, error.message
                    )));
                }
            }
        };

        if let Some(models) = init_result.get("models").and_then(|m| m.as_array()) {
            let model_list: Vec<String> = models
                .iter()
                .filter_map(|m| m.as_str().map(String::from))
                .collect();
            if let Ok(mut guard) = self.models.lock() {
                *guard = model_list;
            }
        }

        self.initialized.store(true, Ordering::SeqCst);
        tracing::info!("Provider plugin '{}' initialized successfully", self.name);
        Ok(())
    }

    /// Send a completion request and return the response.
    pub async fn complete(&self, params: &CompleteParams) -> AppResult<CompleteResult> {
        if !self.initialized.load(Ordering::SeqCst) {
            return Err(Error::Message(format!(
                "Provider '{}' not initialized: call start() first",
                self.name
            )));
        }

        // Write request via stored stdin (tokio::sync::Mutex: Send safe)
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = build_complete_request(id, params) + "\n";
        {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.as_mut().ok_or_else(|| {
                Error::Message(format!("Stdin not available for provider '{}'", self.name))
            })?;
            stdin
                .write_all(request.as_bytes())
                .await
                .ctx(format!("Failed to write to provider '{}'", self.name))?;
            stdin.flush().await.ctx(format!(
                "Failed to flush stdin for provider '{}'",
                self.name
            ))?;
        }

        // Read response via stored stdout
        let line = {
            let mut guard = self.stdout.lock().await;
            let stdout = guard.as_mut().ok_or_else(|| {
                Error::Message(format!("Stdout not available for provider '{}'", self.name))
            })?;
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).await.ctx(format!(
                "Failed to read completion response from provider '{}'",
                self.name
            ))?;
            line
        };

        if line.trim().is_empty() {
            return Err(Error::Message(format!(
                "Empty completion response from provider '{}'",
                self.name
            )));
        }

        let resp: ProviderResponse = serde_json::from_str(line.trim()).ctx(format!(
            "Failed to parse completion response: {}",
            line.trim()
        ))?;

        match resp {
            ProviderResponse::Success {
                id: resp_id,
                result,
            } => {
                if resp_id != id {
                    tracing::warn!(
                        "Provider '{}' returned id {} (expected {}), ignoring",
                        self.name,
                        resp_id,
                        id
                    );
                }
                let content = result
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let reasoning = result
                    .get("reasoning")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let tool_calls = result
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let usage = result
                    .get("usage")
                    .map(|u| crate::provider::external::UsageResult {
                        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                            as u32,
                        completion_tokens: u
                            .get("completion_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32,
                        cached_tokens: u
                            .get("cached_tokens")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32),
                        reasoning_tokens: u
                            .get("reasoning_tokens")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32),
                    });
                Ok(CompleteResult {
                    content,
                    reasoning,
                    tool_calls,
                    usage,
                })
            }
            ProviderResponse::Error { id: _, error } => Err(Error::Message(format!(
                "Provider '{}' completion error (code {}): {}",
                self.name, error.code, error.message
            ))),
        }
    }

    /// Get the list of models from initialization.
    pub fn models(&self) -> Vec<String> {
        self.models
            .lock()
            .expect("ModelsCache lock poisoned")
            .clone()
    }
}

impl Drop for ExternalProviderClient {
    #[allow(clippy::let_underscore_future)]
    fn drop(&mut self) {
        if let Ok(mut guard) = self.process.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
            }
        }
    }
}
