//! Background data fetches, terminal image rendering, and HTML/Markdown resource previews.

use std::sync::Arc;

use crate::{
    img_preview::{FitBox, TerminalGraphicsCaps},
    sync::SyncCommand,
};

use super::state::{
    is_markdown_resource_meta, sniff_preview_bytes_as_html, AppState, UiMsg, UiState,
};

impl AppState {
    pub fn request_section_pages(&mut self, section_key: String) {
        self.loading_onenote_pages = true;
        self.sync_ctrl
            .request(SyncCommand::RefreshSectionPages { section_key });
    }

    pub fn request_page_detail(&mut self, page_key: String) {
        self.loading_onenote_detail = !self.page_details.contains_key(&page_key);
        self.sync_ctrl
            .request(SyncCommand::HydratePage { page_key });
    }

    /// Triggered when the terminal regains focus (`FocusGained`): immediately re-reads
    /// the open page via `HydratePage` (even if section-level timestamps look unchanged),
    /// preserving the last cached view while refreshing.
    pub fn on_terminal_focus_gained(&mut self) {
        if let Some(pk) = self.active_page_key.clone() {
            self.sync_ctrl.request(SyncCommand::HydratePage { page_key: pk });
        } else if let Some(sk) = self.active_section_key.clone() {
            self.sync_ctrl
                .request(SyncCommand::RefreshSectionPages { section_key: sk });
        }
    }

    /// Requests an inline thumbnail or fullscreen HD terminal image render for a OneNote resource URL.
    pub fn request_onenote_image(
        &mut self,
        cache_key: String,
        resource_url: String,
        fit: FitBox,
        bg_rgb: (u8, u8, u8),
        caps: TerminalGraphicsCaps,
        halfblock_only: bool,
    ) {
        self.request_onenote_image_with_meta(
            cache_key,
            resource_url,
            None,
            None,
            fit,
            bg_rgb,
            caps,
            halfblock_only,
        );
    }

