use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;

use super::handle::{BackgroundCommandHandle, BackgroundCommandStatus};
use super::id::BackgroundCommandId;

/// Error returned by [`BackgroundCommandRegistry::insert`].
#[derive(Debug, Error)]
pub enum BackgroundCommandRegistryError {
    #[error("background command registry cap of {cap} reached ({live} commands live)")]
    AtCapacity { live: usize, cap: usize },
}

/// Per-session registry of live [`BackgroundCommandHandle`]s.
///
/// Shared via `Arc<BackgroundCommandRegistry>` on `RunnerContext` so every
/// tool invocation in a session can access background command state without a
/// process-wide singleton. This follows the same Arc-per-context sharing
/// pattern used by `background_agents` and `read_file_state`.
///
/// Enforces a cap on [`insert`](Self::insert) to prevent unbounded registry
/// growth from runaway background spawning.
pub struct BackgroundCommandRegistry {
    inner: RwLock<HashMap<BackgroundCommandId, Arc<BackgroundCommandHandle>>>,
    cap: usize,
}

impl BackgroundCommandRegistry {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            cap,
        }
    }

    /// Insert `handle`. Returns [`BackgroundCommandRegistryError::AtCapacity`]
    /// when the live count is already at the cap.
    pub async fn insert(
        &self,
        handle: Arc<BackgroundCommandHandle>,
    ) -> Result<(), BackgroundCommandRegistryError> {
        let mut map = self.inner.write().await;
        if map.len() >= self.cap {
            return Err(BackgroundCommandRegistryError::AtCapacity {
                live: map.len(),
                cap: self.cap,
            });
        }
        map.insert(handle.id.clone(), handle);
        Ok(())
    }

    /// Return an Arc clone of the handle for `id`, or `None`.
    pub async fn get(&self, id: &BackgroundCommandId) -> Option<Arc<BackgroundCommandHandle>> {
        self.inner.read().await.get(id).cloned()
    }

    /// Remove and return the handle for `id`, or `None`.
    pub async fn remove(
        &self,
        id: &BackgroundCommandId,
    ) -> Option<Arc<BackgroundCommandHandle>> {
        self.inner.write().await.remove(id)
    }

    /// Snapshot of all registered command ids.
    pub async fn list(&self) -> Vec<BackgroundCommandId> {
        self.inner.read().await.keys().cloned().collect()
    }

    /// Current number of registered commands.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// The configured capacity for this registry.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Return all commands whose status is `Running`.
    pub async fn list_running(&self) -> Vec<Arc<BackgroundCommandHandle>> {
        self.inner
            .read()
            .await
            .values()
            .filter(|h| matches!(*h.status.lock().unwrap(), BackgroundCommandStatus::Running))
            .cloned()
            .collect()
    }
}
