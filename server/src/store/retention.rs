//! Keeps the local database under a fixed size by purging the oldest records.
use std::time::Duration;

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::Result;

use super::{now_ms, Store};

pub const DATABASE_SIZE_CAP_BYTES: i64 = 16 << 30;
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// A purge that starts above the cap continues until usage drops to this share of it.
const PURGE_TARGET_PERCENT: i64 = 90;
/// Each purge window covers this fraction of the span between the oldest record and the protected line.
const WINDOW_DIVISOR: i64 = 10;
/// Records newer than this are never purged.
const PROTECTED_RECENT_MS: i64 = 60 * 60 * 1000;

/// Conversations that are idle and last changed before the cutoff (`?1`).
const OLD_CONVERSATIONS: &str = "SELECT conversation_id FROM conversations
     WHERE active_run_id IS NULL AND updated_at_ms < ?1";

/// Lets freed pages be returned to the operating system. SQLite applies the mode only through `VACUUM`.
pub(super) async fn ensure_incremental_vacuum(pool: &SqlitePool) -> Result<()> {
    let mut connection = pool.acquire().await?;
    let mode: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
        .fetch_one(&mut *connection)
        .await?;
    if mode != 2 {
        sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
            .execute(&mut *connection)
            .await?;
        sqlx::query("VACUUM").execute(&mut *connection).await?;
    }
    Ok(())
}

impl Store {
    /// Enforces the database size cap now and then every minute until shutdown.
    pub async fn run_size_cap(self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            if let Err(error) = self.enforce_size_cap(DATABASE_SIZE_CAP_BYTES).await {
                tracing::warn!(%error, "failed to enforce the database size cap");
            }
        }
    }

    pub async fn enforce_size_cap(&self, cap_bytes: i64) -> Result<()> {
        if self.used_bytes().await? <= cap_bytes {
            return Ok(());
        }
        let target_bytes = cap_bytes / 100 * PURGE_TARGET_PERCENT;
        let started_bytes = self.used_bytes().await?;
        let mut purged_before_ms = 0;
        while self.used_bytes().await? > target_bytes {
            match self.purge_window(purged_before_ms).await? {
                Some(cutoff_ms) => purged_before_ms = cutoff_ms,
                None => {
                    let used_bytes = self.used_bytes().await?;
                    tracing::warn!(
                        used_bytes,
                        cap_bytes,
                        "database exceeds the size cap but only protected records remain"
                    );
                    break;
                }
            }
        }
        self.reclaim_free_pages().await?;
        let used_bytes = self.used_bytes().await?;
        tracing::info!(
            started_bytes,
            used_bytes,
            cap_bytes,
            "purged the oldest records to enforce the database size cap"
        );
        Ok(())
    }

    /// Bytes holding data, excluding free pages that are waiting to be reused or released.
    async fn used_bytes(&self) -> Result<i64> {
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await?;
        let freelist_count: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;
        Ok((page_count - freelist_count) * page_size)
    }

    async fn reclaim_free_pages(&self) -> Result<()> {
        let _write = self.writes.lock().await;
        sqlx::query("PRAGMA incremental_vacuum")
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Purges the oldest slice of records at or after `after_ms` and returns the slice's end.
    /// Returns `None` when no unprotected record remains.
    async fn purge_window(&self, after_ms: i64) -> Result<Option<i64>> {
        let _write = self.writes.lock().await;
        let protected_after_ms = now_ms() - PROTECTED_RECENT_MS;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let oldest_ms: Option<i64> = sqlx::query_scalar(
            "SELECT MIN(created_ms) FROM (
                 SELECT MIN(created_at_ms) AS created_ms FROM blobs WHERE created_at_ms >= ?1
                 UNION ALL
                 SELECT MIN(received_at_ms) FROM cursor_run_traces WHERE received_at_ms >= ?1
                 UNION ALL
                 SELECT MIN(call.created_at_ms) FROM llm_calls call
                 JOIN llm_call_requests request ON request.call_id = call.call_id
                 WHERE call.created_at_ms >= ?1
                 UNION ALL
                 SELECT MIN(updated_at_ms) FROM conversations
                 WHERE active_run_id IS NULL AND updated_at_ms >= ?1
             )",
        )
        .bind(after_ms)
        .fetch_one(&mut *tx)
        .await?;
        let Some(oldest_ms) = oldest_ms.filter(|oldest_ms| *oldest_ms < protected_after_ms) else {
            return Ok(None);
        };
        let window_ms = ((protected_after_ms - oldest_ms) / WINDOW_DIVISOR).max(1);
        let cutoff_ms = (oldest_ms + window_ms).min(protected_after_ms);

        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *tx)
            .await?;
        // Call rows stay as statistics; only their request and response details go.
        for table in ["llm_call_requests", "llm_call_response_chunks"] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE call_id IN (
                     SELECT call_id FROM llm_calls WHERE created_at_ms < ?1
                 )"
            ))
            .bind(cutoff_ms)
            .execute(&mut *tx)
            .await?;
        }
        // Artifacts are removed by the foreign key cascade.
        sqlx::query("DELETE FROM cursor_run_traces WHERE received_at_ms < ?1")
            .bind(cutoff_ms)
            .execute(&mut *tx)
            .await?;
        for statement in [
            format!(
                "DELETE FROM tool_round_calls WHERE round_id IN (
                     SELECT round_id FROM tool_rounds WHERE run_id IN (
                         SELECT run_id FROM runs WHERE conversation_id IN ({OLD_CONVERSATIONS})
                     )
                 )"
            ),
            format!(
                "DELETE FROM tool_rounds WHERE run_id IN (
                     SELECT run_id FROM runs WHERE conversation_id IN ({OLD_CONVERSATIONS})
                 )"
            ),
            format!("DELETE FROM runs WHERE conversation_id IN ({OLD_CONVERSATIONS})"),
            format!("DELETE FROM input_anchors WHERE conversation_id IN ({OLD_CONVERSATIONS})"),
            format!(
                "DELETE FROM checkpoint_messages WHERE conversation_id IN ({OLD_CONVERSATIONS})"
            ),
            format!("DELETE FROM messages WHERE conversation_id IN ({OLD_CONVERSATIONS})"),
            format!(
                "DELETE FROM conversation_checkpoints WHERE conversation_id IN ({OLD_CONVERSATIONS})"
            ),
            format!("DELETE FROM conversations WHERE conversation_id IN ({OLD_CONVERSATIONS})"),
        ] {
            sqlx::query(&statement)
                .bind(cutoff_ms)
                .execute(&mut *tx)
                .await?;
        }

        // Blobs have no owner. Keep any blob that may belong to a surviving message and any trace artifact.
        let blob_cutoff_ms =
            sqlx::query_scalar::<_, Option<i64>>("SELECT MIN(created_at_ms) FROM messages")
                .fetch_one(&mut *tx)
                .await?
                .map_or(cutoff_ms, |oldest_message_ms| {
                    oldest_message_ms.min(cutoff_ms)
                });
        const PURGEABLE_BLOBS: &str = "SELECT blob_id FROM blobs
             WHERE created_at_ms < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM cursor_run_trace_artifacts a WHERE a.blob_id = blobs.blob_id
               )";
        sqlx::query(&format!(
            "DELETE FROM blob_edges
             WHERE parent_blob_id IN ({PURGEABLE_BLOBS}) OR child_blob_id IN ({PURGEABLE_BLOBS})"
        ))
        .bind(blob_cutoff_ms)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "DELETE FROM blobs WHERE blob_id IN ({PURGEABLE_BLOBS})"
        ))
        .bind(blob_cutoff_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(cutoff_ms))
    }
}
