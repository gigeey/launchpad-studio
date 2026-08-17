use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

/// Status of a managed run.
#[derive(Debug, Clone, PartialEq)]
pub enum RunStatus {
    Running,
    Completed,
    Cancelled,
}

/// Record of a managed run for tracking purposes.
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub run_id: String,
    pub backend_id: String,
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub scope_key: Option<String>,
    pub status: RunStatus,
}

/// Thread-safe registry tracking all active and completed runs.
#[derive(Debug, Clone)]
pub struct RunRegistry {
    records: Arc<Mutex<HashMap<String, RunRecord>>>,
}

impl RunRegistry {
    pub fn new() -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, record: RunRecord) {
        let mut records = self.records.lock().unwrap();
        records.insert(record.run_id.clone(), record);
    }

    pub fn update_status(&self, run_id: &str, status: RunStatus) {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.get_mut(run_id) {
            record.status = status;
        }
    }

    pub fn get(&self, run_id: &str) -> Option<RunRecord> {
        let records = self.records.lock().unwrap();
        records.get(run_id).cloned()
    }

    pub fn list_active(&self) -> Vec<RunRecord> {
        let records = self.records.lock().unwrap();
        records
            .values()
            .filter(|r| r.status == RunStatus::Running)
            .cloned()
            .collect()
    }

    pub fn remove(&self, run_id: &str) {
        let mut records = self.records.lock().unwrap();
        records.remove(run_id);
    }
}

impl Default for RunRegistry {
    fn default() -> Self {
        Self::new()
    }
}
