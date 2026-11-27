//! Shared OneNote read & cache-hydration helpers (used by CLI, HTTP API, and MCP).

use anyhow::{bail, Result};

use crate::{
    graph::GraphClient,
    models::{
        split_onenote_entity_key, OnenoteNotebook, OnenotePageDetail, OnenotePageSummary,
        OnenoteSection,
    },
    resolver,
    storage::Storage,
};

use super::runtime::{
    is_cursor_fresh, page_slice, record_sync_cursor, CURSOR_HIERARCHY, CURSOR_NOTEBOOKS_ALL,
    CURSOR_NOTEBOOKS_PERSONAL, CURSOR_SECTIONS_ALL,
};

pub async fn load_onenote_notebooks(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
) -> Result<Vec<OnenoteNotebook>> {
    load_onenote_notebooks_with_cache(storage, graph, include_teams, refresh, offline, false).await
}

pub async fn load_onenote_notebooks_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<Vec<OnenoteNotebook>> {
    Ok(
        load_onenote_notebooks_with_warnings(storage, graph, include_teams, refresh, offline, cached)
            .await?
            .0,
    )
}

/// Like [`load_onenote_notebooks_with_cache`], plus warnings about notebooks
/// that may be missing (Teams skipped for this sign-in, Teams cap reached,
/// Teams listing failed).
pub async fn load_onenote_notebooks_with_warnings(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(Vec<OnenoteNotebook>, Vec<String>)> {
    let mut warnings = Vec::new();
    let notebooks = load_onenote_notebooks_inner(
        storage,
        graph,
        include_teams,
        refresh,
        offline,
        cached,
        &mut warnings,
    )
    .await?;
    // A cache hit makes no Graph call, so repeat the sign-in hint here: it is
    // derived from the token alone and explains why Teams notebooks are absent.
    if include_teams && !offline {
        let tokens = graph.auth().current_tokens().await;
        if let Some(hint) = crate::graph::onenote::teams_skipped_warning(tokens.as_ref()) {
            if !warnings.iter().any(|w| w == hint) {
                warnings.push(hint.to_string());
            }
        }
    }
    Ok((notebooks, warnings))
}

async fn load_onenote_notebooks_inner(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<OnenoteNotebook>> {
    let filter_nbs = |items: Vec<OnenoteNotebook>| -> Vec<OnenoteNotebook> {
        if include_teams {
            items
        } else {
            items.into_iter().filter(|n| n.scope.is_personal()).collect()
        }
    };

    if offline {
        return Ok(filter_nbs(storage.list_onenote_notebooks()?));
    }

    if !refresh {
        let cached_all = storage.list_onenote_notebooks()?;
        let filtered = filter_nbs(cached_all);
        if !filtered.is_empty() {
            if cached {
                let teams_synced = !include_teams
                    || storage.get_sync_cursor(CURSOR_NOTEBOOKS_ALL).ok().flatten().is_some()
                    || storage.get_sync_cursor(CURSOR_HIERARCHY).ok().flatten().is_some()
                    || filtered.iter().any(|n| !n.scope.is_personal());
                if teams_synced {
                    return Ok(filtered);
                }
            } else {
                let ttl = graph.config().cli_cache_ttl_secs;
                let fresh = if include_teams {
                    is_cursor_fresh(storage, CURSOR_NOTEBOOKS_ALL, ttl)
                        || is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl)
                } else {
                    is_cursor_fresh(storage, CURSOR_NOTEBOOKS_PERSONAL, ttl)
                        || is_cursor_fresh(storage, CURSOR_NOTEBOOKS_ALL, ttl)
                        || is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl)
                };
                if fresh {
                    return Ok(filtered);
                }
            }
        }
    }

    let nb_res = graph
        .list_all_accessible_notebooks_scoped(include_teams)
        .await?;
    storage.reconcile_onenote_notebooks_for_scopes(&nb_res.notebooks, &nb_res.complete_scopes)?;
    record_sync_cursor(storage, CURSOR_NOTEBOOKS_PERSONAL, None);
    if include_teams {
        record_sync_cursor(storage, CURSOR_NOTEBOOKS_ALL, None);
    }
    warnings.extend(nb_res.warnings);
    Ok(nb_res.notebooks)
}

