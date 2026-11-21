//! Smart entity resolver for CLI, HTTP API, and MCP.
//!
//! Allows users and AI agents to pass either exact Microsoft Graph IDs / composite keys
//! (`me|...`, `group:...|...`, `site:...|...`) OR human-friendly display names and prefixes
//! for OneNote notebooks, sections, and pages.

use anyhow::{bail, Result};

use crate::{
    graph::GraphClient,
    models::{OnenoteNotebook, OnenotePageSummary, OnenoteSection},
    storage::Storage,
};

/// Resolves a OneNote notebook query against an in-memory slice of `OnenoteNotebook`s.
///
/// Priority:
/// 1. Empty / `"default"` -> default notebook (`is_default`) or first personal notebook
/// 2. Exact `notebook_key` or exact `id` match
/// 3. Unique case-insensitive exact `display_name` match
/// 4. Unique case-insensitive `display_name` prefix match
/// 5. Unique case-insensitive `display_name` substring match
pub fn resolve_notebook_from_slice(
    notebooks: &[OnenoteNotebook],
    query: &str,
) -> Result<OnenoteNotebook> {
    let q = query.trim();
    if notebooks.is_empty() {
        bail!("No OneNote notebooks found.");
    }
    if q.is_empty() || q.eq_ignore_ascii_case("default") {
        if let Some(def) = notebooks.iter().find(|n| n.is_default && n.scope.is_personal()) {
            return Ok(def.clone());
        }
        if let Some(def) = notebooks.iter().find(|n| n.is_default) {
            return Ok(def.clone());
        }
        return Ok(notebooks[0].clone());
    }

    if let Some(exact) = notebooks
        .iter()
        .find(|n| n.notebook_key == q || n.id == q)
    {
        return Ok(exact.clone());
    }

    let q_lower = q.to_lowercase();
    let exact_name: Vec<&OnenoteNotebook> = notebooks
        .iter()
        .filter(|n| n.display_name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous notebook '{q}' matches multiple notebooks:\n{}",
            format_notebook_candidates(&exact_name)
        );
    }

    let prefix_matches: Vec<&OnenoteNotebook> = notebooks
        .iter()
        .filter(|n| n.display_name.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous notebook prefix '{q}' matches multiple notebooks:\n{}",
            format_notebook_candidates(&prefix_matches)
        );
    }

    let sub_matches: Vec<&OnenoteNotebook> = notebooks
        .iter()
        .filter(|n| n.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous notebook '{q}' matches {n} notebooks:\n{}",
            format_notebook_candidates(&sub_matches)
        ),
        _ => bail!("OneNote notebook not found: {q}"),
    }
}

/// Resolves a OneNote notebook from local SQLite cache or refreshes from Microsoft Graph.
pub async fn resolve_notebook(
    graph: &GraphClient,
    storage: &Storage,
    query: &str,
    prefer_cache: bool,
) -> Result<OnenoteNotebook> {
    resolve_notebook_with_refresh(graph, storage, query, prefer_cache, false).await
}

/// Resolves a OneNote notebook, optionally bypassing the local cache to refresh from Graph first.
pub async fn resolve_notebook_with_refresh(
    graph: &GraphClient,
    storage: &Storage,
    query: &str,
    prefer_cache: bool,
    force_refresh: bool,
) -> Result<OnenoteNotebook> {
    if !force_refresh || prefer_cache {
        let cached = storage.list_onenote_notebooks()?;
        if !cached.is_empty() {
            match resolve_notebook_from_slice(&cached, query) {
                Ok(found) => return Ok(found),
                Err(err) if prefer_cache => return Err(err),
                Err(_) => {}
            }
        } else if prefer_cache {
            bail!(
                "No cached OneNote notebooks found. Run without --offline or run `onenote sync`."
            );
        }
    }

    let fetched = graph.list_all_accessible_notebooks_scoped(true).await?;
    storage.reconcile_onenote_notebooks_for_scopes(&fetched.notebooks, &fetched.complete_scopes)?;
    resolve_notebook_from_slice(&fetched.notebooks, query)
}

