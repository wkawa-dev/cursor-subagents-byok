CREATE INDEX IF NOT EXISTS blobs_created_at ON blobs(created_at_ms);
CREATE INDEX IF NOT EXISTS messages_created_at ON messages(created_at_ms);
CREATE INDEX IF NOT EXISTS cursor_run_trace_artifacts_blob ON cursor_run_trace_artifacts(blob_id);
CREATE INDEX IF NOT EXISTS conversations_updated ON conversations(active_run_id, updated_at_ms);
CREATE INDEX IF NOT EXISTS checkpoint_messages_conversation ON checkpoint_messages(conversation_id);
