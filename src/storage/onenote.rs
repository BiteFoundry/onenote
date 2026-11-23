//! SQLite persistence and FTS5 (`trigram` + `LIKE` hybrid) search for Microsoft OneNote.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

use crate::{
    models::{
        OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteScope, OnenoteSearchHit,
        OnenoteSection, OnenoteSectionGroup,
    },
    storage::escape_like,
};

const PAGE_SUMMARY_COLS: &str = "
    page_key, id, scope_json, scope_key, section_id, section_name,
    notebook_id, notebook_name, title, level, page_order,
    created_at, last_modified_at, content_url, web_url, preview_text, has_todo
";

fn normalized_notebook_key(nb: &OnenoteNotebook) -> String {
    if nb.notebook_key.trim().is_empty() {
        nb.scope.make_entity_key(&nb.id)
    } else {
        nb.notebook_key.clone()
    }
}

fn normalized_section_group_key(sg: &OnenoteSectionGroup) -> String {
    if sg.section_group_key.trim().is_empty() {
        sg.scope.make_entity_key(&sg.id)
    } else {
        sg.section_group_key.clone()
    }
}

fn normalized_section_key(sec: &OnenoteSection) -> String {
    if sec.section_key.trim().is_empty() {
        sec.scope.make_entity_key(&sec.id)
    } else {
        sec.section_key.clone()
    }
}

fn normalized_page_key(item: &OnenotePageSummary) -> String {
    if item.page_key.trim().is_empty() {
        item.scope.make_entity_key(&item.id)
    } else {
        item.page_key.clone()
    }
}

fn upsert_onenote_notebooks_in_tx(
    tx: &Transaction<'_>,
    items: &[OnenoteNotebook],
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "
        INSERT INTO onenote_notebooks (
            notebook_key, id, scope_json, scope_key, display_name, color,
            is_default, is_shared, user_role, created_at, last_modified_at,
            created_by, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11,
            ?12, ?13
        )
        ON CONFLICT(notebook_key) DO UPDATE SET
            id = excluded.id,
            scope_json = excluded.scope_json,
            scope_key = excluded.scope_key,
            display_name = excluded.display_name,
            color = COALESCE(excluded.color, onenote_notebooks.color),
            is_default = excluded.is_default,
            is_shared = excluded.is_shared,
            user_role = COALESCE(excluded.user_role, onenote_notebooks.user_role),
            created_at = COALESCE(excluded.created_at, onenote_notebooks.created_at),
            last_modified_at = COALESCE(excluded.last_modified_at, onenote_notebooks.last_modified_at),
            created_by = COALESCE(excluded.created_by, onenote_notebooks.created_by),
            web_url = COALESCE(excluded.web_url, onenote_notebooks.web_url)
        ",
    )?;

    for nb in items {
        let scope_key = nb.scope.scope_key();
        let scope_json = serde_json::to_string(&nb.scope)?;
        let notebook_key = normalized_notebook_key(nb);
        stmt.execute(params![
            notebook_key,
            nb.id,
            scope_json,
            scope_key,
            nb.display_name,
            nb.color,
            bool_to_i32(nb.is_default),
            bool_to_i32(nb.is_shared),
            nb.user_role,
            nb.created_at,
            nb.last_modified_at,
            nb.created_by,
            nb.web_url,
        ])?;
    }
    Ok(())
}