/// Resolves a OneNote section query against an in-memory slice of `OnenoteSection`s.
pub fn resolve_section_from_slice(
    sections: &[OnenoteSection],
    query: &str,
) -> Result<OnenoteSection> {
    let q = query.trim();
    if sections.is_empty() {
        bail!("No OneNote sections found.");
    }
    if q.is_empty() || q.eq_ignore_ascii_case("default") {
        if let Some(def) = sections.iter().find(|s| s.is_default && s.scope.is_personal()) {
            return Ok(def.clone());
        }
        if let Some(def) = sections.iter().find(|s| s.is_default) {
            return Ok(def.clone());
        }
        return Ok(sections[0].clone());
    }

    if let Some(exact) = sections
        .iter()
        .find(|s| s.section_key == q || s.id == q)
    {
        return Ok(exact.clone());
    }

    let id_prefix: Vec<&OnenoteSection> = sections
        .iter()
        .filter(|s| s.id.starts_with(q) || s.section_key.starts_with(q))
        .collect();
    if id_prefix.len() == 1 {
        return Ok(id_prefix[0].clone());
    }

    let q_lower = q.to_lowercase();
    let exact_name: Vec<&OnenoteSection> = sections
        .iter()
        .filter(|s| s.display_name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous section '{q}' matches multiple sections:\n{}",
            format_section_candidates(&exact_name)
        );
    }

    let prefix_matches: Vec<&OnenoteSection> = sections
        .iter()
        .filter(|s| s.display_name.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous section prefix '{q}' matches multiple sections:\n{}",
            format_section_candidates(&prefix_matches)
        );
    }

    let sub_matches: Vec<&OnenoteSection> = sections
        .iter()
        .filter(|s| s.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous section '{q}' matches {n} sections:\n{}",
            format_section_candidates(&sub_matches)
        ),
        _ => bail!("OneNote section not found: {q}"),
    }
}

/// Resolves a OneNote section from local SQLite cache or refreshes from Microsoft Graph.
pub async fn resolve_section(
    graph: &GraphClient,
    storage: &Storage,
    section_query: &str,
    notebook_filter: Option<&str>,
    prefer_cache: bool,
) -> Result<OnenoteSection> {
    resolve_section_with_refresh(
        graph,
        storage,
        section_query,
        notebook_filter,
        prefer_cache,
        false,
    )
    .await
}

/// Resolves a OneNote section, optionally refreshing notebooks and sections from Graph first.
pub async fn resolve_section_with_refresh(
    graph: &GraphClient,
    storage: &Storage,
    section_query: &str,
    notebook_filter: Option<&str>,
    prefer_cache: bool,
    force_refresh: bool,
) -> Result<OnenoteSection> {
    let nb_key_filter = if let Some(nb_q) = notebook_filter.filter(|s| !s.trim().is_empty()) {
        let nb =
            resolve_notebook_with_refresh(graph, storage, nb_q, prefer_cache, force_refresh)
                .await?;
        Some(nb.notebook_key)
    } else {
        None
    };

    if !force_refresh || prefer_cache {
        let cached = storage.list_onenote_sections(nb_key_filter.as_deref())?;
        if !cached.is_empty() {
            match resolve_section_from_slice(&cached, section_query) {
                Ok(found) => return Ok(found),
                Err(err) if prefer_cache => return Err(err),
                Err(_) => {}
            }
        } else if prefer_cache {
            bail!(
                "No cached OneNote sections found. Run without --offline or run `onenote sync`."
            );
        }
    }

    let nb_res = graph.list_all_accessible_notebooks_scoped(true).await?;
    let sec_res = graph
        .list_all_accessible_sections_scoped(&nb_res.notebooks, Some(&nb_res.complete_scopes))
        .await?;
    let reconciled_scopes: std::collections::HashSet<String> = nb_res
        .complete_scopes
        .intersection(&sec_res.complete_scopes)
        .cloned()
        .collect();
    if !reconciled_scopes.is_empty() {
        storage.reconcile_onenote_hierarchy_for_scopes(
            &nb_res.notebooks,
            &sec_res.section_groups,
            &sec_res.sections,
            &reconciled_scopes,
        )?;
    } else {
        storage.reconcile_onenote_notebooks_for_scopes(&nb_res.notebooks, &nb_res.complete_scopes)?;
        storage.reconcile_onenote_sections_for_scopes(
            &sec_res.section_groups,
            &sec_res.sections,
            &sec_res.complete_scopes,
        )?;
    }

    let scoped = storage.list_onenote_sections(nb_key_filter.as_deref())?;
    resolve_section_from_slice(&scoped, section_query)
}

/// Outcome of resolving a OneNote page query against cached summaries.
#[derive(Debug, Clone)]
pub enum PageResolution {
    Found(OnenotePageSummary),
    NotFound,
    Ambiguous {
        message: String,
        candidates: Vec<OnenotePageSummary>,
    },
}

impl PageResolution {
    /// Converts `Found` into `Ok(Some(_))`, `NotFound` into `Ok(None)`,
    /// and propagates `Ambiguous` as an error so callers never fall back to using
    /// an ambiguous title/prefix as a raw remote page ID.
    pub fn into_option(self) -> Result<Option<OnenotePageSummary>> {
        match self {
            Self::Found(page) => Ok(Some(page)),
            Self::NotFound => Ok(None),
            Self::Ambiguous { message, .. } => bail!("{message}"),
        }
    }
}

