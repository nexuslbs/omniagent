use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::{oneshot, RwLock};

pub type TaskId = String;

/// Global task registry, lazy-initialized.
pub static TASK_REGISTRY: OnceLock<Arc<TaskRegistry>> = OnceLock::new();

/// Initialize the global task registry. Called once at startup.
pub fn init_registry() -> Arc<TaskRegistry> {
    let arc = Arc::new(TaskRegistry::new());
    TASK_REGISTRY
        .set(arc.clone())
        .unwrap_or_else(|_| panic!("TASK_REGISTRY already initialized"));
    arc
}

#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: TaskId,
    pub thread_id: i64,
    pub tool_name: String,
    pub start_time: std::time::Instant,
    pub status: TaskStatus,
}

#[derive(Debug, Clone)]
pub enum TaskStatus {
    Running,
    Completed(String),
    Failed(String),
    Cancelled,
}

pub struct TaskEntry {
    pub info: TaskInfo,
    pub abort_tx: Option<oneshot::Sender<()>>,
    pub log_buffer: Arc<RwLock<Vec<String>>>,
}

pub struct TaskRegistry {
    tasks: RwLock<HashMap<TaskId, TaskEntry>>,
    next_id: RwLock<u64>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            next_id: RwLock::new(0),
        }
    }

    pub async fn register(
        &self,
        thread_id: i64,
        tool_name: &str,
    ) -> (TaskId, oneshot::Receiver<()>, Arc<RwLock<Vec<String>>>) {
        let mut id_guard = self.next_id.write().await;
        *id_guard += 1;
        let task_id = format!("task_{}_{}", thread_id, id_guard);
        drop(id_guard);

        let (abort_tx, abort_rx) = oneshot::channel();
        let log_buffer = Arc::new(RwLock::new(Vec::new()));

        let entry = TaskEntry {
            info: TaskInfo {
                id: task_id.clone(),
                thread_id,
                tool_name: tool_name.to_string(),
                start_time: std::time::Instant::now(),
                status: TaskStatus::Running,
            },
            abort_tx: Some(abort_tx),
            log_buffer: log_buffer.clone(),
        };

        self.tasks.write().await.insert(task_id.clone(), entry);

        (task_id, abort_rx, log_buffer)
    }

    pub async fn unregister(&self, id: &str) {
        self.tasks.write().await.remove(id);
    }

    pub async fn set_status(&self, id: &str, status: TaskStatus) -> bool {
        if let Some(entry) = self.tasks.write().await.get_mut(id) {
            entry.info.status = status;
            true
        } else {
            false
        }
    }

    pub async fn cancel(&self, id: &str) -> bool {
        let mut guard = self.tasks.write().await;
        if let Some(entry) = guard.get_mut(id) {
            if let Some(tx) = entry.abort_tx.take() {
                let _ = tx.send(()); // oneshot: ok if receiver dropped
            }
            entry.info.status = TaskStatus::Cancelled;
            true
        } else {
            false
        }
    }

    pub async fn cancel_all_for_thread(&self, thread_id: i64) -> usize {
        let ids: Vec<TaskId> = {
            let guard = self.tasks.read().await;
            guard
                .iter()
                .filter(|(_, e)| {
                    e.info.thread_id == thread_id && matches!(e.info.status, TaskStatus::Running)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        let mut count = 0;
        for id in &ids {
            if self.cancel(id).await {
                count += 1;
            }
        }
        count
    }

    pub async fn get_info(&self, id: &str) -> Option<TaskInfo> {
        let guard = self.tasks.read().await;
        guard.get(id).map(|e| e.info.clone())
    }

    pub async fn list_for_thread(&self, thread_id: i64) -> Vec<TaskInfo> {
        let guard = self.tasks.read().await;
        guard
            .iter()
            .filter(|(_, e)| e.info.thread_id == thread_id)
            .map(|(_, e)| e.info.clone())
            .collect()
    }

    pub async fn append_log(&self, id: &str, line: &str) {
        if let Some(entry) = self.tasks.write().await.get_mut(id) {
            let mut buf = entry.log_buffer.write().await;
            buf.push(line.to_string());
            // Keep at most 10K lines in buffer
            let overflow = buf.len().saturating_sub(10_000);
            if overflow > 0 {
                buf.drain(0..overflow);
            }
        }
    }

    pub async fn read_logs(
        &self,
        id: &str,
        cursor: Option<usize>,
        limit: Option<usize>,
    ) -> (Vec<String>, Option<usize>) {
        let guard = self.tasks.read().await;
        if let Some(entry) = guard.get(id) {
            let buf = entry.log_buffer.read().await;
            let start = cursor.unwrap_or(0);
            let max = limit.unwrap_or(100);
            if start >= buf.len() {
                return (vec![], Some(buf.len()));
            }
            let end = (start + max).min(buf.len());
            let lines = buf[start..end].to_vec();
            let next = if end >= buf.len() { None } else { Some(end) };
            (lines, next)
        } else {
            (vec![], None)
        }
    }

    pub async fn running_count(&self) -> usize {
        let guard = self.tasks.read().await;
        guard
            .values()
            .filter(|e| matches!(e.info.status, TaskStatus::Running))
            .count()
    }
}