pub fn upsert_onenote_notebooks(conn: &mut Connection, items: &[OnenoteNotebook]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_onenote_notebooks_in_tx(&tx, items)?;
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_notebooks(conn: &Connection) -> Result<Vec<OnenoteNotebook>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            notebook_key, id, scope_json, scope_key, display_name, color,
            is_default, is_shared, user_role, created_at, last_modified_at,
            created_by, web_url
        FROM onenote_notebooks
        ORDER BY
            CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
            is_default DESC,
            display_name COLLATE NOCASE ASC,
            notebook_key ASC
        ",
    )?;
    let rows = stmt.query_map([], map_notebook_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn upsert_onenote_section_groups_in_tx(
    tx: &Transaction<'_>,
    items: &[OnenoteSectionGroup],
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "
        INSERT INTO onenote_section_groups (
            section_group_key, id, scope_json, scope_key, notebook_id,
            parent_section_group_id, display_name, created_at, last_modified_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(section_group_key) DO UPDATE SET
            id = excluded.id,
            scope_json = excluded.scope_json,
            scope_key = excluded.scope_key,
            notebook_id = COALESCE(excluded.notebook_id, onenote_section_groups.notebook_id),
            parent_section_group_id = COALESCE(
                excluded.parent_section_group_id,
                onenote_section_groups.parent_section_group_id
            ),
            display_name = excluded.display_name,
            created_at = COALESCE(excluded.created_at, onenote_section_groups.created_at),
            last_modified_at = COALESCE(
                excluded.last_modified_at,
                onenote_section_groups.last_modified_at
            )
        ",
    )?;

    for sg in items {
        let scope_key = sg.scope.scope_key();
        let scope_json = serde_json::to_string(&sg.scope)?;
        let section_group_key = normalized_section_group_key(sg);
        stmt.execute(params![
            section_group_key,
            sg.id,
            scope_json,
            scope_key,
            sg.notebook_id,
            sg.parent_section_group_id,
            sg.display_name,
            sg.created_at,
            sg.last_modified_at,
        ])?;
    }
    Ok(())
}

pub fn upsert_onenote_section_groups(
    conn: &mut Connection,
    items: &[OnenoteSectionGroup],
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_onenote_section_groups_in_tx(&tx, items)?;
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_section_groups(conn: &Connection) -> Result<Vec<OnenoteSectionGroup>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            section_group_key, id, scope_json, scope_key, notebook_id,
            parent_section_group_id, display_name, created_at, last_modified_at
        FROM onenote_section_groups
        ORDER BY
            CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
            display_name COLLATE NOCASE ASC,
            section_group_key ASC
        ",
    )?;
    let rows = stmt.query_map([], map_section_group_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn upsert_onenote_sections_in_tx(
    tx: &Transaction<'_>,
    items: &[OnenoteSection],
    clear_parent_section_group: bool,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let sql = if clear_parent_section_group {
        "
        INSERT INTO onenote_sections (
            section_key, id, scope_json, scope_key, notebook_id, notebook_name,
            parent_section_group_id, parent_section_group_name, display_name, color,
            is_default, pages_url, created_at, last_modified_at, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            COALESCE(
                ?6,
                (SELECT display_name FROM onenote_notebooks WHERE notebook_key = ?4 || '|' || COALESCE(?5, ''))
            ),
            ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15
        )
        ON CONFLICT(section_key) DO UPDATE SET
            id = excluded.id,
            scope_json = excluded.scope_json,
            scope_key = excluded.scope_key,
            notebook_id = COALESCE(excluded.notebook_id, onenote_sections.notebook_id),
            notebook_name = COALESCE(excluded.notebook_name, onenote_sections.notebook_name),
            parent_section_group_id = excluded.parent_section_group_id,
            parent_section_group_name = excluded.parent_section_group_name,
            display_name = excluded.display_name,
            color = COALESCE(excluded.color, onenote_sections.color),
            is_default = excluded.is_default,
            pages_url = COALESCE(excluded.pages_url, onenote_sections.pages_url),
            created_at = COALESCE(excluded.created_at, onenote_sections.created_at),
            last_modified_at = COALESCE(excluded.last_modified_at, onenote_sections.last_modified_at),
            web_url = COALESCE(excluded.web_url, onenote_sections.web_url)
        "
    } else {
        "
        INSERT INTO onenote_sections (
            section_key, id, scope_json, scope_key, notebook_id, notebook_name,
            parent_section_group_id, parent_section_group_name, display_name, color,
            is_default, pages_url, created_at, last_modified_at, web_url
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            COALESCE(
                ?6,
                (SELECT display_name FROM onenote_notebooks WHERE notebook_key = ?4 || '|' || COALESCE(?5, ''))
            ),
            ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15
        )
        ON CONFLICT(section_key) DO UPDATE SET
            id = excluded.id,
            scope_json = excluded.scope_json,
            scope_key = excluded.scope_key,
            notebook_id = COALESCE(excluded.notebook_id, onenote_sections.notebook_id),
            notebook_name = COALESCE(excluded.notebook_name, onenote_sections.notebook_name),
            parent_section_group_id = COALESCE(
                excluded.parent_section_group_id,
                onenote_sections.parent_section_group_id
            ),
            parent_section_group_name = COALESCE(
                excluded.parent_section_group_name,
                onenote_sections.parent_section_group_name
            ),
            display_name = excluded.display_name,
            color = COALESCE(excluded.color, onenote_sections.color),
            is_default = excluded.is_default,
            pages_url = COALESCE(excluded.pages_url, onenote_sections.pages_url),
            created_at = COALESCE(excluded.created_at, onenote_sections.created_at),
            last_modified_at = COALESCE(excluded.last_modified_at, onenote_sections.last_modified_at),
            web_url = COALESCE(excluded.web_url, onenote_sections.web_url)
        "
    };
    let mut stmt = tx.prepare(sql)?;

    for sec in items {
        let scope_key = sec.scope.scope_key();
        let scope_json = serde_json::to_string(&sec.scope)?;
        let section_key = normalized_section_key(sec);
        stmt.execute(params![
            section_key,
            sec.id,
            scope_json,
            scope_key,
            sec.notebook_id,
            sec.notebook_name,
            sec.parent_section_group_id,
            sec.parent_section_group_name,
            sec.display_name,
            sec.color,
            bool_to_i32(sec.is_default),
            sec.pages_url,
            sec.created_at,
            sec.last_modified_at,
            sec.web_url,
        ])?;
    }
    Ok(())
}

pub fn upsert_onenote_sections(conn: &mut Connection, items: &[OnenoteSection]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_onenote_sections_in_tx(&tx, items, false)?;
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_sections(
    conn: &Connection,
    notebook_key_filter: Option<&str>,
) -> Result<Vec<OnenoteSection>> {
    let filter = notebook_key_filter
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(nb_filter) = filter {
        let mut stmt = conn.prepare(
            "
            SELECT
                section_key, id, scope_json, scope_key, notebook_id, notebook_name,
                parent_section_group_id, parent_section_group_name, display_name, color,
                is_default, pages_url, created_at, last_modified_at, web_url
            FROM onenote_sections
            WHERE (scope_key || '|' || COALESCE(notebook_id, '')) = ?1
               OR notebook_id = ?1
               OR LOWER(COALESCE(notebook_name, '')) = LOWER(?1)
            ORDER BY
                CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
                is_default DESC,
                display_name COLLATE NOCASE ASC,
                section_key ASC
            ",
        )?;
        let rows = stmt.query_map(params![nb_filter], map_section_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare(
            "
            SELECT
                section_key, id, scope_json, scope_key, notebook_id, notebook_name,
                parent_section_group_id, parent_section_group_name, display_name, color,
                is_default, pages_url, created_at, last_modified_at, web_url
            FROM onenote_sections
            ORDER BY
                CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
                is_default DESC,
                display_name COLLATE NOCASE ASC,
                section_key ASC
            ",
        )?;
        let rows = stmt.query_map([], map_section_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

#[derive(PartialEq)]
struct PageSearchFields {
    section_key: String,
    title: String,
    notebook_name: String,
    section_name: String,
    // None represents a retained body. A list upsert can only retain or clear body_plain.
    preview: Option<String>,
}

fn map_page_search_fields(row: &rusqlite::Row<'_>) -> rusqlite::Result<PageSearchFields> {
    Ok(PageSearchFields {
        section_key: row.get(0)?,
        title: row.get(1)?,
        notebook_name: row.get(2)?,
        section_name: row.get(3)?,
        preview: row.get(4)?,
    })
}

fn upsert_onenote_pages_in_tx(
    tx: &Transaction<'_>,
    items: &[OnenotePageSummary],
    preserve_detail_key: Option<&str>,
) -> Result<Vec<String>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let page_keys: Vec<String> = items.iter().map(normalized_page_key).collect();
    let mut indexed_keys: HashSet<String> = {
        // FTS page_key is unindexed. Check the whole batch in a single scan of just that column.
        let mut stmt = tx.prepare(
            "SELECT page_key FROM onenote_fts WHERE page_key IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt.query_map(params![serde_json::to_string(&page_keys)?], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut existing_stmt = tx.prepare(
        "SELECT section_key, title, COALESCE(notebook_name, ''), COALESCE(section_name, ''),
                CASE
                    WHEN body_plain IS NULL THEN COALESCE(preview_text, '')
                    WHEN ?2 IS NOT NULL AND ?2 IS NOT last_modified_at THEN
                        CASE WHEN body_plain = COALESCE(?3, '') THEN COALESCE(?3, '') END
                END,
                last_modified_at, detail_json IS NOT NULL
         FROM onenote_pages WHERE page_key = ?1",
    )?;
    let mut page_stmt = tx.prepare(
        "
        INSERT INTO onenote_pages (
            page_key, id, scope_json, scope_key, section_key, section_id,
            section_name, notebook_id, notebook_name, title, level, page_order,
            created_at, last_modified_at, content_url, web_url,
            preview_text, has_todo, detail_json, body_plain
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            COALESCE(?7, (SELECT display_name FROM onenote_sections WHERE section_key = ?5)),
            COALESCE(?8, (SELECT notebook_id FROM onenote_sections WHERE section_key = ?5)),
            COALESCE(?9, (SELECT notebook_name FROM onenote_sections WHERE section_key = ?5)),
            ?10, ?11, ?12,
            ?13, ?14, ?15, ?16,
            ?17, ?18, NULL, NULL
        )
        ON CONFLICT(page_key) DO UPDATE SET
            id = excluded.id,
            scope_json = excluded.scope_json,
            scope_key = excluded.scope_key,
            section_key = excluded.section_key,
            section_id = excluded.section_id,
            section_name = CASE
                WHEN excluded.section_key != onenote_pages.section_key THEN
                    COALESCE(
                        excluded.section_name,
                        (SELECT display_name FROM onenote_sections WHERE section_key = excluded.section_key)
                    )
                ELSE COALESCE(excluded.section_name, onenote_pages.section_name)
            END,
            notebook_id = CASE
                WHEN excluded.section_key != onenote_pages.section_key THEN
                    COALESCE(
                        excluded.notebook_id,
                        (SELECT notebook_id FROM onenote_sections WHERE section_key = excluded.section_key)
                    )
                ELSE COALESCE(excluded.notebook_id, onenote_pages.notebook_id)
            END,
            notebook_name = CASE
                WHEN excluded.section_key != onenote_pages.section_key THEN
                    COALESCE(
                        excluded.notebook_name,
                        (SELECT notebook_name FROM onenote_sections WHERE section_key = excluded.section_key)
                    )
                ELSE COALESCE(excluded.notebook_name, onenote_pages.notebook_name)
            END,
            title = excluded.title,
            level = excluded.level,
            page_order = excluded.page_order,
            created_at = COALESCE(excluded.created_at, onenote_pages.created_at),
            last_modified_at = COALESCE(excluded.last_modified_at, onenote_pages.last_modified_at),
            content_url = COALESCE(excluded.content_url, onenote_pages.content_url),
            web_url = COALESCE(excluded.web_url, onenote_pages.web_url),
            preview_text = CASE
                WHEN ?19 = 0 AND excluded.last_modified_at IS NOT NULL
                     AND (
                         onenote_pages.last_modified_at IS NULL
                         OR excluded.last_modified_at != onenote_pages.last_modified_at
                     )
                THEN excluded.preview_text
                ELSE COALESCE(excluded.preview_text, onenote_pages.preview_text)
            END,
            has_todo = CASE
                WHEN excluded.has_todo != 0 THEN 1
                WHEN ?19 = 0 AND excluded.last_modified_at IS NOT NULL
                     AND (
                         onenote_pages.last_modified_at IS NULL
                         OR excluded.last_modified_at != onenote_pages.last_modified_at
                     )
                THEN 0
                WHEN onenote_pages.detail_json IS NOT NULL THEN onenote_pages.has_todo
                ELSE 0
            END,
            detail_json = CASE
                WHEN ?19 != 0 AND excluded.last_modified_at IS NOT NULL
                     AND json_valid(onenote_pages.detail_json)
                THEN json_set(
                    onenote_pages.detail_json,
                    '$.summary.last_modified_at',
                    excluded.last_modified_at
                )
                WHEN ?19 = 0 AND excluded.last_modified_at IS NOT NULL
                     AND (
                         onenote_pages.last_modified_at IS NULL
                         OR excluded.last_modified_at != onenote_pages.last_modified_at
                     )
                THEN excluded.detail_json
                ELSE COALESCE(excluded.detail_json, onenote_pages.detail_json)
            END,
            body_plain = CASE
                WHEN ?19 = 0 AND excluded.last_modified_at IS NOT NULL
                     AND (
                         onenote_pages.last_modified_at IS NULL
                         OR excluded.last_modified_at != onenote_pages.last_modified_at
                     )
                THEN excluded.body_plain
                ELSE COALESCE(excluded.body_plain, onenote_pages.body_plain)
            END
        RETURNING section_key, title, COALESCE(notebook_name, ''), COALESCE(section_name, ''),
                  CASE WHEN body_plain IS NULL THEN COALESCE(preview_text, '') END
        ",
    )?;

    let mut fts_del_stmt = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
    let mut fts_ins_stmt = tx.prepare(
        "
        INSERT INTO onenote_fts (
            page_key, section_key, title, notebook_name, section_name, body_plain
        )
        SELECT
            page_key,
            section_key,
            title,
            COALESCE(notebook_name, ''),
            COALESCE(section_name, ''),
            COALESCE(body_plain, preview_text, '')
        FROM onenote_pages
        WHERE page_key = ?1
        ",
    )?;

    let mut invalidated_keys = Vec::new();
    for (item, page_key) in items.iter().zip(page_keys) {
        let scope_key = item.scope.scope_key();
        let scope_json = serde_json::to_string(&item.scope)?;
        let section_key = item.scope.make_entity_key(&item.section_id);
        let preview_clean = item
            .preview_text
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let preserve_detail = preserve_detail_key == Some(page_key.as_str());
        let invalidating_mod = item.last_modified_at.as_deref().filter(|_| !preserve_detail);
        let existing = existing_stmt
            .query_row(params![page_key, invalidating_mod, preview_clean], |r| {
                Ok((
                    map_page_search_fields(r)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            })
            .optional()?;
        if existing.as_ref().is_some_and(|(_, existing_mod, had_detail)| {
            *had_detail && invalidating_mod.is_some() && invalidating_mod != existing_mod.as_deref()
        }) {
            invalidated_keys.push(page_key.clone());
        }

        let search_fields = page_stmt.query_row(
            params![
                page_key,
                item.id,
                scope_json,
                scope_key,
                section_key,
                item.section_id,
                item.section_name,
                item.notebook_id,
                item.notebook_name,
                item.title,
                item.level,
                item.order,
                item.created_at,
                item.last_modified_at,
                item.content_url,
                item.web_url,
                preview_clean,
                bool_to_i32(item.has_todo),
                bool_to_i32(preserve_detail),
            ],
            map_page_search_fields,
        )?;

        let was_indexed = indexed_keys.contains(&page_key);
        if !was_indexed || existing.as_ref().map(|(fields, _, _)| fields) != Some(&search_fields) {
            if was_indexed {
                fts_del_stmt.execute(params![page_key])?;
            }
            fts_ins_stmt.execute(params![page_key])?;
            indexed_keys.insert(page_key);
        }
    }
    Ok(invalidated_keys)
}

pub fn upsert_onenote_pages(conn: &mut Connection, items: &[OnenotePageSummary]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    let _ = upsert_onenote_pages_in_tx(&tx, items, None)?;
    tx.commit()?;
    Ok(())
}

/// Atomically upserts the current hierarchy snapshot and prunes stale notebooks,
/// section groups, sections, and orphaned pages + `onenote_fts` entries.
pub fn reconcile_onenote_hierarchy(
    conn: &mut Connection,
    notebooks: &[OnenoteNotebook],
    section_groups: &[OnenoteSectionGroup],
    sections: &[OnenoteSection],
    synced_all_scopes: bool,
) -> Result<()> {
    if synced_all_scopes {
        reconcile_onenote_hierarchy_inner(conn, notebooks, section_groups, sections, None)
    } else {
        let mut scopes = HashSet::new();
        for nb in notebooks {
            scopes.insert(nb.scope.scope_key());
        }
        for sg in section_groups {
            scopes.insert(sg.scope.scope_key());
        }
        for sec in sections {
            scopes.insert(sec.scope.scope_key());
        }
        reconcile_onenote_hierarchy_for_scopes(conn, notebooks, section_groups, sections, &scopes)
    }
}

/// Atomically upserts the current hierarchy snapshot and prunes stale notebooks,
/// section groups, sections, cursors, and orphaned pages ONLY within `complete_scopes`.
/// Any scope absent from `complete_scopes` (e.g. a failed group or unenumerated `site:*` scope)
/// is preserved intact.
pub fn reconcile_onenote_hierarchy_for_scopes(
    conn: &mut Connection,
    notebooks: &[OnenoteNotebook],
    section_groups: &[OnenoteSectionGroup],
    sections: &[OnenoteSection],
    complete_scopes: &HashSet<String>,
) -> Result<()> {
    reconcile_onenote_hierarchy_inner(
        conn,
        notebooks,
        section_groups,
        sections,
        Some(complete_scopes),
    )
}

fn reconcile_onenote_hierarchy_inner(
    conn: &mut Connection,
    notebooks: &[OnenoteNotebook],
    section_groups: &[OnenoteSectionGroup],
    sections: &[OnenoteSection],
    reconciled_scopes: Option<&HashSet<String>>,
) -> Result<()> {
    let tx = conn.transaction()?;

    upsert_onenote_notebooks_in_tx(&tx, notebooks)?;
    upsert_onenote_section_groups_in_tx(&tx, section_groups)?;
    upsert_onenote_sections_in_tx(&tx, sections, true)?;

    let keep_nb_keys: HashSet<String> = notebooks.iter().map(normalized_notebook_key).collect();
    let keep_sg_keys: HashSet<String> = section_groups
        .iter()
        .map(normalized_section_group_key)
        .collect();
    let keep_sec_keys: HashSet<String> = sections.iter().map(normalized_section_key).collect();

    let should_prune = reconciled_scopes.is_none_or(|scopes| !scopes.is_empty());

    if should_prune {
        let scope_in_reconciled = |scope_key: &str| -> bool {
            match reconciled_scopes {
                None => true,
                Some(scopes) => scopes.contains(scope_key),
            }
        };

        // Prune stale notebooks
        let stale_nbs: Vec<String> = {
            let mut stmt = tx.prepare("SELECT notebook_key, scope_key FROM onenote_notebooks")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(k, scope)| scope_in_reconciled(scope) && !keep_nb_keys.contains(k))
                .map(|(k, _)| k)
                .collect()
        };
        if !stale_nbs.is_empty() {
            let mut del = tx.prepare("DELETE FROM onenote_notebooks WHERE notebook_key = ?1")?;
            for k in stale_nbs {
                del.execute(params![k])?;
            }
        }

        // Prune stale section groups
        let stale_sgs: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT section_group_key, scope_key FROM onenote_section_groups")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(k, scope)| scope_in_reconciled(scope) && !keep_sg_keys.contains(k))
                .map(|(k, _)| k)
                .collect()
        };
        if !stale_sgs.is_empty() {
            let mut del =
                tx.prepare("DELETE FROM onenote_section_groups WHERE section_group_key = ?1")?;
            for k in stale_sgs {
                del.execute(params![k])?;
            }
        }

        // Prune stale sections and their per-section sync cursors
        let stale_secs: Vec<String> = {
            let mut stmt = tx.prepare("SELECT section_key, scope_key FROM onenote_sections")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(k, scope)| scope_in_reconciled(scope) && !keep_sec_keys.contains(k))
                .map(|(k, _)| k)
                .collect()
        };
        if !stale_secs.is_empty() {
            let mut del_sec = tx.prepare("DELETE FROM onenote_sections WHERE section_key = ?1")?;
            let mut del_cur = tx.prepare("DELETE FROM sync_cursors WHERE scope_key = ?1")?;
            for k in stale_secs {
                del_sec.execute(params![k])?;
                let cursor_key = format!("onenote:pages:{k}");
                del_cur.execute(params![cursor_key])?;
            }
        }

        // Prune orphaned pages (belonging to removed sections) and their FTS5 entries
        let orphan_pages: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT page_key, section_key, scope_key FROM onenote_pages")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(_, sec_key, scope)| {
                    scope_in_reconciled(scope) && !keep_sec_keys.contains(sec_key)
                })
                .map(|(pk, _, _)| pk)
                .collect()
        };
        if !orphan_pages.is_empty() {
            let mut del_page = tx.prepare("DELETE FROM onenote_pages WHERE page_key = ?1")?;
            let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
            for pk in orphan_pages {
                del_page.execute(params![pk])?;
                del_fts.execute(params![pk])?;
            }
        }
    }

    tx.commit()?;
    Ok(())
}

/// Atomically upserts notebooks and prunes deleted notebooks (plus their sections,
/// section groups, pages, FTS5 entries, and cursors) strictly within `complete_scopes`.
pub fn reconcile_onenote_notebooks_for_scopes(
    conn: &mut Connection,
    notebooks: &[OnenoteNotebook],
    complete_scopes: &HashSet<String>,
) -> Result<()> {
    let tx = conn.transaction()?;
    upsert_onenote_notebooks_in_tx(&tx, notebooks)?;

    if !complete_scopes.is_empty() {
        let keep_nb_keys: HashSet<String> = notebooks.iter().map(normalized_notebook_key).collect();
        let stale_nbs: Vec<(String, String, String)> = {
            let mut stmt =
                tx.prepare("SELECT notebook_key, id, scope_key FROM onenote_notebooks")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(k, _, scope)| complete_scopes.contains(scope) && !keep_nb_keys.contains(k))
                .collect()
        };
        if !stale_nbs.is_empty() {
            let mut del_nb = tx.prepare("DELETE FROM onenote_notebooks WHERE notebook_key = ?1")?;
            let mut del_sg = tx.prepare(
                "DELETE FROM onenote_section_groups WHERE scope_key = ?1 AND notebook_id = ?2",
            )?;
            let mut del_sec = tx.prepare("DELETE FROM onenote_sections WHERE section_key = ?1")?;
            let mut del_cur = tx.prepare("DELETE FROM sync_cursors WHERE scope_key = ?1")?;
            let mut del_page = tx.prepare("DELETE FROM onenote_pages WHERE page_key = ?1")?;
            let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;

            for (nb_key, nb_id, scope_key) in stale_nbs {
                let sec_keys: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT section_key FROM onenote_sections WHERE scope_key = ?1 AND notebook_id = ?2",
                    )?;
                    let rows = stmt.query_map(params![&scope_key, &nb_id], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for sec_key in sec_keys {
                    let page_keys: Vec<String> = {
                        let mut stmt = tx
                            .prepare("SELECT page_key FROM onenote_pages WHERE section_key = ?1")?;
                        let rows = stmt.query_map(params![&sec_key], |r| r.get(0))?;
                        rows.collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for pk in page_keys {
                        del_page.execute(params![&pk])?;
                        del_fts.execute(params![&pk])?;
                    }
                    del_sec.execute(params![&sec_key])?;
                    del_cur.execute(params![format!("onenote:pages:{sec_key}")])?;
                }
                let direct_pages: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT page_key FROM onenote_pages WHERE scope_key = ?1 AND notebook_id = ?2",
                    )?;
                    let rows = stmt.query_map(params![&scope_key, &nb_id], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for pk in direct_pages {
                    del_page.execute(params![&pk])?;
                    del_fts.execute(params![&pk])?;
                }
                del_sg.execute(params![&scope_key, &nb_id])?;
                del_cur.execute(params![format!("onenote:sections:{nb_key}")])?;
                del_nb.execute(params![&nb_key])?;
            }
        }
    }

    tx.commit()?;
    Ok(())
}

/// Atomically upserts section groups & sections and prunes removed sections/groups
/// and orphaned pages/FTS strictly within `complete_scopes`.
pub fn reconcile_onenote_sections_for_scopes(
    conn: &mut Connection,
    section_groups: &[OnenoteSectionGroup],
    sections: &[OnenoteSection],
    complete_scopes: &HashSet<String>,
) -> Result<()> {
    let tx = conn.transaction()?;
    upsert_onenote_section_groups_in_tx(&tx, section_groups)?;
    upsert_onenote_sections_in_tx(&tx, sections, true)?;

    if !complete_scopes.is_empty() {
        let keep_sg_keys: HashSet<String> = section_groups
            .iter()
            .map(normalized_section_group_key)
            .collect();
        let keep_sec_keys: HashSet<String> = sections.iter().map(normalized_section_key).collect();

        let stale_sgs: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT section_group_key, scope_key FROM onenote_section_groups")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(k, scope)| complete_scopes.contains(scope) && !keep_sg_keys.contains(k))
                .map(|(k, _)| k)
                .collect()
        };
        if !stale_sgs.is_empty() {
            let mut del =
                tx.prepare("DELETE FROM onenote_section_groups WHERE section_group_key = ?1")?;
            for k in stale_sgs {
                del.execute(params![k])?;
            }
        }

        let stale_secs: Vec<String> = {
            let mut stmt = tx.prepare("SELECT section_key, scope_key FROM onenote_sections")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|(k, scope)| complete_scopes.contains(scope) && !keep_sec_keys.contains(k))
                .map(|(k, _)| k)
                .collect()
        };
        if !stale_secs.is_empty() {
            let mut del_sec = tx.prepare("DELETE FROM onenote_sections WHERE section_key = ?1")?;
            let mut del_cur = tx.prepare("DELETE FROM sync_cursors WHERE scope_key = ?1")?;
            let mut del_page = tx.prepare("DELETE FROM onenote_pages WHERE page_key = ?1")?;
            let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
            for k in stale_secs {
                let page_keys: Vec<String> = {
                    let mut stmt =
                        tx.prepare("SELECT page_key FROM onenote_pages WHERE section_key = ?1")?;
                    let rows = stmt.query_map(params![&k], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for pk in page_keys {
                    del_page.execute(params![&pk])?;
                    del_fts.execute(params![&pk])?;
                }
                del_sec.execute(params![&k])?;
                del_cur.execute(params![format!("onenote:pages:{k}")])?;
            }
        }
    }

    tx.commit()?;
    Ok(())
}

/// Atomically upserts sections for a single notebook and, when `is_complete` is true,
/// prunes sections belonging to that notebook that are absent from `sections`, along with
/// their cursors, pages, and FTS5 entries.
pub fn reconcile_onenote_notebook_sections(
    conn: &mut Connection,
    notebook_key: &str,
    sections: &[OnenoteSection],
    is_complete: bool,
) -> Result<()> {
    let trimmed = notebook_key.trim();
    if trimmed.is_empty() {
        return upsert_onenote_sections(conn, sections);
    }
    let tx = conn.transaction()?;
    upsert_onenote_sections_in_tx(&tx, sections, is_complete)?;

    if is_complete {
        let keep_sec_keys: HashSet<String> = sections.iter().map(normalized_section_key).collect();
        let stale_secs: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT section_key FROM onenote_sections WHERE (scope_key || '|' || COALESCE(notebook_id, '')) = ?1",
            )?;
            let rows = stmt.query_map(params![trimmed], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|k| !keep_sec_keys.contains(k))
                .collect()
        };
        if !stale_secs.is_empty() {
            let mut del_sec = tx.prepare("DELETE FROM onenote_sections WHERE section_key = ?1")?;
            let mut del_cur = tx.prepare("DELETE FROM sync_cursors WHERE scope_key = ?1")?;
            let mut del_page = tx.prepare("DELETE FROM onenote_pages WHERE page_key = ?1")?;
            let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
            for k in stale_secs {
                let page_keys: Vec<String> = {
                    let mut stmt =
                        tx.prepare("SELECT page_key FROM onenote_pages WHERE section_key = ?1")?;
                    let rows = stmt.query_map(params![&k], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for pk in page_keys {
                    del_page.execute(params![&pk])?;
                    del_fts.execute(params![&pk])?;
                }
                del_sec.execute(params![&k])?;
                del_cur.execute(params![format!("onenote:pages:{k}")])?;
            }
        }
    }

    tx.commit()?;
    Ok(())
}

/// Atomically upserts pages for a section and, when `is_complete_section_list` is true,
/// deletes removed/moved pages (and their `onenote_fts` entries) for that section.
pub fn reconcile_onenote_section_pages(
    conn: &mut Connection,
    section_key: &str,
    pages: &[OnenotePageSummary],
    is_complete_section_list: bool,
) -> Result<()> {
    let _ = reconcile_onenote_section_pages_returning_invalidated(
        conn,
        section_key,
        pages,
        is_complete_section_list,
    )?;
    Ok(())
}

/// Same as `reconcile_onenote_section_pages`, returning `page_key`s whose previously-hydrated
/// `detail_json` was invalidated by a `last_modified_at` change.
pub fn reconcile_onenote_section_pages_returning_invalidated(
    conn: &mut Connection,
    section_key: &str,
    pages: &[OnenotePageSummary],
    is_complete_section_list: bool,
) -> Result<Vec<String>> {
    reconcile_onenote_section_pages_preserving(
        conn,
        section_key,
        pages,
        is_complete_section_list,
        None,
        None,
    )
}

/// Same as `reconcile_onenote_section_pages_returning_invalidated`, optionally preserving
/// `preserve_page_key` from pruning, and the last good body of `preserve_detail_key` from
/// invalidation while a replacement body is still pending.
pub fn reconcile_onenote_section_pages_preserving(
    conn: &mut Connection,
    section_key: &str,
    pages: &[OnenotePageSummary],
    is_complete_section_list: bool,
    preserve_page_key: Option<&str>,
    preserve_detail_key: Option<&str>,
) -> Result<Vec<String>> {
    let trimmed = section_key.trim();
    if trimmed.is_empty() {
        let tx = conn.transaction()?;
        let invalidated = upsert_onenote_pages_in_tx(&tx, pages, preserve_detail_key)?;
        tx.commit()?;
        return Ok(invalidated);
    }

    let canonical_section_key = if trimmed.contains('|') {
        trimmed.to_string()
    } else {
        OnenoteScope::Me.make_entity_key(trimmed)
    };

    let tx = conn.transaction()?;
    let invalidated = upsert_onenote_pages_in_tx(&tx, pages, preserve_detail_key)?;

    if is_complete_section_list {
        prune_onenote_section_pages_in_tx(&tx, &canonical_section_key, pages, preserve_page_key)?;
    }

    tx.commit()?;
    Ok(invalidated)
}

fn prune_onenote_section_pages_in_tx(
    tx: &Transaction<'_>,
    canonical_section_key: &str,
    pages: &[OnenotePageSummary],
    preserve_page_key: Option<&str>,
) -> Result<()> {
    let keep_page_keys: HashSet<String> = pages.iter().map(normalized_page_key).collect();
    let stale_pages: Vec<String> = list_onenote_page_keys(tx, canonical_section_key)?
        .into_iter()
        .filter(|pk| !keep_page_keys.contains(pk) && preserve_page_key != Some(pk.as_str()))
        .collect();
    if !stale_pages.is_empty() {
        let mut del_page = tx.prepare("DELETE FROM onenote_pages WHERE page_key = ?1")?;
        let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
        for pk in stale_pages {
            del_page.execute(params![pk])?;
            del_fts.execute(params![pk])?;
        }
    }
    Ok(())
}

/// Prunes cached pages (and FTS5 entries) still pointing at `section_key` that are absent from `pages`.
pub fn prune_onenote_section_pages(
    conn: &mut Connection,
    section_key: &str,
    pages: &[OnenotePageSummary],
) -> Result<()> {
    let trimmed = section_key.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let canonical_section_key = if trimmed.contains('|') {
        trimmed.to_string()
    } else {
        OnenoteScope::Me.make_entity_key(trimmed)
    };
    let tx = conn.transaction()?;
    prune_onenote_section_pages_in_tx(&tx, &canonical_section_key, pages, None)?;
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_page_keys(conn: &Connection, section_key: &str) -> Result<Vec<String>> {
    let trimmed = section_key.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let (sql, args) = if trimmed.contains('|') {
        (
            "SELECT page_key FROM onenote_pages WHERE section_key = ?1",
            vec![trimmed.to_owned()],
        )
    } else {
        (
            "SELECT page_key FROM onenote_pages WHERE section_key = ?1 OR section_id = ?2",
            vec![OnenoteScope::Me.make_entity_key(trimmed), trimmed.to_owned()],
        )
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub fn list_onenote_pages(conn: &Connection, section_key: &str) -> Result<Vec<OnenotePageSummary>> {
    let trimmed = section_key.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.contains('|') {
        let sql = format!(
            "
            SELECT {PAGE_SUMMARY_COLS}
            FROM onenote_pages
            WHERE section_key = ?1
            ORDER BY page_order ASC, COALESCE(last_modified_at, '') DESC, id ASC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![trimmed], map_page_summary_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let me_key = OnenoteScope::Me.make_entity_key(trimmed);
        let sql = format!(
            "
            SELECT {PAGE_SUMMARY_COLS}
            FROM onenote_pages
            WHERE section_key = ?1 OR section_id = ?2
            ORDER BY page_order ASC, COALESCE(last_modified_at, '') DESC, id ASC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![me_key, trimmed], map_page_summary_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn upsert_onenote_page_detail(conn: &mut Connection, detail: &OnenotePageDetail) -> Result<()> {
    let mut normalized = detail.clone();
    if normalized.summary.page_key.trim().is_empty() {
        normalized.summary.page_key = normalized
            .summary
            .scope
            .make_entity_key(&normalized.summary.id);
    }

    let page_key = normalized.summary.page_key.clone();
    let scope_key = normalized.summary.scope.scope_key();
    let scope_json = serde_json::to_string(&normalized.summary.scope)?;
    let section_key = normalized
        .summary
        .scope
        .make_entity_key(&normalized.summary.section_id);
    let preview_text = normalized
        .summary
        .preview_text
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| derive_plain_preview(&normalized.body_plain, 140));
    let has_todo = normalized.summary.has_todo
        || normalized
            .blocks
            .iter()
            .any(|b| b.tag.as_ref().map(|t| t.is_todo()).unwrap_or(false));
    let detail_json = serde_json::to_string(&normalized)?;

    let tx = conn.transaction()?;
    {
        tx.execute(
            "
            INSERT INTO onenote_pages (
                page_key, id, scope_json, scope_key, section_key, section_id,
                section_name, notebook_id, notebook_name, title, level, page_order,
                created_at, last_modified_at, content_url, web_url,
                preview_text, has_todo, detail_json, body_plain
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                COALESCE(?7, (SELECT display_name FROM onenote_sections WHERE section_key = ?5)),
                COALESCE(?8, (SELECT notebook_id FROM onenote_sections WHERE section_key = ?5)),
                COALESCE(?9, (SELECT notebook_name FROM onenote_sections WHERE section_key = ?5)),
                ?10, ?11, ?12,
                ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20
            )
            ON CONFLICT(page_key) DO UPDATE SET
                id = excluded.id,
                scope_json = excluded.scope_json,
                scope_key = excluded.scope_key,
                section_key = excluded.section_key,
                section_id = excluded.section_id,
                section_name = CASE
                    WHEN excluded.section_key != onenote_pages.section_key THEN
                        COALESCE(
                            excluded.section_name,
                            (SELECT display_name FROM onenote_sections WHERE section_key = excluded.section_key)
                        )
                    ELSE COALESCE(excluded.section_name, onenote_pages.section_name)
                END,
                notebook_id = CASE
                    WHEN excluded.section_key != onenote_pages.section_key THEN
                        COALESCE(
                            excluded.notebook_id,
                            (SELECT notebook_id FROM onenote_sections WHERE section_key = excluded.section_key)
                        )
                    ELSE COALESCE(excluded.notebook_id, onenote_pages.notebook_id)
                END,
                notebook_name = CASE
                    WHEN excluded.section_key != onenote_pages.section_key THEN
                        COALESCE(
                            excluded.notebook_name,
                            (SELECT notebook_name FROM onenote_sections WHERE section_key = excluded.section_key)
                        )
                    ELSE COALESCE(excluded.notebook_name, onenote_pages.notebook_name)
                END,
                title = excluded.title,
                level = excluded.level,
                page_order = excluded.page_order,
                created_at = COALESCE(excluded.created_at, onenote_pages.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, onenote_pages.last_modified_at),
                content_url = COALESCE(excluded.content_url, onenote_pages.content_url),
                web_url = COALESCE(excluded.web_url, onenote_pages.web_url),
                preview_text = excluded.preview_text,
                has_todo = excluded.has_todo,
                detail_json = excluded.detail_json,
                body_plain = excluded.body_plain
            ",
            params![
                page_key,
                normalized.summary.id,
                scope_json,
                scope_key,
                section_key,
                normalized.summary.section_id,
                normalized.summary.section_name,
                normalized.summary.notebook_id,
                normalized.summary.notebook_name,
                normalized.summary.title,
                normalized.summary.level,
                normalized.summary.order,
                normalized.summary.created_at,
                normalized.summary.last_modified_at,
                normalized.summary.content_url,
                normalized.summary.web_url,
                preview_text,
                bool_to_i32(has_todo),
                detail_json,
                normalized.body_plain,
            ],
        )?;

        tx.execute("DELETE FROM onenote_fts WHERE page_key = ?1", params![page_key])?;
        tx.execute(
            "
            INSERT INTO onenote_fts (
                page_key, section_key, title, notebook_name, section_name, body_plain
            )
            SELECT
                page_key,
                section_key,
                title,
                COALESCE(notebook_name, ''),
                COALESCE(section_name, ''),
                COALESCE(body_plain, preview_text, '')
            FROM onenote_pages
            WHERE page_key = ?1
            ",
            params![page_key],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn get_onenote_page_detail(
    conn: &Connection,
    page_key_or_id: &str,
) -> Result<Option<OnenotePageDetail>> {
    let key = page_key_or_id.trim();
    if key.is_empty() {
        return Ok(None);
    }

    type DetailRow = (
        Option<String>,
        String,
        i32,
        i32,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    let row_opt: Option<DetailRow> = conn
        .query_row(
            "
            SELECT detail_json, title, level, page_order, last_modified_at,
                   section_id, section_name, notebook_id, notebook_name, preview_text
            FROM onenote_pages
            WHERE page_key = ?1
            ",
            params![key],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()?;

    let row_data = match row_opt {
        Some(data) => Some(data),
        None => conn
            .query_row(
                "
                SELECT detail_json, title, level, page_order, last_modified_at,
                       section_id, section_name, notebook_id, notebook_name, preview_text
                FROM onenote_pages
                WHERE id = ?1
                ORDER BY
                    CASE WHEN detail_json IS NOT NULL THEN 0 ELSE 1 END ASC,
                    COALESCE(last_modified_at, '') DESC
                LIMIT 1
                ",
                params![key],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .optional()?,
    };

    let Some((
        Some(detail_json),
        row_title,
        row_level,
        row_order,
        row_modified,
        row_section_id,
        row_section_name,
        row_notebook_id,
        row_notebook_name,
        row_preview_text,
    )) = row_data
    else {
        return Ok(None);
    };

    let mut detail: OnenotePageDetail = serde_json::from_str(&detail_json)?;
    if let Some(row_mod) = row_modified.as_deref() {
        if detail.summary.last_modified_at.as_deref() != Some(row_mod) {
            return Ok(None);
        }
    }
    detail.summary.title = row_title;
    detail.summary.level = row_level;
    detail.summary.order = row_order;
    if !row_section_id.trim().is_empty() {
        detail.summary.section_id = row_section_id;
    }
    detail.summary.section_name = row_section_name;
    detail.summary.notebook_id = row_notebook_id;
    detail.summary.notebook_name = row_notebook_name;
    if row_modified.is_some() {
        detail.summary.last_modified_at = row_modified;
    }
    if row_preview_text.is_some() {
        detail.summary.preview_text = row_preview_text;
    }
    Ok(Some(detail))
}

pub fn get_onenote_page_summary(
    conn: &Connection,
    page_key_or_id: &str,
) -> Result<Option<OnenotePageSummary>> {
    let key = page_key_or_id.trim();
    if key.is_empty() {
        return Ok(None);
    }

    let exact_sql = format!(
        "
        SELECT {PAGE_SUMMARY_COLS}
        FROM onenote_pages
        WHERE page_key = ?1
        "
    );
    if let Some(found) = conn
        .query_row(&exact_sql, params![key], map_page_summary_row)
        .optional()?
    {
        return Ok(Some(found));
    }

    let fallback_sql = format!(
        "
        SELECT {PAGE_SUMMARY_COLS}
        FROM onenote_pages
        WHERE id = ?1
        ORDER BY COALESCE(last_modified_at, '') DESC, page_key ASC
        LIMIT 1
        "
    );
    conn.query_row(&fallback_sql, params![key], map_page_summary_row)
        .optional()
        .map_err(Into::into)
}

pub fn delete_onenote_page(conn: &mut Connection, page_key_or_id: &str) -> Result<()> {
    let key = page_key_or_id.trim();
    if key.is_empty() {
        return Ok(());
    }

    let matched_keys: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT page_key FROM onenote_pages WHERE page_key = ?1 OR id = ?1",
        )?;
        let rows = stmt.query_map(params![key], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let tx = conn.transaction()?;
    {
        tx.execute(
            "DELETE FROM onenote_pages WHERE page_key = ?1 OR id = ?1",
            params![key],
        )?;
        let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
        del_fts.execute(params![key])?;
        for pk in matched_keys {
            del_fts.execute(params![pk])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn count_onenote_pages(conn: &Connection) -> Result<(u64, u64)> {
    let (pages, pages_with_body): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(detail_json IS NOT NULL), 0) FROM onenote_pages",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((pages.max(0) as u64, pages_with_body.max(0) as u64))
}

pub fn search_onenote_pages(
    conn: &Connection,
    query: &str,
    section_key_filter: Option<&str>,
    limit: usize,
    trigram_enabled: bool,
) -> Result<Vec<OnenoteSearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let section_filter = section_key_filter
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if (trigram_enabled && trimmed.chars().count() >= 3)
        || (!trigram_enabled && trimmed.chars().count() >= 2)
    {
        if let Some(fts_query) = build_fts_query(trimmed) {
            if let Ok(hits) =
                run_onenote_fts_search(conn, &fts_query, trimmed, section_filter, limit)
            {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
    }

    run_onenote_like_search(conn, trimmed, section_filter, limit)
}

fn run_onenote_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    section_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<OnenoteSearchHit>> {
    let cols = "
        p.page_key, p.id, p.scope_json, p.scope_key, p.notebook_name,
        p.section_id, p.section_name, p.title, p.last_modified_at,
        COALESCE(p.body_plain, ''), COALESCE(p.preview_text, ''), p.web_url
    ";
    if let Some(sec) = section_filter {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_fts f
            JOIN onenote_pages p ON p.page_key = f.page_key
            WHERE onenote_fts MATCH ?1
              AND (p.section_key = ?2 OR p.section_id = ?2)
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![fts_query, sec, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_fts f
            JOIN onenote_pages p ON p.page_key = f.page_key
            WHERE onenote_fts MATCH ?1
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn run_onenote_like_search(
    conn: &Connection,
    raw_query: &str,
    section_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<OnenoteSearchHit>> {
    let cols = "
        p.page_key, p.id, p.scope_json, p.scope_key, p.notebook_name,
        p.section_id, p.section_name, p.title, p.last_modified_at,
        COALESCE(p.body_plain, ''), COALESCE(p.preview_text, ''), p.web_url
    ";
    let pattern = format!("%{}%", escape_like(&raw_query.to_lowercase()));
    if let Some(sec) = section_filter {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_pages p
            WHERE (p.section_key = ?1 OR p.section_id = ?1)
              AND (
                  LOWER(p.title) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.body_plain, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.preview_text, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.notebook_name, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.section_name, '')) LIKE ?2 ESCAPE '\\'
              )
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![sec, pattern, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_pages p
            WHERE (
                LOWER(p.title) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.body_plain, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.preview_text, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.notebook_name, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.section_name, '')) LIKE ?1 ESCAPE '\\'
            )
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn build_fts_query(trimmed: &str) -> Option<String> {
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() > 1 && tokens.iter().all(|t| t.chars().count() >= 3) {
        let quoted: Vec<String> = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" "))
    } else {
        Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
    }
}

fn parse_scope_from_row(scope_json: &str, scope_key: &str) -> OnenoteScope {
    serde_json::from_str::<OnenoteScope>(scope_json)
        .ok()
        .or_else(|| OnenoteScope::from_scope_key(scope_key, None))
        .unwrap_or(OnenoteScope::Me)
}

fn map_notebook_row(row: &Row<'_>) -> rusqlite::Result<OnenoteNotebook> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let is_default_int: i32 = row.get(6)?;
    let is_shared_int: i32 = row.get(7)?;
    Ok(OnenoteNotebook {
        notebook_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        display_name: row.get(4)?,
        color: row.get(5)?,
        is_default: is_default_int != 0,
        is_shared: is_shared_int != 0,
        user_role: row.get(8)?,
        created_at: row.get(9)?,
        last_modified_at: row.get(10)?,
        created_by: row.get(11)?,
        web_url: row.get(12)?,
    })
}

fn map_section_group_row(row: &Row<'_>) -> rusqlite::Result<OnenoteSectionGroup> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    Ok(OnenoteSectionGroup {
        section_group_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        notebook_id: row.get(4)?,
        parent_section_group_id: row.get(5)?,
        display_name: row.get(6)?,
        created_at: row.get(7)?,
        last_modified_at: row.get(8)?,
    })
}

fn map_section_row(row: &Row<'_>) -> rusqlite::Result<OnenoteSection> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let is_default_int: i32 = row.get(10)?;
    Ok(OnenoteSection {
        section_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        notebook_id: row.get(4)?,
        notebook_name: row.get(5)?,
        parent_section_group_id: row.get(6)?,
        parent_section_group_name: row.get(7)?,
        display_name: row.get(8)?,
        color: row.get(9)?,
        is_default: is_default_int != 0,
        pages_url: row.get(11)?,
        created_at: row.get(12)?,
        last_modified_at: row.get(13)?,
        web_url: row.get(14)?,
    })
}

fn map_page_summary_row(row: &Row<'_>) -> rusqlite::Result<OnenotePageSummary> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let has_todo_int: i32 = row.get(16)?;
    Ok(OnenotePageSummary {
        page_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
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
        has_todo: has_todo_int != 0,
    })
}

fn map_onenote_search_hit_row(
    row: &Row<'_>,
    raw_query: &str,
) -> rusqlite::Result<OnenoteSearchHit> {
    let page_key: String = row.get(0)?;
    let page_id: String = row.get(1)?;
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let notebook_name: Option<String> = row.get(4)?;
    let section_id: String = row.get(5)?;
    let section_name: Option<String> = row.get(6)?;
    let title: String = row.get(7)?;
    let last_modified_at: Option<String> = row.get(8)?;
    let body_plain: String = row.get(9)?;
    let preview_text: String = row.get(10)?;
    let web_url: Option<String> = row.get(11)?;

    let body_source = if body_plain.trim().is_empty() {
        preview_text.as_str()
    } else {
        body_plain.as_str()
    };
    let snippet = build_onenote_snippet(
        &title,
        body_source,
        section_name.as_deref(),
        notebook_name.as_deref(),
        raw_query,
        120,
    );

    Ok(OnenoteSearchHit {
        page_key,
        page_id,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        notebook_name,
        section_id,
        section_name,
        title,
        last_modified_at,
        snippet,
        web_url,
    })
}

fn build_onenote_snippet(
    title: &str,
    body_plain: &str,
    section_name: Option<&str>,
    notebook_name: Option<&str>,
    query: &str,
    max_chars: usize,
) -> String {
    let clean_body = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean_title = title.trim();
    let source = if !clean_body.is_empty() {
        clean_body
    } else if !clean_title.is_empty() {
        clean_title.to_string()
    } else {
        section_name
            .or(notebook_name)
            .unwrap_or("(No content)")
            .to_string()
    };

    let chars: Vec<char> = source.chars().collect();
    if chars.len() <= max_chars {
        return source;
    }

    let query_lower = query.trim().to_lowercase();
    let match_char_idx = find_case_insensitive_char_pos(&source, &query_lower)
        .or_else(|| {
            query_lower
                .split_whitespace()
                .filter_map(|tok| find_case_insensitive_char_pos(&source, tok))
                .min()
        })
        .unwrap_or(0);

    let half = max_chars / 2;
    let mut start = match_char_idx.saturating_sub(half);
    let end = (start + max_chars).min(chars.len());
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let slice: String = chars[start..end].iter().collect();
    out.push_str(slice.trim());
    if end < chars.len() {
        out.push('…');
    }
    out
}

fn find_case_insensitive_char_pos(haystack: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return None;
    }
    let haystack_lower = haystack.to_lowercase();
    let byte_idx = haystack_lower.find(needle_lower)?;
    Some(haystack_lower[..byte_idx].chars().count())
}

fn derive_plain_preview(body_plain: &str, max_chars: usize) -> Option<String> {
    let single_line = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() {
        return None;
    }
    if single_line.chars().count() <= max_chars {
        Some(single_line)
    } else {
        let truncated: String = single_line.chars().take(max_chars.saturating_sub(1)).collect();
        Some(format!("{truncated}…"))
    }
}

fn bool_to_i32(v: bool) -> i32 {
    if v {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{OnenoteBlock, OnenoteBlockKind, OnenoteResource, OnenoteTagKind, RichSpan},
        storage::Storage,
    };

    fn sample_page_summary(
        scope: OnenoteScope,
        id: &str,
        section_id: &str,
        title: &str,
        order: i32,
        modified_at: &str,
    ) -> OnenotePageSummary {
        OnenotePageSummary {
            page_key: scope.make_entity_key(id),
            id: id.to_string(),
            scope,
            section_id: section_id.to_string(),
            section_name: None,
            notebook_id: None,
            notebook_name: None,
            title: title.to_string(),
            level: 0,
            order,
            created_at: Some("2026-11-15T08:00:00Z".to_string()),
            last_modified_at: Some(modified_at.to_string()),
            content_url: None,
            web_url: Some(format!("https://onenote.example.com/{id}")),
            preview_text: None,
            has_todo: false,
        }
    }

    #[test]
    fn test_onenote_hierarchy_and_detail_preservation_across_summary_refresh() {
        let storage = Storage::open_in_memory().unwrap();
        let team_scope = OnenoteScope::Group {
            group_id: "g-eng".to_string(),
            group_name: Some("엔지니어링팀".to_string()),
        };

        let notebooks = vec![
            OnenoteNotebook {
                notebook_key: "me|nb-1".to_string(),
                id: "nb-1".to_string(),
                scope: OnenoteScope::Me,
                display_name: "개인 업무 노트".to_string(),
                color: Some("#7719AA".to_string()),
                is_default: true,
                is_shared: false,
                user_role: Some("Owner".to_string()),
                created_at: Some("2026-11-01T00:00:00Z".to_string()),
                last_modified_at: Some("2026-11-15T09:00:00Z".to_string()),
                created_by: Some("홍길동".to_string()),
                web_url: Some("https://onenote.example.com/nb-1".to_string()),
            },
            OnenoteNotebook {
                notebook_key: team_scope.make_entity_key("nb-team"),
                id: "nb-team".to_string(),
                scope: team_scope.clone(),
                display_name: "플랫폼 런북".to_string(),
                color: None,
                is_default: false,
                is_shared: true,
                user_role: Some("Contributor".to_string()),
                created_at: None,
                last_modified_at: None,
                created_by: None,
                web_url: None,
            },
        ];
        storage.upsert_onenote_notebooks(&notebooks).unwrap();
        let listed_nbs = storage.list_onenote_notebooks().unwrap();
        assert_eq!(listed_nbs.len(), 2);
        assert_eq!(listed_nbs[0].notebook_key, "me|nb-1");
        assert_eq!(listed_nbs[1].scope.owner_label(), "엔지니어링팀");

        let sgs = vec![OnenoteSectionGroup {
            section_group_key: "me|sg-1".to_string(),
            id: "sg-1".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            parent_section_group_id: None,
            display_name: "4분기 프로젝트".to_string(),
            created_at: None,
            last_modified_at: None,
        }];
        storage.upsert_onenote_section_groups(&sgs).unwrap();
        assert_eq!(storage.list_onenote_section_groups().unwrap().len(), 1);

        let sections = vec![
            OnenoteSection {
                section_key: "me|sec-1".to_string(),
                id: "sec-1".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb-1".to_string()),
                notebook_name: None, // auto-enriched from onenote_notebooks
                parent_section_group_id: Some("sg-1".to_string()),
                parent_section_group_name: Some("4분기 프로젝트".to_string()),
                display_name: "스프린트 회의록".to_string(),
                color: None,
                is_default: true,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
            OnenoteSection {
                section_key: team_scope.make_entity_key("sec-ops"),
                id: "sec-ops".to_string(),
                scope: team_scope.clone(),
                notebook_id: Some("nb-team".to_string()),
                notebook_name: Some("플랫폼 런북".to_string()),
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "장애 대응".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
        ];
        storage.upsert_onenote_sections(&sections).unwrap();

        let all_secs = storage.list_onenote_sections(None).unwrap();
        assert_eq!(all_secs.len(), 2);
        assert_eq!(all_secs[0].notebook_name.as_deref(), Some("개인 업무 노트"));

        let filtered_secs = storage.list_onenote_sections(Some("me|nb-1")).unwrap();
        assert_eq!(filtered_secs.len(), 1);
        assert_eq!(filtered_secs[0].section_key, "me|sec-1");

        // Initial page summaries ordered by (order ASC, last_modified_at DESC)
        let p_second = sample_page_summary(
            OnenoteScope::Me,
            "p-2",
            "sec-1",
            "후속 작업",
            1,
            "2026-11-15T12:00:00Z",
        );
        let p_first_older = sample_page_summary(
            OnenoteScope::Me,
            "p-1a",
            "sec-1",
            "오전 스탠드업",
            0,
            "2026-11-15T09:00:00Z",
        );
        let p_first_newer = sample_page_summary(
            OnenoteScope::Me,
            "p-1b",
            "sec-1",
            "오후 아키텍처 리뷰",
            0,
            "2026-11-15T11:00:00Z",
        );
        storage
            .upsert_onenote_pages(&[p_second, p_first_older, p_first_newer.clone()])
            .unwrap();

        let listed_pages = storage.list_onenote_pages("me|sec-1").unwrap();
        assert_eq!(listed_pages.len(), 3);
        assert_eq!(listed_pages[0].id, "p-1b"); // order 0, newer modified_at
        assert_eq!(listed_pages[1].id, "p-1a"); // order 0, older modified_at
        assert_eq!(listed_pages[2].id, "p-2"); // order 1
        assert_eq!(
            listed_pages[0].section_name.as_deref(),
            Some("스프린트 회의록")
        );
        assert_eq!(
            listed_pages[0].notebook_name.as_deref(),
            Some("개인 업무 노트")
        );

        // Cache full XHTML detail for p-1b
        let mut detail_summary = listed_pages[0].clone();
        detail_summary.preview_text = Some("Kubernetes 클러스터 업그레이드 및 체크리스트".to_string());
        detail_summary.has_todo = true;
        let detail = OnenotePageDetail {
            summary: detail_summary,
            raw_xhtml: "<html><body><p id=\"p:1\" data-tag=\"to-do\">Kubernetes 클러스터 업그레이드</p></body></html>".to_string(),
            body_markdown: "- [ ] Kubernetes 클러스터 업그레이드".to_string(),
            body_plain: "Kubernetes 클러스터 업그레이드 및 체크리스트 검토".to_string(),
            blocks: vec![OnenoteBlock {
                element_id: Some("p:1".to_string()),
                tag: Some(OnenoteTagKind::Todo { completed: false }),
                raw_html: Some("<p id=\"p:1\" data-tag=\"to-do\">Kubernetes 클러스터 업그레이드</p>".to_string()),
                kind: OnenoteBlockKind::Paragraph {
                    spans: vec![RichSpan::plain("Kubernetes 클러스터 업그레이드")],
                },
            }],
            resources: vec![OnenoteResource {
                element_id: Some("img:1".to_string()),
                resource_url: "https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content"
                    .to_string(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("diagram.png".to_string()),
                is_image: true,
                alt_text: Some("다이어그램".to_string()),
            }],
            primary_div_id: Some("div:{1111}{32}".to_string()),
            synced_at: Some("2026-11-15T11:05:00Z".to_string()),
        };
        storage.upsert_onenote_page_detail(&detail).unwrap();

        // Lookup by both composite page_key and raw page_id
        assert_eq!(
            storage
                .get_onenote_page_detail("me|p-1b")
                .unwrap()
                .unwrap(),
            detail
        );
        assert_eq!(
            storage.get_onenote_page_detail("p-1b").unwrap().unwrap(),
            detail
        );

        // Refreshing page summary (with preview_text=None, has_todo=false, new title) MUST NOT wipe detail_json or body_plain!
        let mut refreshed_summary = p_first_newer;
        refreshed_summary.title = "오후 아키텍처 리뷰 (수정됨)".to_string();
        refreshed_summary.preview_text = None;
        refreshed_summary.has_todo = false;
        storage.upsert_onenote_pages(&[refreshed_summary]).unwrap();

        let preserved_summary = storage.get_onenote_page_summary("p-1b").unwrap().unwrap();
        assert_eq!(preserved_summary.title, "오후 아키텍처 리뷰 (수정됨)");
        assert_eq!(
            preserved_summary.preview_text.as_deref(),
            Some("Kubernetes 클러스터 업그레이드 및 체크리스트")
        );
        assert!(preserved_summary.has_todo);

        let preserved_detail = storage
            .get_onenote_page_detail("me|p-1b")
            .unwrap()
            .unwrap();
        assert_eq!(preserved_detail.summary.title, "오후 아키텍처 리뷰 (수정됨)");
        assert_eq!(preserved_detail.blocks.len(), 1);
        assert_eq!(preserved_detail.resources.len(), 1);
        assert_eq!(
            preserved_detail.primary_div_id.as_deref(),
            Some("div:{1111}{32}")
        );
    }

    #[test]
    fn section_reconcile_rewrites_only_changed_or_missing_search_rows() {
        let storage = Storage::open_in_memory().unwrap();
        let stamp = "2026-11-15T09:00:00Z";
        let mut first = sample_page_summary(
            OnenoteScope::Me, "p1", "sec-a", "OriginalTitle", 0, stamp,
        );
        let mut second = sample_page_summary(
            OnenoteScope::Me, "p2", "sec-a", "OtherTitle", 1, stamp,
        );
        second.preview_text = Some("PreviewOnly".into());
        let detail = |summary, text| {
            crate::parser::parse_onenote_page_xhtml(
                summary,
                &format!("<html><body><p>{text}</p></body></html>"),
            )
        };
        storage.upsert_onenote_pages(&[second.clone()]).unwrap();
        storage
            .upsert_onenote_page_detail(&detail(first.clone(), "OriginalBody"))
            .unwrap();
        storage
            .conn()
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TEMP TABLE fts_writes (kind TEXT, page_key TEXT);
                 CREATE TEMP TRIGGER fts_insert AFTER INSERT ON onenote_fts_content BEGIN
                     INSERT INTO fts_writes VALUES ('insert', new.c0);
                 END;
                 CREATE TEMP TRIGGER fts_delete AFTER DELETE ON onenote_fts_content BEGIN
                     INSERT INTO fts_writes VALUES ('delete', old.c0);
                 END;",
            )
            .unwrap();

        let take_writes = || {
            let conn = storage.conn().lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT kind, page_key FROM fts_writes ORDER BY rowid")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            let writes: Vec<_> = rows.collect::<rusqlite::Result<_>>().unwrap();
            conn.execute("DELETE FROM fts_writes", []).unwrap();
            writes
        };
        let matches = |query: &str, section: Option<&str>| {
            storage
                .search_onenote_pages(query, section, 10)
                .unwrap()
                .into_iter()
                .map(|hit| hit.page_key)
                .collect::<Vec<_>>()
        };
        let reconcile = |section, pages: &[OnenotePageSummary]| {
            storage
                .reconcile_onenote_section_pages(section, pages, true)
                .unwrap();
        };
        let replacement = |key: &str| vec![("delete".into(), key.into()), ("insert".into(), key.into())];

        let before = matches("OriginalBody", Some("me|sec-a"));
        assert_eq!(before, ["me|p1"]);
        first.preview_text = Some("Ignored while a body exists".into());
        reconcile("me|sec-a", &[first.clone(), second.clone()]);
        assert!(take_writes().is_empty());
        assert_eq!(matches("OriginalBody", Some("me|sec-a")), before);

        first.title = "RenamedTitle".into();
        first.section_name = Some("RenamedSection".into());
        first.notebook_name = Some("RenamedNotebook".into());
        reconcile("me|sec-a", &[first.clone(), second.clone()]);
        assert_eq!(take_writes(), replacement("me|p1"));
        for query in ["RenamedTitle", "RenamedSection", "RenamedNotebook"] {
            assert_eq!(matches(query, None), ["me|p1"]);
        }
        assert!(matches("OriginalTitle", None).is_empty());

        storage
            .conn()
            .lock()
            .unwrap()
            .execute("DELETE FROM onenote_fts WHERE page_key = 'me|p2'", [])
            .unwrap();
        assert_eq!(take_writes(), [("delete".into(), "me|p2".into())]);
        reconcile("me|sec-a", &[first.clone(), second.clone()]);
        assert_eq!(take_writes(), [("insert".into(), "me|p2".into())]);
        assert_eq!(matches("PreviewOnly", None), ["me|p2"]);

        first.section_id = "sec-b".into();
        reconcile("me|sec-b", &[first.clone()]);
        assert_eq!(take_writes(), replacement("me|p1"));
        assert!(matches("OriginalBody", Some("me|sec-a")).is_empty());
        assert_eq!(matches("OriginalBody", Some("me|sec-b")), ["me|p1"]);

        storage
            .upsert_onenote_page_detail(&detail(first, "EditedBody"))
            .unwrap();
        assert_eq!(take_writes(), replacement("me|p1"));
        assert!(matches("OriginalBody", None).is_empty());
        assert_eq!(matches("EditedBody", None), ["me|p1"]);

        reconcile("me|sec-b", &[]);
        assert_eq!(take_writes(), [("delete".into(), "me|p1".into())]);
        assert!(matches("EditedBody", None).is_empty());
    }

    #[test]
    fn test_onenote_fts5_korean_english_search_and_delete() {
        let storage = Storage::open_in_memory().unwrap();
        let team_scope = OnenoteScope::Group {
            group_id: "g-1".to_string(),
            group_name: Some("인프라팀".to_string()),
        };

        // Page 1: summary-only upsert (title indexed into onenote_fts)
        let mut p1 = sample_page_summary(
            OnenoteScope::Me,
            "pg-budget",
            "sec-fin",
            "2026년도 클라우드 예산 계획",
            0,
            "2026-11-15T09:00:00Z",
        );
        p1.notebook_name = Some("재무 노트".to_string());
        p1.section_name = Some("예산안".to_string());
        storage.upsert_onenote_pages(&[p1]).unwrap();

        // Page 2: full detail upsert in group scope
        let mut p2_sum = sample_page_summary(
            team_scope.clone(),
            "pg-runbook",
            "sec-sre",
            "장애 복구 매뉴얼",
            0,
            "2026-11-15T10:00:00Z",
        );
        p2_sum.notebook_name = Some("SRE 런북".to_string());
        p2_sum.section_name = Some("프로덕션".to_string());
        let p2_detail = OnenotePageDetail {
            summary: p2_sum,
            raw_xhtml: "<html><body><p>PostgreSQL failover 절차 및 복구 명령어 안내</p></body></html>"
                .to_string(),
            body_markdown: "PostgreSQL failover 절차 및 복구 명령어 안내".to_string(),
            body_plain: "PostgreSQL failover 절차 및 복구 명령어 안내".to_string(),
            blocks: Vec::new(),
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: Some("2026-11-15T10:01:00Z".to_string()),
        };
        storage.upsert_onenote_page_detail(&p2_detail).unwrap();

        // >= 3 chars Korean trigram title match on summary-only page
        let hits_ko_title = storage
            .search_onenote_pages("클라우드 예산", None, 10)
            .unwrap();
        assert_eq!(hits_ko_title.len(), 1);
        assert_eq!(hits_ko_title[0].page_key, "me|pg-budget");

        // >= 3 chars English trigram body_plain match
        let hits_en_body = storage
            .search_onenote_pages("postgresql failover", None, 10)
            .unwrap();
        assert_eq!(hits_en_body.len(), 1);
        assert_eq!(hits_en_body[0].page_id, "pg-runbook");
        assert!(hits_en_body[0].snippet.contains("PostgreSQL failover"));

        // < 3 chars 2-character Korean LIKE fallback + section filter
        let hits_two_char = storage
            .search_onenote_pages("예산", Some("me|sec-fin"), 10)
            .unwrap();
        assert_eq!(hits_two_char.len(), 1);
        assert_eq!(hits_two_char[0].page_id, "pg-budget");

        let hits_filtered_out = storage
            .search_onenote_pages("예산", Some("group:g-1|sec-sre"), 10)
            .unwrap();
        assert!(hits_filtered_out.is_empty());

        // Delete removes from both onenote_pages and onenote_fts
        storage.delete_onenote_page("pg-runbook").unwrap();
        assert!(storage
            .get_onenote_page_detail("group:g-1|pg-runbook")
            .unwrap()
            .is_none());
        assert!(storage
            .get_onenote_page_summary("pg-runbook")
            .unwrap()
            .is_none());
        assert!(storage
            .search_onenote_pages("PostgreSQL", None, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_upsert_pages_invalidates_stale_detail_when_last_modified_changes() {
        let storage = Storage::open_in_memory().unwrap();
        let initial_mod = "2026-11-15T10:00:00Z";
        let summary = sample_page_summary(
            OnenoteScope::Me,
            "p-stale",
            "sec-1",
            "Initial Spec",
            0,
            initial_mod,
        );
        let detail = OnenotePageDetail {
            summary: OnenotePageSummary {
                preview_text: Some("Original body text".to_string()),
                has_todo: true,
                ..summary.clone()
            },
            raw_xhtml: "<html><body><p id=\"p:1\" data-tag=\"to-do\">Original body text</p></body></html>".to_string(),
            body_markdown: "- [ ] Original body text".to_string(),
            body_plain: "Original body text".to_string(),
            blocks: vec![OnenoteBlock {
                element_id: Some("p:1".to_string()),
                tag: Some(OnenoteTagKind::Todo { completed: false }),
                raw_html: None,
                kind: OnenoteBlockKind::Paragraph {
                    spans: vec![RichSpan::plain("Original body text")],
                },
            }],
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: Some(initial_mod.to_string()),
        };
        storage.upsert_onenote_page_detail(&detail).unwrap();
        assert!(storage.get_onenote_page_detail("me|p-stale").unwrap().is_some());

        // Same last_modified_at preserves detail_json, body_plain, preview_text, and has_todo.
        let mut same_mod_summary = summary.clone();
        same_mod_summary.title = "Renamed Without Content Edit".to_string();
        storage.upsert_onenote_pages(&[same_mod_summary]).unwrap();
        let preserved = storage
            .get_onenote_page_detail("me|p-stale")
            .unwrap()
            .expect("detail should remain cached when last_modified_at is unchanged");
        assert_eq!(preserved.summary.title, "Renamed Without Content Edit");
        assert_eq!(preserved.body_plain, "Original body text");

        // Newer last_modified_at invalidates stale detail_json & body_plain.
        let mut newer_mod_summary = summary;
        newer_mod_summary.title = "Updated Spec".to_string();
        newer_mod_summary.last_modified_at = Some("2026-11-15T11:00:00Z".to_string());

        // Focused refresh keeps the last good body accessible through the typed cache reader.
        storage
            .reconcile_onenote_section_pages_preserving(
                "me|sec-1",
                std::slice::from_ref(&newer_mod_summary),
                false,
                None,
                Some("me|p-stale"),
            )
            .unwrap();
        let open = storage.get_onenote_page_detail("me|p-stale").unwrap().unwrap();
        assert_eq!(open.summary.last_modified_at, newer_mod_summary.last_modified_at);
        assert_eq!(open.body_plain, detail.body_plain);
        assert_eq!(open.synced_at, detail.synced_at);
        assert_eq!(storage.search_onenote_pages("Original body", None, 10).unwrap().len(), 1);

        storage.upsert_onenote_page_detail(&detail).unwrap();
        storage.upsert_onenote_pages(&[newer_mod_summary]).unwrap();

        assert!(
            storage.get_onenote_page_detail("me|p-stale").unwrap().is_none(),
            "detail must be invalidated when last_modified_at changes"
        );
        let updated_summary = storage.get_onenote_page_summary("me|p-stale").unwrap().unwrap();
        assert_eq!(updated_summary.title, "Updated Spec");
        assert_eq!(
            updated_summary.last_modified_at.as_deref(),
            Some("2026-11-15T11:00:00Z")
        );
        assert!(
            storage
                .search_onenote_pages("Original body", None, 10)
                .unwrap()
                .is_empty(),
            "stale body_plain should be cleared from FTS5 index"
        );
    }

    #[test]
    fn test_reconcile_prunes_deleted_notebooks_sections_and_pages_and_fts() {
        let storage = Storage::open_in_memory().unwrap();
        let nb1 = OnenoteNotebook {
            notebook_key: "me|nb-1".to_string(),
            id: "nb-1".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Active Notebook".to_string(),
            color: None,
            is_default: true,
            is_shared: false,
            user_role: Some("Owner".to_string()),
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        };
        let nb2 = OnenoteNotebook {
            notebook_key: "me|nb-2".to_string(),
            id: "nb-2".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Deleted Notebook".to_string(),
            color: None,
            is_default: false,
            is_shared: false,
            user_role: Some("Owner".to_string()),
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        };
        let sec1 = OnenoteSection {
            section_key: "me|sec-1".to_string(),
            id: "sec-1".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Active Notebook".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Kept Section".to_string(),
            color: None,
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        };
        let sec2 = OnenoteSection {
            section_key: "me|sec-2".to_string(),
            id: "sec-2".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-2".to_string()),
            notebook_name: Some("Deleted Notebook".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Removed Section".to_string(),
            color: None,
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        };
        storage.upsert_onenote_notebooks(&[nb1.clone(), nb2]).unwrap();
        storage.upsert_onenote_sections(&[sec1.clone(), sec2]).unwrap();

        let p1 = sample_page_summary(
            OnenoteScope::Me,
            "p-1",
            "sec-1",
            "Alpha Architecture Roadmap",
            0,
            "2026-11-15T10:00:00Z",
        );
        let p2 = sample_page_summary(
            OnenoteScope::Me,
            "p-2",
            "sec-1",
            "Beta Removed Page",
            1,
            "2026-11-15T10:05:00Z",
        );
        let p3 = sample_page_summary(
            OnenoteScope::Me,
            "p-3",
            "sec-2",
            "Gamma Orphaned Notebook Page",
            0,
            "2026-11-15T10:10:00Z",
        );
        storage
            .upsert_onenote_pages(&[p1.clone(), p2.clone(), p3])
            .unwrap();
        assert_eq!(storage.list_onenote_notebooks().unwrap().len(), 2);
        assert_eq!(storage.list_onenote_sections(None).unwrap().len(), 2);
        assert_eq!(
            storage.search_onenote_pages("Gamma Orphaned", None, 10).unwrap().len(),
            1
        );

        // Reconcile hierarchy down to 1 notebook and 1 section -> prunes nb-2, sec-2, and p-3 (+ FTS)
        storage
            .reconcile_onenote_hierarchy(&[nb1], &[], &[sec1], true)
            .unwrap();
        assert_eq!(storage.list_onenote_notebooks().unwrap().len(), 1);
        assert_eq!(storage.list_onenote_notebooks().unwrap()[0].id, "nb-1");
        assert_eq!(storage.list_onenote_sections(None).unwrap().len(), 1);
        assert_eq!(storage.list_onenote_sections(None).unwrap()[0].id, "sec-1");
        assert!(storage.list_onenote_pages("me|sec-2").unwrap().is_empty());
        assert!(storage
            .search_onenote_pages("Gamma Orphaned", None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(storage.list_onenote_pages("me|sec-1").unwrap().len(), 2);

        // Reconcile section pages on sec-1 with complete list containing only p-1 -> prunes p-2 (+ FTS)
        storage
            .reconcile_onenote_section_pages("me|sec-1", &[p1], true)
            .unwrap();
        let remaining = storage.list_onenote_pages("me|sec-1").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "p-1");
        assert!(storage
            .search_onenote_pages("Beta Removed", None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            storage
                .search_onenote_pages("Alpha Architecture", None, 10)
                .unwrap()
                .len(),
            1
        );
    }
}