/// Resolves a OneNote page against an in-memory slice, distinguishing
/// unique match, no match, and multiple ambiguous candidates.
pub fn resolve_page_in_slice(
    pages: &[OnenotePageSummary],
    query: &str,
) -> Result<PageResolution> {
    let q = query.trim();
    if q.is_empty() {
        bail!("OneNote page ID or title is empty.");
    }
    if let Some(exact) = pages.iter().find(|p| p.page_key == q || p.id == q) {
        return Ok(PageResolution::Found(exact.clone()));
    }
    let id_prefix: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.id.starts_with(q) || p.page_key.starts_with(q))
        .collect();
    if id_prefix.len() == 1 {
        return Ok(PageResolution::Found(id_prefix[0].clone()));
    } else if id_prefix.len() > 1 {
        let message = format!(
            "Ambiguous page ID prefix '{q}' matches {} pages:\n{}",
            id_prefix.len(),
            format_page_candidates(&id_prefix)
        );
        return Ok(PageResolution::Ambiguous {
            message,
            candidates: id_prefix.into_iter().cloned().collect(),
        });
    }

    let q_lower = q.to_lowercase();
    let title_exact: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.title.trim().to_lowercase() == q_lower)
        .collect();
    if title_exact.len() == 1 {
        return Ok(PageResolution::Found(title_exact[0].clone()));
    } else if title_exact.len() > 1 {
        let message = format!(
            "Ambiguous page title '{q}' matches {} pages:\n{}",
            title_exact.len(),
            format_page_candidates(&title_exact)
        );
        return Ok(PageResolution::Ambiguous {
            message,
            candidates: title_exact.into_iter().cloned().collect(),
        });
    }

    let title_prefix: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.title.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if title_prefix.len() == 1 {
        return Ok(PageResolution::Found(title_prefix[0].clone()));
    } else if title_prefix.len() > 1 {
        let message = format!(
            "Ambiguous page prefix '{q}' matches {} pages:\n{}",
            title_prefix.len(),
            format_page_candidates(&title_prefix)
        );
        return Ok(PageResolution::Ambiguous {
            message,
            candidates: title_prefix.into_iter().cloned().collect(),
        });
    }

    let title_sub: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.title.to_lowercase().contains(&q_lower))
        .collect();
    if title_sub.len() == 1 {
        return Ok(PageResolution::Found(title_sub[0].clone()));
    } else if title_sub.len() > 1 {
        let message = format!(
            "Ambiguous page '{q}' matches {} pages:\n{}",
            title_sub.len(),
            format_page_candidates(&title_sub)
        );
        return Ok(PageResolution::Ambiguous {
            message,
            candidates: title_sub.into_iter().cloned().collect(),
        });
    }

    Ok(PageResolution::NotFound)
}

/// Resolves a OneNote page against an in-memory slice of `OnenotePageSummary`s.
pub fn resolve_page_from_slice(
    pages: &[OnenotePageSummary],
    query: &str,
) -> Result<OnenotePageSummary> {
    let q = query.trim();
    match resolve_page_in_slice(pages, q)? {
        PageResolution::Found(page) => Ok(page),
        PageResolution::Ambiguous { message, .. } => bail!("{message}"),
        PageResolution::NotFound => bail!("OneNote page not found: {q}"),
    }
}

/// Resolves a OneNote page from cache (optionally scoped to a section),
/// distinguishing `Found`, `NotFound`, and `Ambiguous`.
pub fn resolve_page_cached(
    storage: &Storage,
    query: &str,
    section_key: Option<&str>,
) -> Result<PageResolution> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(PageResolution::NotFound);
    }
    let section_filter = section_key.map(str::trim).filter(|s| !s.is_empty());
    if section_filter.is_none() {
        if let Some(exact) = storage.get_onenote_page_summary(q)? {
            return Ok(PageResolution::Found(exact));
        }
    }
    let candidates = match section_filter {
        Some(sk) => storage.list_onenote_pages(sk)?,
        None => storage.list_all_onenote_pages(500)?,
    };
    resolve_page_in_slice(&candidates, q)
}

fn format_notebook_candidates(candidates: &[&OnenoteNotebook]) -> String {
    const MAX_LIST: usize = 8;
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_LIST)
        .map(|n| format!("  - {} ({})", n.display_name, n.notebook_key))
        .collect();
    if candidates.len() > MAX_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_LIST));
    }
    lines.join("\n")
}

fn format_section_candidates(candidates: &[&OnenoteSection]) -> String {
    const MAX_LIST: usize = 8;
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_LIST)
        .map(|s| {
            let nb = s.notebook_name.as_deref().unwrap_or("Notebook");
            format!("  - {} › {} ({})", nb, s.display_name, s.section_key)
        })
        .collect();
    if candidates.len() > MAX_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_LIST));
    }
    lines.join("\n")
}