pub async fn load_onenote_sections(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>)> {
    load_onenote_sections_with_cache(storage, graph, notebook_query, refresh, offline, false).await
}

pub async fn load_onenote_sections_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>)> {
    let (nb, sections, _warnings) =
        load_onenote_sections_with_warnings(storage, graph, notebook_query, refresh, offline, cached)
            .await?;
    Ok((nb, sections))
}

/// Like [`load_onenote_sections_with_cache`], plus warnings about sections
/// that could not be fetched. After a partial fetch nothing is pruned from the
/// cache, so the returned list still contains the previously cached sections.
pub async fn load_onenote_sections_with_warnings(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>, Vec<String>)> {
    let mut warnings = Vec::new();
    let (nb, sections) = load_onenote_sections_inner(
        storage,
        graph,
        notebook_query,
        refresh,
        offline,
        cached,
        &mut warnings,
    )
    .await?;
    Ok((nb, sections, warnings))
}

async fn load_onenote_sections_inner(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
    warnings: &mut Vec<String>,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>)> {
    let nb = if let Some(q) = notebook_query.map(str::trim).filter(|s| !s.is_empty()) {
        Some(resolver::resolve_notebook_with_refresh(graph, storage, q, offline, refresh).await?)
    } else {
        None
    };
    let nb_key = nb.as_ref().map(|n| n.notebook_key.clone());

    if offline {
        return Ok((nb, storage.list_onenote_sections(nb_key.as_deref())?));
    }

    let scope_key = nb_key
        .as_deref()
        .map(|k| format!("onenote:sections:{k}"))
        .unwrap_or_else(|| CURSOR_SECTIONS_ALL.to_string());
    let ttl = graph.config().cli_cache_ttl_secs;

    if !refresh {
        let cached_secs = storage.list_onenote_sections(nb_key.as_deref())?;
        if !cached_secs.is_empty()
            && (cached
                || is_cursor_fresh(storage, &scope_key, ttl)
                || is_cursor_fresh(storage, CURSOR_SECTIONS_ALL, ttl)
                || is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl))
        {
            return Ok((nb, cached_secs));
        }
    }

    if let Some(ref target_nb) = nb {
        let (fetched, complete) = graph
            .list_onenote_sections_scoped(&target_nb.scope, Some(&target_nb.id))
            .await?;
        // Prune cached sections only after a complete fetch: a section group
        // that failed to load must not delete its cached sections and pages.
        storage.reconcile_onenote_notebook_sections(&target_nb.notebook_key, &fetched, complete)?;
        if !complete {
            warnings.push(format!(
                "Some sections of '{}' could not be fetched; cached sections were kept. Run the command again with --refresh.",
                target_nb.display_name
            ));
        }
    } else {
        let mut notebooks = storage.list_onenote_notebooks()?;
        let mut nb_complete_scopes: Option<std::collections::HashSet<String>> = None;
        if notebooks.is_empty()
            || refresh
            || (!cached
                && !is_cursor_fresh(storage, CURSOR_NOTEBOOKS_ALL, ttl)
                && !is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl))
        {
            let nb_res = graph.list_all_accessible_notebooks_scoped(true).await?;
            notebooks = nb_res.notebooks;
            nb_complete_scopes = Some(nb_res.complete_scopes);
            warnings.extend(nb_res.warnings);
            record_sync_cursor(storage, CURSOR_NOTEBOOKS_PERSONAL, None);
            record_sync_cursor(storage, CURSOR_NOTEBOOKS_ALL, None);
        }
        let sec_res = graph
            .list_all_accessible_sections_scoped(&notebooks, nb_complete_scopes.as_ref())
            .await?;
        if let Some(nb_scopes) = nb_complete_scopes {
            let reconciled_scopes: std::collections::HashSet<String> = nb_scopes
                .intersection(&sec_res.complete_scopes)
                .cloned()
                .collect();
            if !reconciled_scopes.is_empty() {
                storage.reconcile_onenote_hierarchy_for_scopes(
                    &notebooks,
                    &sec_res.section_groups,
                    &sec_res.sections,
                    &reconciled_scopes,
                )?;
            } else {
                storage.reconcile_onenote_notebooks_for_scopes(&notebooks, &nb_scopes)?;
                storage.reconcile_onenote_sections_for_scopes(
                    &sec_res.section_groups,
                    &sec_res.sections,
                    &sec_res.complete_scopes,
                )?;
            }
        } else {
            storage.reconcile_onenote_sections_for_scopes(
                &sec_res.section_groups,
                &sec_res.sections,
                &sec_res.complete_scopes,
            )?;
        }
    }
    record_sync_cursor(storage, scope_key, None);
    Ok((nb, storage.list_onenote_sections(nb_key.as_deref())?))
}

