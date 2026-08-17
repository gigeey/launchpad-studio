use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use super::handle::{BackgroundProcessHandle, BackgroundProcessId};
use super::registry::{BackgroundProcessRegistry, RegistryError};

fn make_dummy_handle(id: BackgroundProcessId) -> Arc<BackgroundProcessHandle> {
    // Spawn a no-op child so we have a real tokio::process::Child.
    let child = tokio::process::Command::new("true")
        .spawn()
        .expect("failed to spawn 'true'");
    Arc::new(BackgroundProcessHandle {
        id,
        child: tokio::sync::Mutex::new(child),
        stdout: std::sync::Mutex::new(Vec::new()),
        stderr: std::sync::Mutex::new(Vec::new()),
        started_at: SystemTime::now(),
        command: "true".to_string(),
        cancel: CancellationToken::new(),
    })
}

#[tokio::test]
async fn registry_insert_and_get() {
    let registry = BackgroundProcessRegistry::new(4);
    let id = BackgroundProcessId::new();
    let handle = make_dummy_handle(id.clone());

    assert!(registry.insert(handle).await.is_ok());
    assert_eq!(registry.len().await, 1);
    assert!(registry.get(&id).await.is_some());
}

#[tokio::test]
async fn registry_at_capacity_returns_error() {
    let registry = BackgroundProcessRegistry::new(1);

    let id1 = BackgroundProcessId::new();
    let id2 = BackgroundProcessId::new();
    let h1 = make_dummy_handle(id1);
    let h2 = make_dummy_handle(id2);

    assert!(registry.insert(h1).await.is_ok());
    let result = registry.insert(h2).await;
    assert!(
        matches!(result, Err(RegistryError::AtCapacity { live: 1, cap: 1 })),
        "expected AtCapacity, got: {result:?}",
    );
}

#[tokio::test]
async fn registry_remove_releases_slot() {
    let registry = BackgroundProcessRegistry::new(1);
    let id = BackgroundProcessId::new();
    let handle = make_dummy_handle(id.clone());

    registry.insert(handle).await.unwrap();
    assert_eq!(registry.len().await, 1);

    let removed = registry.remove(&id).await;
    assert!(removed.is_some(), "remove should return the handle");
    assert_eq!(registry.len().await, 0, "slot should be released");
    assert!(registry.get(&id).await.is_none(), "entry should be gone");

    // After removal, a new insert should succeed (slot was released).
    let id2 = BackgroundProcessId::new();
    let h2 = make_dummy_handle(id2);
    assert!(registry.insert(h2).await.is_ok(), "slot should be available again");
}