fn format_page_candidates(candidates: &[&OnenotePageSummary]) -> String {
    const MAX_LIST: usize = 8;
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_LIST)
        .map(|p| {
            let section = p
                .section_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let id = p.section_id.trim();
                    if id.is_empty() {
                        None
                    } else {
                        Some(id)
                    }
                });
            match section {
                Some(sec) => format!("  - {} [{}] ({})", p.title, sec, p.page_key),
                None => format!("  - {} ({})", p.title, p.page_key),
            }
        })
        .collect();
    if candidates.len() > MAX_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_LIST));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::OnenoteScope;

    fn sample_nb(id: &str, name: &str, is_default: bool) -> OnenoteNotebook {
        OnenoteNotebook {
            notebook_key: format!("me|{id}"),
            id: id.to_string(),
            scope: OnenoteScope::Me,
            display_name: name.to_string(),
            color: None,
            is_default,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        }
    }

    fn sample_sec(id: &str, name: &str, is_default: bool) -> OnenoteSection {
        OnenoteSection {
            section_key: format!("me|{id}"),
            id: id.to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Work".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: name.to_string(),
            color: None,
            is_default,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        }
    }

    #[test]
    fn resolves_notebooks_and_sections_by_id_key_default_name_and_prefix() {
        let notebooks = vec![
            sample_nb("nb-work", "Engineering Work", false),
            sample_nb("nb-def", "Personal Notes", true),
        ];
        assert_eq!(
            resolve_notebook_from_slice(&notebooks, "default").unwrap().id,
            "nb-def"
        );
        assert_eq!(
            resolve_notebook_from_slice(&notebooks, "me|nb-work").unwrap().id,
            "nb-work"
        );
        assert_eq!(
            resolve_notebook_from_slice(&notebooks, "eng").unwrap().id,
            "nb-work"
        );

        let sections = vec![
            sample_sec("sec-1", "Architecture", false),
            sample_sec("sec-2", "Quick Notes", true),
        ];
        assert_eq!(
            resolve_section_from_slice(&sections, "").unwrap().id,
            "sec-2"
        );
        assert_eq!(
            resolve_section_from_slice(&sections, "arch").unwrap().id,
            "sec-1"
        );
        assert!(resolve_section_from_slice(&sections, "missing").is_err());
    }

    fn sample_page(id: &str, title: &str, sec_id: &str, sec_name: &str) -> OnenotePageSummary {
        OnenotePageSummary {
            page_key: format!("me|{id}"),
            id: id.to_string(),
            scope: OnenoteScope::Me,
            section_id: sec_id.to_string(),
            section_name: Some(sec_name.to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: title.to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        }
    }

    #[test]
    fn regression_onenote_bug_08_resolve_page_in_slice_distinguishes_ambiguous_and_missing() {
        let pages = vec![
            sample_page("page-a", "Standup notes", "sec-1", "Alpha"),
            sample_page("page-b", "Standup notes", "sec-2", "Beta"),
            sample_page("page-u", "Quarterly Roadmap", "sec-1", "Alpha"),
        ];

        match resolve_page_in_slice(&pages, "Standup notes").unwrap() {
            PageResolution::Ambiguous { message, candidates } => {
                assert_eq!(candidates.len(), 2);
                assert!(message.contains("Standup notes"));
                assert!(message.contains("Alpha"));
                assert!(message.contains("Beta"));
                assert!(message.contains("me|page-a"));
                assert!(message.contains("me|page-b"));
            }
            other => panic!("expected Ambiguous for exact duplicate title, got {other:?}"),
        }

        match resolve_page_in_slice(&pages, "Standup").unwrap() {
            PageResolution::Ambiguous { message, candidates } => {
                assert_eq!(candidates.len(), 2);
                assert!(message.contains("me|page-a") && message.contains("me|page-b"));
            }
            other => panic!("expected Ambiguous for prefix duplicate, got {other:?}"),
        }

        match resolve_page_in_slice(&pages, "me|page-a").unwrap() {
            PageResolution::Found(p) => assert_eq!(p.page_key, "me|page-a"),
            other => panic!("expected Found for scoped key, got {other:?}"),
        }

        match resolve_page_in_slice(&pages, "Roadmap").unwrap() {
            PageResolution::Found(p) => assert_eq!(p.page_key, "me|page-u"),
            other => panic!("expected Found for unique substring, got {other:?}"),
        }

        assert!(matches!(
            resolve_page_in_slice(&pages, "remote-only-id").unwrap(),
            PageResolution::NotFound
        ));
    }
}
