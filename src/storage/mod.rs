//! Local SQLite WAL persistence + 4-reader pool + FTS5 (`trigram` + `LIKE` hybrid) OneNote repository.

pub mod db;
pub mod onenote;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::models::{
    OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteSearchHit, OnenoteSection,
    OnenoteSectionGroup, SyncCursor,
};

const MAX_READER_POOL_SIZE: usize = 4;

/// Escapes `%`, `_`, and `\` for `LIKE ... ESCAPE '\'`.
pub fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    writer: Arc<Mutex<rusqlite::Connection>>,
    readers: Option<Arc<Mutex<Vec<rusqlite::Connection>>>>,
    trigram_enabled: bool,
}

enum ReadConnInner<'a> {
    Pooled {
        conn: Option<rusqlite::Connection>,
        pool: &'a Arc<Mutex<Vec<rusqlite::Connection>>>,
    },
    Writer(std::sync::MutexGuard<'a, rusqlite::Connection>),
}

struct ReadConnGuard<'a>(ReadConnInner<'a>);

impl std::ops::Deref for ReadConnGuard<'_> {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            ReadConnInner::Pooled { conn, .. } => conn
                .as_ref()
                .expect("pooled reader connection available until Drop"),
            ReadConnInner::Writer(guard) => guard,
        }
    }
}

