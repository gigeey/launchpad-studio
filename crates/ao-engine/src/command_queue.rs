use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

pub struct CommandQueue {
    lanes: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl CommandQueue {
    pub fn new() -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn acquire(&self, lane_id: &str, max_concurrent: u32) -> OwnedSemaphorePermit {
        let semaphore = {
            let mut lanes = self.lanes.lock().await;
            lanes
                .entry(lane_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(max_concurrent as usize)))
                .clone()
        };

        semaphore
            .acquire_owned()
            .await
            .expect("semaphore should not be closed")
    }
}
