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
    let cached = storage.list_onenote_notebooks()?;
    if !cached.is_empty() {
        match resolve_notebook_from_slice(&cached, query) {
            Ok(found) => return Ok(found),
            Err(err) if prefer_cache => return Err(err),
            Err(_) => {}
        }
    } else if prefer_cache {
        bail!("No cached OneNote notebooks found. Run without --offline or run `onenote sync`.");
    }

    let fetched = graph.list_all_accessible_notebooks(true).await?;
    storage.upsert_onenote_notebooks(&fetched)?;
    resolve_notebook_from_slice(&fetched, query)
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
    let nb_key_filter = if let Some(nb_q) = notebook_filter.filter(|s| !s.trim().is_empty()) {
        let nb = resolve_notebook(graph, storage, nb_q, prefer_cache).await?;
        Some(nb.notebook_key)
    } else {
        None
    };

    let cached = storage.list_onenote_sections(nb_key_filter.as_deref())?;
    if !cached.is_empty() {
        match resolve_section_from_slice(&cached, section_query) {
            Ok(found) => return Ok(found),
            Err(err) if prefer_cache => return Err(err),
            Err(_) => {}
        }
    } else if prefer_cache {
        bail!("No cached OneNote sections found. Run without --offline or run `onenote sync`.");
    }

    let mut notebooks = storage.list_onenote_notebooks()?;
    if notebooks.is_empty() {
        notebooks = graph.list_all_accessible_notebooks(true).await?;
        storage.upsert_onenote_notebooks(&notebooks)?;
    }
    let (groups, fetched) = graph.list_all_accessible_sections(&notebooks).await?;
    if !groups.is_empty() {
        storage.upsert_onenote_section_groups(&groups)?;
    }
    storage.upsert_onenote_sections(&fetched)?;
    let scoped = storage.list_onenote_sections(nb_key_filter.as_deref())?;
    resolve_section_from_slice(&scoped, section_query)
}

/// Resolves a OneNote page against an in-memory slice of `OnenotePageSummary`s.
pub fn resolve_page_from_slice(
    pages: &[OnenotePageSummary],
    query: &str,
) -> Result<OnenotePageSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("OneNote page ID or title is empty.");
    }
    if let Some(exact) = pages.iter().find(|p| p.page_key == q || p.id == q) {
        return Ok(exact.clone());
    }
    let id_prefix: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.id.starts_with(q) || p.page_key.starts_with(q))
        .collect();
    if id_prefix.len() == 1 {
        return Ok(id_prefix[0].clone());
    } else if id_prefix.len() > 1 {
        bail!(
            "Ambiguous page ID prefix '{q}' matches {} pages.",
            id_prefix.len()
        );
    }

    let q_lower = q.to_lowercase();
    let title_exact: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.title.trim().to_lowercase() == q_lower)
        .collect();
    if title_exact.len() == 1 {
        return Ok(title_exact[0].clone());
    } else if title_exact.len() > 1 {
        bail!(
            "Ambiguous page title '{q}' matches {} pages:\n{}",
            title_exact.len(),
            format_page_candidates(&title_exact)
        );
    }

    let title_prefix: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.title.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if title_prefix.len() == 1 {
        return Ok(title_prefix[0].clone());
    }

    let title_sub: Vec<&OnenotePageSummary> = pages
        .iter()
        .filter(|p| p.title.to_lowercase().contains(&q_lower))
        .collect();
    if title_sub.len() == 1 {
        return Ok(title_sub[0].clone());
    } else if title_sub.len() > 1 {
        bail!(
            "Ambiguous page '{q}' matches {} pages:\n{}",
            title_sub.len(),
            format_page_candidates(&title_sub)
        );
    }

    bail!("OneNote page not found: {q}")
}

/// Resolves a OneNote page from cache (optionally scoped to a section) or falls back to treating `query` as a direct page key/ID.
pub fn resolve_page_cached(
    storage: &Storage,
    query: &str,
    section_key: Option<&str>,
) -> Option<OnenotePageSummary> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if let Ok(Some(exact)) = storage.get_onenote_page_summary(q) {
        return Some(exact);
    }
    let candidates = match section_key {
        Some(sk) if !sk.trim().is_empty() => storage.list_onenote_pages(sk).ok()?,
        _ => storage.list_all_onenote_pages(500).ok()?,
    };
    resolve_page_from_slice(&candidates, q).ok()
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
        .map(|p| format!("  - {} ({})", p.title, p.page_key))
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
}
