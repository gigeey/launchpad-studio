use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::RwLock;

use super::handle::{BackgroundProcessHandle, BackgroundProcessId};

/// Error returned by [`BackgroundProcessRegistry::insert`].
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Refused because the live count is at or above the configured cap.
    #[error("background process registry cap of {cap} reached ({live} processes live)")]
    AtCapacity { live: usize, cap: usize },
}

/// Per-context registry of live [`BackgroundProcessHandle`]s.
///
/// Enforces a capacity cap on [`insert`](Self::insert) and exposes
/// Arc-based lookup so callers can interact with the child process
/// (e.g., kill it for cleanup) after retrieval.
pub struct BackgroundProcessRegistry {
    inner: RwLock<HashMap<BackgroundProcessId, Arc<BackgroundProcessHandle>>>,
    cap: usize,
}

impl BackgroundProcessRegistry {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            cap,
        }
    }

    /// Insert `handle`, returning [`RegistryError::AtCapacity`] if the live
    /// count is already at the cap.
    pub async fn insert(&self, handle: Arc<BackgroundProcessHandle>) -> Result<(), RegistryError> {
        let mut map = self.inner.write().await;
        if map.len() >= self.cap {
            return Err(RegistryError::AtCapacity {
                live: map.len(),
                cap: self.cap,
            });
        }
        map.insert(handle.id.clone(), handle);
        Ok(())
    }

    /// Return the `Arc<BackgroundProcessHandle>` for `id`, or `None` if not
    /// present. The caller receives a clone of the Arc — the handle remains
    /// in the registry.
    pub async fn get(&self, id: &BackgroundProcessId) -> Option<Arc<BackgroundProcessHandle>> {
        self.inner.read().await.get(id).cloned()
    }

    /// Remove and return the `Arc<BackgroundProcessHandle>` for `id`, or `None`
    /// if not present.
    pub async fn remove(&self, id: &BackgroundProcessId) -> Option<Arc<BackgroundProcessHandle>> {
        self.inner.write().await.remove(id)
    }

    /// Return a snapshot of all registered process ids.
    pub async fn list(&self) -> Vec<BackgroundProcessId> {
        self.inner.read().await.keys().cloned().collect()
    }

    /// Current number of registered processes.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// The configured capacity for this registry.
    pub fn cap(&self) -> usize {
        self.cap
    }
}