    /// Requests a OneNote image resource with optional display name & MIME hint.
    /// Checks the LRU `image_raw_cache` first so zoom (`+`/`-`/`0`), protocol cycle (`p`),
    /// and resize re-render locally without refetching from Graph API.
    /// When `halfblock_only == false` (fullscreen `LoadPreviewImage`), sniffs bytes via
    /// `bitefoundry_html::sniff_content_kind` and routes `ContentKind::Html` to `Overlay::HtmlPreview`.
    #[allow(clippy::too_many_arguments)]
    pub fn request_onenote_image_with_meta(
        &mut self,
        cache_key: String,
        resource_url: String,
        display_name: Option<String>,
        mime_hint: Option<String>,
        fit: FitBox,
        bg_rgb: (u8, u8, u8),
        caps: TerminalGraphicsCaps,
        halfblock_only: bool,
    ) {
        if self.image_cache.contains_key(&cache_key) || self.inflight_images.contains(&cache_key) {
            return;
        }
        self.inflight_images.insert(cache_key.clone());
        let cached_raw = self.touch_raw_image_cache(&resource_url);
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let fetched: anyhow::Result<(Option<String>, Arc<[u8]>)> = match cached_raw {
                Some(bytes) => Ok((None, bytes)),
                None => match graph.fetch_onenote_resource_bytes(&resource_url).await {
                    Ok((fetched_mime, bytes)) => {
                        let arc: Arc<[u8]> = Arc::from(bytes);
                        Ok((Some(fetched_mime), arc))
                    }
                    Err(err) => Err(err),
                },
            };

            match fetched {
                Ok((fetched_mime_opt, bytes)) => {
                    let effective_mime = match fetched_mime_opt.as_deref() {
                        Some("application/octet-stream") | None => mime_hint.as_deref(),
                        Some(m) => Some(m),
                    };
                    if let Some(html) =
                        sniff_preview_bytes_as_html(effective_mime, display_name.as_deref(), &bytes)
                    {
                        if !halfblock_only {
                            let title = display_name
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| "HTML Resource".to_string());
                            let _ = tx.send(UiMsg::ImageRoutedToHtml {
                                cache_key,
                                title,
                                html,
                            });
                        } else {
                            let _ = tx.send(UiMsg::ImageLoadFailed {
                                cache_key,
                                error: "Resource is HTML, not a raster image".to_string(),
                            });
                        }
                        return;
                    }

                    // Populate LRU raw byte cache on a fresh network fetch.
                    if fetched_mime_opt.is_some() {
                        let _ = tx.send(UiMsg::ImageBytesFetched {
                            resource_url,
                            bytes: Arc::clone(&bytes),
                        });
                    }

                    let render_res = tokio::task::spawn_blocking(move || {
                        if halfblock_only {
                            crate::img_preview::decode_and_render_halfblocks_with_cell_px(
                                &bytes,
                                fit,
                                bg_rgb,
                                caps.cell_px,
                            )
                        } else {
                            crate::img_preview::decode_and_render_for_protocol(
                                &bytes, fit, bg_rgb, caps,
                            )
                        }
                    })
                    .await;
                    match render_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::ImageLoaded {
                                cache_key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::ImageLoadFailed {
                                cache_key,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageLoadFailed {
                                cache_key,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageLoadFailed {
                        cache_key,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    /// Fetches a `.html`/`.htm`/`.md` OneNote resource and opens it in `Overlay::HtmlPreview`.
    pub fn spawn_load_html_resource(&mut self, resource_idx: usize, ui: &mut UiState) {
        let Some(detail) = self.active_page_detail() else {
            ui.warn("No page loaded");
            return;
        };
        let page_key = detail.summary.page_key.clone();
        let Some(res) = detail.resources.get(resource_idx).cloned() else {
            ui.warn("Resource not found");
            return;
        };
        let url = res
            .fullres_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&res.resource_url)
            .to_string();
        let is_md = is_markdown_resource_meta(Some(&res.mime_type), res.file_name.as_deref());
        let title = res
            .file_name
            .as_deref()
            .or(res.alt_text.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if is_md {
                    format!("resource-{}.md", resource_idx + 1)
                } else {
                    format!("resource-{}.html", resource_idx + 1)
                }
            });
        let res_mime = res.mime_type.clone();
        self.html_resource_seq = self.html_resource_seq.wrapping_add(1);
        let seq = self.html_resource_seq;
        self.active_html_resource_req = Some((seq, page_key.clone()));
        let kind_label = if is_md { "Markdown" } else { "HTML" };
        let task_id = ui.tasks.start(format!("Loading {kind_label} '{title}'"));
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.fetch_onenote_resource_bytes(&url).await {
                Ok((fetched_mime, bytes)) => {
                    let effective_mime = if fetched_mime.trim().is_empty()
                        || fetched_mime.eq_ignore_ascii_case("application/octet-stream")
                    {
                        if res_mime.trim().is_empty() {
                            None
                        } else {
                            Some(res_mime)
                        }
                    } else {
                        Some(fetched_mime)
                    };
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = tx.send(UiMsg::HtmlResourceLoaded {
                        task_id,
                        seq,
                        page_key,
                        title,
                        mime_type: effective_mime,
                        html,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to load {kind_label} resource: {err}"),
                    });
                }
            }
        });
    }

    /// Serves HTML natively via the lazy localhost `NativeHtmlServer` and sends `UiMsg::HtmlServed`.
    pub fn spawn_serve_html_in_browser(&mut self, title: String, html: String, ui: &mut UiState) {
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        ui.info("Starting local HTML preview server...");
        tokio::spawn(async move {
            match server.serve_html(&title, &html).await {
                Ok(url) => {
                    let _ = tx.send(UiMsg::HtmlServed { url });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::Error(format!(
                        "Failed to serve HTML locally: {err}"
                    )));
                }
            }
        });
    }

}
