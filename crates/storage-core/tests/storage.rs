//! Storage round-trips: settings KV, persistent grants, revocation.

use capability_core::{Capability, PrincipalKind, Resource, ResourceScope};
use policy_core::{GrantedScope, GrantLifetime};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use storage_core::{default_db_path, persistent_grant_hook, Storage, StorageError};

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "utsuwa-storage-test-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("state.db")
}

fn sample_grant(lifetime: GrantLifetime) -> GrantedScope {
    GrantedScope {
        principal_kind: PrincipalKind::Agent,
        capability: Capability::FilesystemRead,
        scope: ResourceScope::new(vec![Resource::Path(PathBuf::from("/work"))]),
        lifetime,
    }
}

#[test]
fn settings_round_trip() {
    let db = temp_db("settings");
    let store = Storage::open(&db).unwrap();
    assert_eq!(store.get_setting("theme").unwrap(), None);

    store.set_setting("theme", &json!("dark")).unwrap();
    assert_eq!(store.get_setting("theme").unwrap(), Some(json!("dark")));

    store
        .set_setting("agent", &json!({"model": "m1", "temp": 0.5}))
        .unwrap();
    assert_eq!(
        store.get_setting("agent").unwrap(),
        Some(json!({"model": "m1", "temp": 0.5}))
    );

    // Overwrite replaces.
    store.set_setting("theme", &json!("light")).unwrap();
    assert_eq!(store.get_setting("theme").unwrap(), Some(json!("light")));

    // Delete removes; second delete reports missing.
    assert!(store.delete_setting("theme").unwrap());
    assert_eq!(store.get_setting("theme").unwrap(), None);
    assert!(!store.delete_setting("theme").unwrap());
}

#[test]
fn settings_survive_reopen() {
    let db = temp_db("reopen");
    {
        let store = Storage::open(&db).unwrap();
        store.set_setting("k", &json!([1, 2, 3])).unwrap();
    }
    let store = Storage::open(&db).unwrap();
    assert_eq!(store.get_setting("k").unwrap(), Some(json!([1, 2, 3])));
}

#[test]
fn grants_persist_and_revoke() {
    let db = temp_db("grants");
    let store = Storage::open(&db).unwrap();
    assert!(store.load_grants().unwrap().is_empty());

    let id = store.save_grant(&sample_grant(GrantLifetime::Persistent)).unwrap();
    let loaded = store.load_grants().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, id);
    assert_eq!(loaded[0].grant, sample_grant(GrantLifetime::Persistent));

    // Reopen: grant still there.
    drop(store);
    let store = Storage::open(&db).unwrap();
    assert_eq!(store.load_grants().unwrap().len(), 1);

    store.delete_grant(id).unwrap();
    assert!(store.load_grants().unwrap().is_empty());

    // Revoking twice fails.
    assert!(matches!(
        store.delete_grant(id),
        Err(StorageError::GrantNotFound(_))
    ));
}

#[test]
fn non_persistent_grants_rejected() {
    let db = temp_db("lifetimes");
    let store = Storage::open(&db).unwrap();
    for lifetime in [GrantLifetime::Once, GrantLifetime::Task, GrantLifetime::Session] {
        assert!(matches!(
            store.save_grant(&sample_grant(lifetime)),
            Err(StorageError::NonPersistentGrant(_))
        ));
    }
    assert!(store.load_grants().unwrap().is_empty());
}

#[test]
fn persistent_grant_hook_writes_through() {
    let db = temp_db("hook");
    let store = Arc::new(Mutex::new(Storage::open(&db).unwrap()));
    let hook = persistent_grant_hook(store.clone());

    hook(&sample_grant(GrantLifetime::Persistent)).unwrap();
    let loaded = store.lock().unwrap().load_grants().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].grant, sample_grant(GrantLifetime::Persistent));

    // Narrower lifetimes never reach the database.
    assert!(hook(&sample_grant(GrantLifetime::Session)).is_err());
    assert_eq!(store.lock().unwrap().load_grants().unwrap().len(), 1);
}

#[test]
fn default_db_path_is_sane() {
    let p = default_db_path("utsuwa-test");
    assert!(p.ends_with("state.db"));
    assert!(p.to_string_lossy().contains("utsuwa-test"));
}