pub async fn resolve_onenote_section_query(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    offline: bool,
) -> Result<OnenoteSection> {
    resolve_onenote_section_query_with_refresh(storage, graph, section_query, offline, false).await
}

pub async fn resolve_onenote_section_query_with_refresh(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    offline: bool,
    force_refresh: bool,
) -> Result<OnenoteSection> {
    let trimmed = section_query.trim();
    if trimmed.is_empty() {
        bail!("OneNote section name or ID is empty.");
    }
    if let Some((nb_part, sec_part)) = trimmed.split_once('/') {
        if !nb_part.trim().is_empty() && !sec_part.trim().is_empty() && !trimmed.contains('|') {
            return resolver::resolve_section_with_refresh(
                graph,
                storage,
                sec_part.trim(),
                Some(nb_part.trim()),
                offline,
                force_refresh,
            )
            .await;
        }
    }
    resolver::resolve_section_with_refresh(graph, storage, trimmed, None, offline, force_refresh)
        .await
}

pub async fn list_onenote_pages_helper(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    limit: usize,
    refresh: bool,
    offline: bool,
) -> Result<(OnenoteSection, Vec<OnenotePageSummary>)> {
    let (sec, pages, _has_more) = list_onenote_pages_with_cache(
        storage,
        graph,
        section_query,
        limit,
        0,
        refresh,
        offline,
        false,
    )
    .await?;
    Ok((sec, pages))
}