impl Drop for ReadConnGuard<'_> {
    fn drop(&mut self) {
        if let ReadConnInner::Pooled { conn, pool } = &mut self.0 {
            if let Some(c) = conn.take() {
                if let Ok(mut guard) = pool.lock() {
                    if guard.len() < MAX_READER_POOL_SIZE {
                        guard.push(c);
                    }
                }
            }
        }
    }
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        crate::config::ensure_parent(path)?;
        let conn = rusqlite::Connection::open(path)?;
        let trigram_enabled = db::init_connection(&conn, false)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(Mutex::new(conn)),
            readers: Some(Arc::new(Mutex::new(Vec::with_capacity(
                MAX_READER_POOL_SIZE,
            )))),
            trigram_enabled,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let trigram_enabled = db::init_connection(&conn, true)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            writer: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    /// Opens an existing database with a single read-only connection without creating an empty DB file.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(path, flags)?;
        db::init_reader_connection(&conn)?;
        let trigram_enabled: bool = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'onenote_fts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|s| s.to_ascii_lowercase().contains("trigram"))
            .unwrap_or(false);
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.writer
    }

    pub fn trigram_enabled(&self) -> bool {
        self.trigram_enabled
    }

    /// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`) and runs `PRAGMA optimize`.
    pub fn shrink_memory(&self) {
        if let Ok(conn) = self.writer.lock() {
            let _ = conn.execute_batch("PRAGMA optimize;");
            db::shrink_connection_memory(&conn);
        }
        if let Some(pool) = &self.readers {
            if let Ok(guard) = pool.lock() {
                for c in guard.iter() {
                    db::shrink_connection_memory(c);
                }
            }
        }
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire SQLite connection lock (poisoned mutex)"))
    }

    fn read_conn(&self) -> Result<ReadConnGuard<'_>> {
        if let Some(pool) = &self.readers {
            let existing = {
                let mut guard = pool.lock().map_err(|_| {
                    anyhow::anyhow!("Failed to acquire SQLite reader pool lock (poisoned mutex)")
                })?;
                guard.pop()
            };
            let conn = match existing {
                Some(c) => c,
                None => {
                    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let c = rusqlite::Connection::open_with_flags(&self.path, flags)?;
                    db::init_reader_connection(&c)?;
                    c
                }
            };
            Ok(ReadConnGuard(ReadConnInner::Pooled {
                conn: Some(conn),
                pool,
            }))
        } else {
            Ok(ReadConnGuard(ReadConnInner::Writer(self.lock_conn()?)))
        }
    }

    // --- OneNote Notebooks, Section Groups, Sections, Pages & Search ---

    pub fn upsert_onenote_notebooks(&self, items: &[OnenoteNotebook]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::upsert_onenote_notebooks(&mut conn, items)
    }

    pub fn list_onenote_notebooks(&self) -> Result<Vec<OnenoteNotebook>> {
        let conn = self.read_conn()?;
        onenote::list_onenote_notebooks(&conn)
    }

    pub fn upsert_onenote_section_groups(&self, items: &[OnenoteSectionGroup]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::upsert_onenote_section_groups(&mut conn, items)
    }

    pub fn list_onenote_section_groups(&self) -> Result<Vec<OnenoteSectionGroup>> {
        let conn = self.read_conn()?;
        onenote::list_onenote_section_groups(&conn)
    }

    pub fn upsert_onenote_sections(&self, items: &[OnenoteSection]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::upsert_onenote_sections(&mut conn, items)
    }

    pub fn list_onenote_sections(
        &self,
        notebook_key_or_id: Option<&str>,
    ) -> Result<Vec<OnenoteSection>> {
        let conn = self.read_conn()?;
        onenote::list_onenote_sections(&conn, notebook_key_or_id)
    }

    pub fn upsert_onenote_pages(&self, items: &[OnenotePageSummary]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::upsert_onenote_pages(&mut conn, items)
    }

    pub fn reconcile_onenote_hierarchy(
        &self,
        notebooks: &[OnenoteNotebook],
        section_groups: &[OnenoteSectionGroup],
        sections: &[OnenoteSection],
        synced_all_scopes: bool,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_hierarchy(
            &mut conn,
            notebooks,
            section_groups,
            sections,
            synced_all_scopes,
        )
    }

    pub fn reconcile_onenote_hierarchy_for_scopes(
        &self,
        notebooks: &[OnenoteNotebook],
        section_groups: &[OnenoteSectionGroup],
        sections: &[OnenoteSection],
        complete_scopes: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_hierarchy_for_scopes(
            &mut conn,
            notebooks,
            section_groups,
            sections,
            complete_scopes,
        )
    }

    pub fn reconcile_onenote_notebooks_for_scopes(
        &self,
        notebooks: &[OnenoteNotebook],
        complete_scopes: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_notebooks_for_scopes(&mut conn, notebooks, complete_scopes)
    }

    pub fn reconcile_onenote_sections_for_scopes(
        &self,
        section_groups: &[OnenoteSectionGroup],
        sections: &[OnenoteSection],
        complete_scopes: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_sections_for_scopes(
            &mut conn,
            section_groups,
            sections,
            complete_scopes,
        )
    }

    pub fn reconcile_onenote_notebook_sections(
        &self,
        notebook_key: &str,
        sections: &[OnenoteSection],
        is_complete: bool,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_notebook_sections(&mut conn, notebook_key, sections, is_complete)
    }

    pub fn reconcile_onenote_section_pages(
        &self,
        section_key: &str,
        pages: &[OnenotePageSummary],
        is_complete_section_list: bool,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_section_pages(
            &mut conn,
            section_key,
            pages,
            is_complete_section_list,
        )
    }

    pub fn reconcile_onenote_section_pages_returning_invalidated(
        &self,
        section_key: &str,
        pages: &[OnenotePageSummary],
        is_complete_section_list: bool,
    ) -> Result<Vec<String>> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_section_pages_returning_invalidated(
            &mut conn,
            section_key,
            pages,
            is_complete_section_list,
        )
    }

    pub fn reconcile_onenote_section_pages_preserving(
        &self,
        section_key: &str,
        pages: &[OnenotePageSummary],
        is_complete_section_list: bool,
        preserve_page_key: Option<&str>,
        preserve_detail_key: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut conn = self.lock_conn()?;
        onenote::reconcile_onenote_section_pages_preserving(
            &mut conn,
            section_key,
            pages,
            is_complete_section_list,
            preserve_page_key,
            preserve_detail_key,
        )
    }

    pub fn prune_onenote_section_pages(
        &self,
        section_key: &str,
        pages: &[OnenotePageSummary],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::prune_onenote_section_pages(&mut conn, section_key, pages)
    }

    pub fn list_onenote_pages(&self, section_key: &str) -> Result<Vec<OnenotePageSummary>> {
        let conn = self.read_conn()?;
        onenote::list_onenote_pages(&conn, section_key)
    }

    pub fn list_all_onenote_pages(&self, limit: usize) -> Result<Vec<OnenotePageSummary>> {
        let conn = self.read_conn()?;
        let mut stmt = conn.prepare(
            "
            SELECT
                page_key, id, scope_json, scope_key, section_id, section_name,
                notebook_id, notebook_name, title, level, page_order,
                created_at, last_modified_at, content_url, web_url, preview_text, has_todo
            FROM onenote_pages
            ORDER BY last_modified_at DESC, page_order ASC, title COLLATE NOCASE ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            let scope_json: String = row.get(2)?;
            let scope_key: String = row.get(3)?;
            let scope = serde_json::from_str(&scope_json)
                .ok()
                .or_else(|| crate::models::OnenoteScope::from_scope_key(&scope_key, None))
                .unwrap_or_default();
            Ok(OnenotePageSummary {
                page_key: row.get(0)?,
                id: row.get(1)?,
                scope,
                section_id: row.get(4)?,
                section_name: row.get(5)?,
                notebook_id: row.get(6)?,
                notebook_name: row.get(7)?,
                title: row.get(8)?,
                level: row.get(9)?,
                order: row.get(10)?,
                created_at: row.get(11)?,
                last_modified_at: row.get(12)?,
                content_url: row.get(13)?,
                web_url: row.get(14)?,
                preview_text: row.get(15)?,
                has_todo: row.get::<_, i32>(16)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn upsert_onenote_page_detail(&self, detail: &OnenotePageDetail) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::upsert_onenote_page_detail(&mut conn, detail)
    }

    pub fn get_onenote_page_detail(
        &self,
        page_key_or_id: &str,
    ) -> Result<Option<OnenotePageDetail>> {
        let conn = self.read_conn()?;
        onenote::get_onenote_page_detail(&conn, page_key_or_id)
    }

    pub fn get_onenote_page_summary(
        &self,
        page_key_or_id: &str,
    ) -> Result<Option<OnenotePageSummary>> {
        let conn = self.read_conn()?;
        onenote::get_onenote_page_summary(&conn, page_key_or_id)
    }

    pub fn delete_onenote_page(&self, page_key_or_id: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        onenote::delete_onenote_page(&mut conn, page_key_or_id)
    }

    pub fn search_onenote_pages(
        &self,
        query: &str,
        section_key_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OnenoteSearchHit>> {
        let conn = self.read_conn()?;
        onenote::search_onenote_pages(
            &conn,
            query,
            section_key_filter,
            limit,
            self.trigram_enabled,
        )
    }

    // --- Sync Cursors ---

    pub fn get_sync_cursor(&self, scope_key: &str) -> Result<Option<SyncCursor>> {
        let conn = self.read_conn()?;
        conn.query_row(
            "SELECT scope_key, delta_link, history_next_link, last_synced_at
             FROM sync_cursors WHERE scope_key = ?1",
            [scope_key],
            |row| {
                Ok(SyncCursor {
                    scope_key: row.get(0)?,
                    delta_link: row.get(1)?,
                    history_next_link: row.get(2)?,
                    last_synced_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn upsert_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO sync_cursors (scope_key, delta_link, history_next_link, last_synced_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key) DO UPDATE SET
                 delta_link = excluded.delta_link,
                 history_next_link = excluded.history_next_link,
                 last_synced_at = excluded.last_synced_at",
            params![
                cursor.scope_key,
                cursor.delta_link,
                cursor.history_next_link,
                cursor.last_synced_at,
            ],
        )?;
        Ok(())
    }

    pub fn save_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        self.upsert_sync_cursor(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OnenoteBlock, OnenoteBlockKind, OnenoteScope, RichSpan};

    #[test]
    fn test_file_backed_reader_pool_concurrent_with_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("onenote-concurrent.db");
        let storage = Storage::open(&db_file).unwrap();

        let nb = OnenoteNotebook {
            notebook_key: "me|nb-1".to_string(),
            id: "nb-1".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Engineering".to_string(),
            color: Some("#7719AA".to_string()),
            is_default: true,
            is_shared: false,
            user_role: Some("Owner".to_string()),
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        };
        storage.upsert_onenote_notebooks(&[nb]).unwrap();

        let sec = OnenoteSection {
            section_key: "me|sec-1".to_string(),
            id: "sec-1".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Architecture".to_string(),
            color: Some("#0078D4".to_string()),
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        };
        storage.upsert_onenote_sections(&[sec]).unwrap();

        let summary = OnenotePageSummary {
            page_key: "me|p-1".to_string(),
            id: "p-1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Architecture".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "WAL Concurrency Spec".to_string(),
            level: 0,
            order: 0,
            created_at: Some("2026-11-18T10:00:00Z".to_string()),
            last_modified_at: Some("2026-11-18T12:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: Some("Concurrent WAL reader verification".to_string()),
            has_todo: false,
        };
        let detail = OnenotePageDetail {
            summary,
            raw_xhtml: "<html><body><p>Concurrent WAL reader verification</p></body></html>"
                .to_string(),
            body_markdown: "Concurrent WAL reader verification".to_string(),
            body_plain: "Concurrent WAL reader verification".to_string(),
            blocks: vec![OnenoteBlock {
                element_id: Some("p:1".to_string()),
                tag: None,
                raw_html: None,
                kind: OnenoteBlockKind::Paragraph {
                    spans: vec![RichSpan::plain("Concurrent WAL reader verification")],
                },
            }],
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: Some("2026-11-18T12:00:00Z".to_string()),
        };
        storage.upsert_onenote_page_detail(&detail).unwrap();

        {
            let _writer_guard = storage.conn().lock().unwrap();
            std::thread::scope(|s| {
                for _ in 0..6 {
                    s.spawn(|| {
                        let notebooks = storage.list_onenote_notebooks().unwrap();
                        assert_eq!(notebooks.len(), 1);
                        let pages = storage.list_onenote_pages("me|sec-1").unwrap();
                        assert_eq!(pages.len(), 1);
                        let hits = storage.search_onenote_pages("verification", None, 10).unwrap();
                        assert_eq!(hits.len(), 1);
                        assert_eq!(hits[0].page_id, "p-1");
                    });
                }
            });
        }

        let pooled_count = storage.readers.as_ref().unwrap().lock().unwrap().len();
        assert!((1..=MAX_READER_POOL_SIZE).contains(&pooled_count));

        let ro = Storage::open_readonly(&db_file).unwrap();
        assert_eq!(
            ro.get_onenote_page_detail("me|p-1").unwrap().unwrap().summary.title,
            "WAL Concurrency Spec"
        );
        storage.shrink_memory();

        let cursor = SyncCursor {
            scope_key: "onenote:hierarchy".to_string(),
            delta_link: None,
            history_next_link: None,
            last_synced_at: "2026-11-18T12:00:00Z".to_string(),
        };
        storage.upsert_sync_cursor(&cursor).unwrap();
        assert_eq!(
            storage.get_sync_cursor("onenote:hierarchy").unwrap(),
            Some(cursor)
        );
    }
}
