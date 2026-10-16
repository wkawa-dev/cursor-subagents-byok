//! Verifies that the database size cap purges the oldest records and keeps protected ones.
#[path = "support/fixtures.rs"]
mod fixtures;

use cursor_server::store::Store;

const MIB: usize = 1 << 20;

async fn insert_blob(store: &Store, seed: u8, created_at_ms: i64) -> Vec<u8> {
    let blob_id = vec![seed; 32];
    sqlx::query("INSERT INTO blobs(blob_id, data, created_at_ms) VALUES (?, ?, ?)")
        .bind(&blob_id)
        .bind(vec![seed; MIB])
        .bind(created_at_ms)
        .execute(store.pool())
        .await
        .unwrap();
    blob_id
}

async fn insert_conversation(
    store: &Store,
    id: &str,
    active_run_id: Option<&str>,
    updated_at_ms: i64,
    message_created_at_ms: i64,
) {
    sqlx::query(
        "INSERT INTO conversations(conversation_id, active_run_id, updated_at_ms) VALUES (?, ?, ?)",
    )
    .bind(id)
    .bind(active_run_id)
    .bind(updated_at_ms)
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
         VALUES (?, 'message', 'user', 'user', '{}', ?)",
    )
    .bind(id)
    .bind(message_created_at_ms)
    .execute(store.pool())
    .await
    .unwrap();
}

async fn blobs(store: &Store, blob_id: &[u8]) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE blob_id = ?")
        .bind(blob_id)
        .fetch_one(store.pool())
        .await
        .unwrap()
}

async fn rows(store: &Store, table: &str, conversation_id: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE conversation_id = ?"
    ))
    .bind(conversation_id)
    .fetch_one(store.pool())
    .await
    .unwrap()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn purges_the_oldest_records_until_under_the_cap() {
    let (directory, store) = fixtures::temp_store().await;
    let now_ms = now_ms();
    let old = [
        insert_blob(&store, 1, 1_000).await,
        insert_blob(&store, 2, 2_000).await,
        insert_blob(&store, 3, 3_000).await,
    ];
    let recent = insert_blob(&store, 4, now_ms).await;
    insert_conversation(&store, "old", None, 1_000, 1_000).await;
    insert_conversation(&store, "active", Some("run"), 1_000, now_ms).await;
    let before = std::fs::metadata(directory.path().join("test.db"))
        .unwrap()
        .len();

    store.enforce_size_cap(2 * MIB as i64).await.unwrap();

    for blob in &old {
        assert_eq!(blobs(&store, blob).await, 0);
    }
    assert_eq!(blobs(&store, &recent).await, 1);
    assert_eq!(rows(&store, "conversations", "old").await, 0);
    assert_eq!(rows(&store, "messages", "old").await, 0);
    assert_eq!(rows(&store, "conversations", "active").await, 1);
    let after = std::fs::metadata(directory.path().join("test.db"))
        .unwrap()
        .len();
    assert!(after < before, "{after} should be smaller than {before}");
}

#[tokio::test]
async fn keeps_everything_when_only_protected_records_exceed_the_cap() {
    let (_directory, store) = fixtures::temp_store().await;
    let now_ms = now_ms();
    let recent = insert_blob(&store, 9, now_ms).await;

    store.enforce_size_cap(MIB as i64 / 2).await.unwrap();

    assert_eq!(blobs(&store, &recent).await, 1);
}