#[allow(clippy::too_many_arguments)]
pub async fn list_onenote_pages_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    limit: usize,
    offset: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(OnenoteSection, Vec<OnenotePageSummary>, Option<bool>)> {
    let mut sec = resolve_onenote_section_query_with_refresh(
        storage,
        graph,
        section_query,
        offline,
        refresh,
    )
    .await?;
    if limit == 0 {
        return Ok((sec, Vec::new(), None));
    }
    if offline {
        let all = storage.list_onenote_pages(&sec.section_key)?;
        let (sliced, has_more) = page_slice(all, offset, limit);
        return Ok((sec, sliced, Some(has_more)));
    }

    let mut cursor_key = format!("onenote:pages:{}", sec.section_key);
    let end = offset.saturating_add(limit);
    if !refresh {
        let cached_pages = storage.list_onenote_pages(&sec.section_key)?;
        if !cached_pages.is_empty() {
            // The page cursor is recorded only after a complete listing, so it
            // vouches for the end of the cached list. Without one, `--cached`
            // still serves a window that ends before the cached rows do (its
            // pages and `has_more` are exact) but fetches any window reaching
            // past them instead of truncating it.
            let use_cache = if cached {
                cached_pages.len() > end || storage.get_sync_cursor(&cursor_key)?.is_some()
            } else {
                is_cursor_fresh(storage, &cursor_key, graph.config().cli_cache_ttl_secs)
            };
            if use_cache {
                let (sliced, has_more) = page_slice(cached_pages, offset, limit);
                return Ok((sec, sliced, Some(has_more)));
            }
        }
    }

    let want = end.saturating_add(1);
    let mut listing = match graph
        .list_onenote_page_listing(&sec.scope, &sec.id, want)
        .await
    {
        Ok(listing) => listing,
        Err(err) if !refresh && crate::graph::onenote::is_not_found_error(&err) => {
            let refreshed_sec = resolve_onenote_section_query_with_refresh(
                storage,
                graph,
                section_query,
                false,
                true,
            )
            .await?;
            if refreshed_sec.section_key == sec.section_key {
                return Err(err);
            }
            sec = refreshed_sec;
            cursor_key = format!("onenote:pages:{}", sec.section_key);
            graph
                .list_onenote_page_listing(&sec.scope, &sec.id, want)
                .await?
        }
        Err(err) => return Err(err),
    };
    if !listing.levels_known {
        storage.preserve_cached_page_hierarchy(&sec.section_key, &mut listing.pages)?;
    }
    let fetched = listing.pages;
    let has_more = fetched.len() > end;
    let complete = !listing.has_more;
    storage.reconcile_onenote_section_pages(&sec.section_key, &fetched, complete)?;
    // A partial listing must not mark the cache fresh: a later page beyond the
    // fetched rows would then be served (truncated) from the cache.
    if complete {
        record_sync_cursor(storage, cursor_key, sec.last_modified_at.clone());
    }
    let sliced = fetched.into_iter().skip(offset).take(limit).collect();
    Ok((sec, sliced, Some(has_more)))
}

pub async fn load_onenote_page_detail(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    offline: bool,
) -> Result<OnenotePageDetail> {
    load_onenote_page_detail_with_cache(storage, graph, page_query, offline, false).await
}

pub(super) async fn fetch_onenote_page_detail_remote(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
) -> Result<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }
    let cached_summary = resolver::resolve_page_cached(storage, q, None)?.into_option()?;
    let was_cached = cached_summary.is_some();
    let (scope, raw_id) = match cached_summary.as_ref() {
        Some(s) => (s.scope.clone(), s.id.clone()),
        None => split_onenote_entity_key(q),
    };
    let resolved_page_key = cached_summary
        .as_ref()
        .map(|s| s.page_key.clone())
        .unwrap_or_else(|| scope.make_entity_key(&raw_id));

    match graph
        .get_onenote_page_detail(&scope, &raw_id, cached_summary)
        .await
    {
        Ok(d) => Ok(d),
        Err(err) => {
            if crate::graph::onenote::is_not_found_error(&err) {
                let _ = storage.delete_onenote_page(&resolved_page_key);
                if !was_cached {
                    return Err(err.context(format!(
                        "OneNote page not found: '{q}' is neither a cached title nor a page ID. Pass a page key from 'onenote pages SECTION --json', or add --section to look the title up in one section."
                    )));
                }
            }
            Err(err)
        }
    }
}

pub async fn load_onenote_page_detail_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    offline: bool,
    cached: bool,
) -> Result<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }

    if offline || cached {
        if let Some(detail) = storage.get_onenote_page_detail(q)? {
            return Ok(detail);
        }
        let cached_summary = resolver::resolve_page_cached(storage, q, None)?.into_option()?;
        if let Some(ref summary) = cached_summary {
            if let Some(detail) = storage.get_onenote_page_detail(&summary.page_key)? {
                return Ok(detail);
            }
        }
        if offline {
            bail!("OneNote page not found in offline cache: {q}");
        }
    }

    let detail = fetch_onenote_page_detail_remote(storage, graph, q).await?;
    storage.upsert_onenote_page_detail(&detail)?;
    Ok(detail)
}
