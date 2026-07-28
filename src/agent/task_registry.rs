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

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_get_info() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "test_tool").await;
        let info = registry.get_info(&task_id).await.unwrap();
        assert_eq!(info.thread_id, 42);
        assert_eq!(info.tool_name, "test_tool");
        assert!(matches!(info.status, TaskStatus::Running));
    }

    #[tokio::test]
    async fn test_set_status_completed() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;
        let result = registry.set_status(&task_id, TaskStatus::Completed("done".to_string())).await;
        assert!(result);
        let info = registry.get_info(&task_id).await.unwrap();
        assert!(matches!(&info.status, TaskStatus::Completed(msg) if msg == "done"));
    }

    #[tokio::test]
    async fn test_cancel_task() {
        let registry = TaskRegistry::new();
        let (task_id, rx, _log) = registry.register(42, "tool").await;
        let result = registry.cancel(&task_id).await;
        assert!(result);
        let info = registry.get_info(&task_id).await.unwrap();
        assert!(matches!(info.status, TaskStatus::Cancelled));
        // abort_tx was fired, so rx should be resolved
        let cancelled = rx.await.is_ok();
        assert!(cancelled);
    }

    #[tokio::test]
    async fn test_cancel_non_existent() {
        let registry = TaskRegistry::new();
        let result = registry.cancel("nonexistent").await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_cancel_all_for_thread() {
        let registry = TaskRegistry::new();
        // Register two tasks for thread 1 and one for thread 2
        let (id1, _rx1, _log1) = registry.register(1, "tool_a").await;
        let (id2, _rx2, _log2) = registry.register(1, "tool_b").await;
        let (id3, _rx3, _log3) = registry.register(2, "tool_c").await;

        let count = registry.cancel_all_for_thread(1).await;
        assert_eq!(count, 2);

        // id1 and id2 should be cancelled
        assert!(matches!(
            registry.get_info(&id1).await.unwrap().status,
            TaskStatus::Cancelled
        ));
        assert!(matches!(
            registry.get_info(&id2).await.unwrap().status,
            TaskStatus::Cancelled
        ));
        // id3 should still be Running
        assert!(matches!(
            registry.get_info(&id3).await.unwrap().status,
            TaskStatus::Running
        ));

        // Second call should cancel 0 (all already cancelled)
        let count2 = registry.cancel_all_for_thread(1).await;
        assert_eq!(count2, 0);
    }

    #[tokio::test]
    async fn test_unregister_task() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;
        // Info exists before unregister
        assert!(registry.get_info(&task_id).await.is_some());
        registry.unregister(&task_id).await;
        // Info is gone after unregister
        assert!(registry.get_info(&task_id).await.is_none());
    }

    #[tokio::test]
    async fn test_append_and_read_logs() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;

        registry.append_log(&task_id, "line1").await;
        registry.append_log(&task_id, "line2").await;
        registry.append_log(&task_id, "line3").await;

        let (lines, next) = registry.read_logs(&task_id, None, None).await;
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
        assert_eq!(next, None); // no more lines
    }

    #[tokio::test]
    async fn test_read_logs_with_cursor_and_limit() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;

        for i in 0..10 {
            registry.append_log(&task_id, &format!("line{}", i)).await;
        }

        // Read first 3 lines
        let (lines, next) = registry.read_logs(&task_id, Some(0), Some(3)).await;
        assert_eq!(lines, vec!["line0", "line1", "line2"]);
        assert_eq!(next, Some(3));

        // Read next 3 lines
        let (lines, next) = registry.read_logs(&task_id, Some(3), Some(3)).await;
        assert_eq!(lines, vec!["line3", "line4", "line5"]);
        assert_eq!(next, Some(6));

        // Read remaining
        let (lines, next) = registry.read_logs(&task_id, Some(6), Some(10)).await;
        assert_eq!(lines, vec!["line6", "line7", "line8", "line9"]);
        assert_eq!(next, None);
    }

    #[tokio::test]
    async fn test_read_logs_start_past_end() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;
        registry.append_log(&task_id, "line1").await;

        let (lines, next) = registry.read_logs(&task_id, Some(100), None).await;
        assert!(lines.is_empty());
        assert_eq!(next, Some(1));
    }

    #[tokio::test]
    async fn test_read_logs_non_existent_task() {
        let registry = TaskRegistry::new();
        let (lines, next) = registry.read_logs("nonexistent", None, None).await;
        assert!(lines.is_empty());
        assert_eq!(next, None);
    }

    #[tokio::test]
    async fn test_list_for_thread() {
        let registry = TaskRegistry::new();
        let (_id1, _rx1, _log1) = registry.register(1, "tool_a").await;
        let (_id2, _rx2, _log2) = registry.register(1, "tool_b").await;
        let (_id3, _rx3, _log3) = registry.register(2, "tool_c").await;

        let thread1_tasks = registry.list_for_thread(1).await;
        assert_eq!(thread1_tasks.len(), 2);
        let names: Vec<&str> = thread1_tasks.iter().map(|t| t.tool_name.as_str()).collect();
        assert!(names.contains(&"tool_a"));
        assert!(names.contains(&"tool_b"));

        let thread2_tasks = registry.list_for_thread(2).await;
        assert_eq!(thread2_tasks.len(), 1);
        assert_eq!(thread2_tasks[0].tool_name, "tool_c");

        let thread3_tasks = registry.list_for_thread(3).await;
        assert!(thread3_tasks.is_empty());
    }

    #[tokio::test]
    async fn test_running_count() {
        let registry = TaskRegistry::new();
        assert_eq!(registry.running_count().await, 0);

        let (id1, _rx1, _log1) = registry.register(1, "tool_a").await;
        assert_eq!(registry.running_count().await, 1);

        let (id2, _rx2, _log2) = registry.register(1, "tool_b").await;
        assert_eq!(registry.running_count().await, 2);

        registry.set_status(&id1, TaskStatus::Completed("ok".to_string())).await;
        assert_eq!(registry.running_count().await, 1);

        registry.set_status(&id2, TaskStatus::Failed("err".to_string())).await;
        assert_eq!(registry.running_count().await, 0);
    }

    #[tokio::test]
    async fn test_set_status_non_existent() {
        let registry = TaskRegistry::new();
        let result = registry.set_status("nonexistent", TaskStatus::Completed("ok".to_string())).await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_log_buffer_overflow() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;

        // Add 10,005 lines
        for i in 0..10_005 {
            registry.append_log(&task_id, &format!("line{}", i)).await;
        }

        // Should keep only the last 10,000
        // Use a high limit to read all buffered lines (default limit=100 won't suffice)
        let (lines, _next) = registry.read_logs(&task_id, None, Some(20_000)).await;
        assert_eq!(lines.len(), 10_000);
        assert_eq!(lines[0], "line5"); // lines 0-4 were dropped
        assert_eq!(lines[9999], "line10004");
    }

    #[tokio::test]
    async fn test_cancel_fires_abort_signal() {
        let registry = TaskRegistry::new();
        let (task_id, rx, _log) = registry.register(42, "tool").await;

        // Cancel the task
        registry.cancel(&task_id).await;

        // The abort receiver should have been signalled
        let result = rx.await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_double_cancel() {
        let registry = TaskRegistry::new();
        let (task_id, _rx, _log) = registry.register(42, "tool").await;

        assert!(registry.cancel(&task_id).await);
        // Second cancel should still return true (task exists, set to Cancelled again)
        assert!(registry.cancel(&task_id).await);
    }
}
