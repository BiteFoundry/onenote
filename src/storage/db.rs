//! Standalone OneNote SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_USER_VERSION_UNICODE61: i32 = 70;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 71;

/// Initializes SQLite PRAGMAs, OneNote tables, indexes, and FTS5 virtual table.
/// Returns `true` if `onenote_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
pub fn init_connection(conn: &Connection, is_memory: bool) -> Result<bool> {
    conn.set_prepared_statement_cache_capacity(64);
    if !is_memory {
        let _ = conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA journal_size_limit = 67108864;
            ",
        );
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -8000;
        ",
    )
    .context("Failed to configure SQLite PRAGMAs")?;

    if !is_memory {
        let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
        let user_version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);
        if user_version == SCHEMA_USER_VERSION_TRIGRAM
            || user_version == SCHEMA_USER_VERSION_UNICODE61
        {
            let intact: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='onenote_fts')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='onenote_pages')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='onenote_notebooks')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_onenote_pages_section_order')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if intact {
                return Ok(user_version == SCHEMA_USER_VERSION_TRIGRAM);
            }
        }
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS onenote_notebooks (
            notebook_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            display_name TEXT NOT NULL,
            color TEXT,
            is_default INTEGER NOT NULL DEFAULT 0,
            is_shared INTEGER NOT NULL DEFAULT 0,
            user_role TEXT,
            created_at TEXT,
            last_modified_at TEXT,
            created_by TEXT,
            web_url TEXT
        );

        CREATE TABLE IF NOT EXISTS onenote_section_groups (
            section_group_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            notebook_id TEXT,
            parent_section_group_id TEXT,
            display_name TEXT NOT NULL,
            created_at TEXT,
            last_modified_at TEXT
        );

        CREATE TABLE IF NOT EXISTS onenote_sections (
            section_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            notebook_id TEXT,
            notebook_name TEXT,
            parent_section_group_id TEXT,
            parent_section_group_name TEXT,
            display_name TEXT NOT NULL,
            color TEXT,
            is_default INTEGER NOT NULL DEFAULT 0,
            pages_url TEXT,
            created_at TEXT,
            last_modified_at TEXT,
            web_url TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_onenote_sections_scope_notebook
            ON onenote_sections(scope_key, notebook_id);

        CREATE TABLE IF NOT EXISTS onenote_pages (
            page_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            section_key TEXT NOT NULL,
            section_id TEXT NOT NULL,
            section_name TEXT,
            notebook_id TEXT,
            notebook_name TEXT,
            title TEXT NOT NULL,
            level INTEGER NOT NULL DEFAULT 0,
            page_order INTEGER NOT NULL DEFAULT 0,
            created_at TEXT,
            last_modified_at TEXT,
            content_url TEXT,
            web_url TEXT,
            preview_text TEXT,
            has_todo INTEGER NOT NULL DEFAULT 0,
            detail_json TEXT,
            body_plain TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_onenote_pages_section_order
            ON onenote_pages(section_key, page_order ASC, last_modified_at DESC);

        CREATE INDEX IF NOT EXISTS idx_onenote_pages_id
            ON onenote_pages(id);

        CREATE TABLE IF NOT EXISTS sync_cursors (
            scope_key TEXT PRIMARY KEY,
            delta_link TEXT,
            history_next_link TEXT,
            last_synced_at TEXT NOT NULL
        );
        ",
    )
    .context("Failed to create OneNote SQLite schema")?;

    let onenote_trigram = ensure_fts5_table(
        conn,
        "onenote_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS onenote_fts USING fts5(
            page_key UNINDEXED,
            section_key UNINDEXED,
            title,
            notebook_name,
            section_name,
            body_plain,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS onenote_fts USING fts5(
            page_key UNINDEXED,
            section_key UNINDEXED,
            title,
            notebook_name,
            section_name,
            body_plain,
            tokenize='unicode61'
        );
        ",
    )?;

    if !is_memory {
        let version = if onenote_trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("Failed to set SQLite user_version")?;
    }

    Ok(onenote_trigram)
}

/// Configures a read-only WAL reader connection for concurrent UI reads.
pub fn init_reader_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -4000;
        ",
    )
    .context("Failed to configure SQLite reader PRAGMAs")?;
    let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
    Ok(())
}

/// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`).
pub fn shrink_connection_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql).with_context(|| {
            format!("Failed to create FTS5 virtual table ({table_name}, unicode61)")
        })?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .ok();
    let uses_trigram = sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true);
    Ok(uses_trigram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn table_exists(conn: &Connection, table_name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table_name],
            |row| row.get(0),
        )
        .unwrap_or(false)
    }

    #[test]
    fn test_pure_onenote_schema_and_user_version_fast_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("onenote_fastpath.db");

        let first_trigram = {
            let conn = Connection::open(&db_path).unwrap();
            let trigram = init_connection(&conn, false).unwrap();
            let ver: i32 = conn
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                ver,
                if trigram {
                    SCHEMA_USER_VERSION_TRIGRAM
                } else {
                    SCHEMA_USER_VERSION_UNICODE61
                }
            );
            for required in [
                "onenote_notebooks",
                "onenote_section_groups",
                "onenote_sections",
                "onenote_pages",
                "onenote_fts",
                "sync_cursors",
            ] {
                assert!(table_exists(&conn, required), "{required} should exist");
            }
            trigram
        };

        let conn2 = Connection::open(&db_path).unwrap();
        assert_eq!(init_connection(&conn2, false).unwrap(), first_trigram);

        let reader = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        init_reader_connection(&reader).unwrap();
        let reader_cache: i32 = reader
            .query_row("PRAGMA cache_size;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reader_cache, -4000);
    }
}
