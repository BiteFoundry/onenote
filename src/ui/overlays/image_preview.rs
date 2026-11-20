//! Full-screen HD inline terminal image preview modal (`v` / `Alt+v` or clicking `[IMG ...]`).
//! Supports embedded OneNote page images (`<img src=".../resources/...">`):
//! vertical/horizontal tall-image scrolling (`j`/`k`/`↑`/`↓`/`PgUp`/`PgDn`/`g`/`G`/wheel),
//! prev/next cycling (`[`/`]`, `←`/`→`), zoom (`+`/`-`/`0`), protocol cycle (`p`),
//! open (`o`), download (`d`), and close (`Esc`/`q`/`v` or clicking `[✕ Esc]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::TerminalImageProtocol,
    models::OnenotePageDetail,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewTarget {
    pub page_key: String,
    pub resource_index: usize,
    pub url: String,
    pub display_name: String,
    pub mime_type: String,
}

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub items: Vec<PreviewTarget>,
    pub index: usize,
    /// Zoom percentage (`25..=300`, default `100`).
    pub zoom_pct: u16,
    pub scroll_y: u16,
    pub scroll_x: u16,
    pub error: Option<String>,
}

impl ImagePreviewOverlay {
    pub fn from_onenote_page(
        detail: &OnenotePageDetail,
        preferred_resource_idx: Option<usize>,
    ) -> Option<Self> {
        let mut items = Vec::new();
        let mut start = 0usize;
        for (res_idx, res) in detail.resources.iter().enumerate() {
            if !res.is_image {
                continue;
            }
            if preferred_resource_idx == Some(res_idx) {
                start = items.len();
            }
            let url = res
                .fullres_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&res.resource_url)
                .to_string();
            let display_name = res
                .file_name
                .as_deref()
                .or(res.alt_text.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("image-{}.png", res_idx + 1));
            items.push(PreviewTarget {
                page_key: detail.summary.page_key.clone(),
                resource_index: res_idx,
                url,
                display_name,
                mime_type: res.mime_type.clone(),
            });
        }
        if items.is_empty() {
            return None;
        }
        Some(Self {
            items,
            index: start,
            zoom_pct: 100,
            scroll_y: 0,
            scroll_x: 0,
            error: None,
        })
    }

    pub fn current(&self) -> Option<&PreviewTarget> {
        self.items.get(self.index)
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let inner_w = rect.width.saturating_sub(2).max(12);
        let inner_h = rect.height.saturating_sub(3).max(6);
        let base_cols = rect.width.saturating_sub(4).max(12);
        let max_cols = inner_w.saturating_mul(2).max(base_cols).min(240);
        let scaled_cols = ((base_cols as u32 * self.zoom_pct as u32) / 100)
            .clamp(8, max_cols as u32) as u16;
        // Pass `scrollable_max_rows >= scaled_cols` so `img_preview` fits tall images to width
        // and lets them scroll vertically immediately at 100% zoom.
        let scrollable_max_rows = ((inner_h as u32 * 4 * self.zoom_pct as u32) / 100)
            .max(scaled_cols as u32)
            .clamp(6, 240) as u16;
        (scaled_cols, scrollable_max_rows)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                self.scroll_y = self.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.scroll_y = self.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.scroll_y = self.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.scroll_y = self.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::ToggleShowLinkUrls | Action::Input('h') => {
                self.scroll_x = self.scroll_x.saturating_sub(4);
                OverlayOutcome::Handled
            }
            Action::Input('l') => {
                self.scroll_x = self.scroll_x.saturating_add(4);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.scroll_y = self.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    self.scroll_y = self.scroll_y.saturating_add(*delta as u16);
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::OnenotePrevSection | Action::Input('[') => {
                if self.items.len() > 1 {
                    self.index = (self.index + self.items.len() - 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                    return OverlayOutcome::Dispatch(Action::LoadPreviewImage);
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight | Action::OnenoteNextSection | Action::Input(']') => {
                if self.items.len() > 1 {
                    self.index = (self.index + 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                    return OverlayOutcome::Dispatch(Action::LoadPreviewImage);
                }
                OverlayOutcome::Handled
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_pct = (self.zoom_pct + 25).min(300);
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('-') => {
                self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('0') => {
                self.zoom_pct = 100;
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('p') => OverlayOutcome::Dispatch(Action::CycleImageProtocol),
            Action::OnenoteOpenWeb | Action::Input('o') | Action::Input('O') => {
                if let Some(target) = self.current() {
                    OverlayOutcome::Dispatch(Action::OpenUrl(target.url.clone()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input('d') => {
                if let Some(target) = self.current() {
                    OverlayOutcome::Dispatch(Action::DownloadResource(target.resource_index))
                } else {
                    OverlayOutcome::Handled
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let proto = ui.graphics_caps.protocol;
        let close_label = "[✕ Esc]";
        let close_w = close_label.width() as u16;
        let show_close_pill = rect.width > close_w + 4;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let title = match self.current() {
            Some(t) => format!(
                " Image {}/{}: {} · {}% · {} · j/k/Wheel Scroll · [/] Prev/Next · +/- Zoom · p Proto · d Save ",
                self.index + 1,
                self.items.len(),
                crate::ui::truncate_width(&t.display_name, 22),
                self.zoom_pct,
                proto.label()
            ),
            None => " Image Preview ".to_string(),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_close_pill {
            let close_x = rect.right().saturating_sub(close_w + 1);
            let close_rect = Rect::new(close_x, rect.y, close_w, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    close_label,
                    Style::default().fg(theme.fg).bg(theme.bg_elevated),
                )),
                close_rect,
            );
            hitmap.add_line(
                close_x,
                rect.y,
                close_w,
                HitTarget::Button(ButtonId::ConfirmCancel),
            );
        }

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if let Some(ref err) = self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("Preview failed: {err}"),
                    Style::default().fg(theme.error),
                )),
                inner,
            );
            return;
        }

        let Some(target) = self.current() else {
            return;
        };
        let (cols, rows) = self.target_dimensions(ui.term_width, ui.term_height);
        let cache_key = format!(
            "hd:{}:{}:{}:{}:{:?}",
            target.page_key, target.resource_index, cols, rows, proto
        );

        if let Some(rendered) = app.image_cache.get(&cache_key) {
            let is_pixel =
                rendered.pixel_payload.is_some() && proto != TerminalImageProtocol::Halfblock;
            let total_rows = if is_pixel {
                rendered.rows
            } else {
                rendered.lines.len() as u16
            };
            let total_cols = rendered.cols;

            let avail_h = if is_pixel
                && !ui.graphics_caps.cell_px_exact
                && proto == TerminalImageProtocol::Sixel
            {
                inner.height.saturating_sub(1).max(1)
            } else {
                inner.height
            };
            let show_scrollbar = total_rows > avail_h && inner.width > 2;
            let avail_w = if show_scrollbar {
                inner.width.saturating_sub(1)
            } else {
                inner.width
            };

            let max_scroll_y = total_rows.saturating_sub(avail_h);
            let max_scroll_x = total_cols.saturating_sub(avail_w);
            self.scroll_y = self.scroll_y.min(max_scroll_y);
            self.scroll_x = self.scroll_x.min(max_scroll_x);
            let sy = self.scroll_y;
            let sx = self.scroll_x;

            if is_pixel {
                let draw_w = total_cols.saturating_sub(sx).min(avail_w);
                let draw_h = total_rows.saturating_sub(sy).min(avail_h);
                let px_x = inner.x + avail_w.saturating_sub(draw_w) / 2;
                let px_y = inner.y + avail_h.saturating_sub(draw_h) / 2;
                ui.pending_pixel_graphic = Some(PendingPixelGraphic {
                    cache_key: format!("{cache_key}#crop:{sx},{sy},{avail_w},{avail_h}"),
                    rect: Rect::new(px_x, px_y, draw_w, draw_h),
                    protocol: proto,
                });
            } else {
                let view_w = total_cols.saturating_sub(sx).min(avail_w);
                let view_h = total_rows.saturating_sub(sy).min(avail_h);
                let pad_x = avail_w.saturating_sub(view_w) / 2;
                let pad_y = avail_h.saturating_sub(view_h) / 2;
                let draw_rect = Rect::new(inner.x + pad_x, inner.y + pad_y, view_w, view_h);
                frame.render_widget(
                    Paragraph::new(rendered.lines.clone()).scroll((sy, sx)),
                    draw_rect,
                );
            }

            if show_scrollbar {
                let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
                let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                    .position(sy as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Loading OneNote image preview...",
                    theme.muted_style(),
                )),
                inner,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        auth::{AuthManager, StoredTokens},
        config::AppConfig,
        graph::GraphClient,
        img_preview::{decode_and_render_for_protocol, TerminalGraphicsCaps},
        models::{OnenotePageSummary, OnenoteResource, OnenoteScope},
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
    };

    fn make_test_state() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let tokens = StoredTokens {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: Some("tester@example.com".to_string()),
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        ui.term_width = 100;
        ui.term_height = 30;
        (app, ui)
    }

    fn make_tall_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([
                ((x * 255) / w.max(1)) as u8,
                ((y * 255) / h.max(1)) as u8,
                160,
                255,
            ])
        });
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn make_preview_overlay() -> ImagePreviewOverlay {
        let detail = OnenotePageDetail {
            summary: OnenotePageSummary {
                page_key: "me|page-1".to_string(),
                id: "page-1".to_string(),
                scope: OnenoteScope::Me,
                section_id: "sec-1".to_string(),
                section_name: Some("Specs".to_string()),
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("Engineering".to_string()),
                title: "Tall Image Page".to_string(),
                level: 0,
                order: 0,
                created_at: None,
                last_modified_at: None,
                content_url: None,
                web_url: None,
                preview_text: None,
                has_todo: false,
            },
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: Vec::new(),
            resources: vec![
                OnenoteResource {
                    element_id: Some("img:1".to_string()),
                    resource_url: "https://example.com/tall.png".to_string(),
                    fullres_url: None,
                    mime_type: "image/png".to_string(),
                    file_name: Some("tall.png".to_string()),
                    is_image: true,
                    alt_text: None,
                },
                OnenoteResource {
                    element_id: Some("img:2".to_string()),
                    resource_url: "https://example.com/second.png".to_string(),
                    fullres_url: None,
                    mime_type: "image/png".to_string(),
                    file_name: Some("second.png".to_string()),
                    is_image: true,
                    alt_text: None,
                },
            ],
            primary_div_id: None,
            synced_at: None,
        };
        ImagePreviewOverlay::from_onenote_page(&detail, Some(0)).unwrap()
    }

    #[test]
    fn test_image_preview_scroll_clamps_and_renders_scrollbar_for_halfblock_and_sixel() {
        let png = make_tall_png(400, 2000);
        let area = Rect::new(0, 0, 100, 30);

        for proto in [TerminalImageProtocol::Halfblock, TerminalImageProtocol::Sixel] {
            let (mut app, mut ui) = make_test_state();
            ui.graphics_caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };

            let mut overlay = make_preview_overlay();
            let (cols, rows) = overlay.target_dimensions(ui.term_width, ui.term_height);
            let rendered =
                decode_and_render_for_protocol(&png, cols, rows, (25, 25, 25), ui.graphics_caps)
                    .unwrap();
            let total_rows = if proto == TerminalImageProtocol::Halfblock {
                rendered.lines.len() as u16
            } else {
                rendered.rows
            };
            let rect = ImagePreviewOverlay::modal_rect(area);
            let inner_h = rect.height.saturating_sub(2);
            let expected_max_scroll = total_rows.saturating_sub(inner_h);
            assert!(
                expected_max_scroll > 0,
                "expected scrollable tall image for {proto:?}, total_rows={total_rows}, inner_h={inner_h}"
            );

            let base_key = format!("hd:me|page-1:0:{cols}:{rows}:{proto:?}");
            app.image_cache.insert(base_key.clone(), rendered);

            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut hitmap = HitMap::new();

            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphic = None;
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            // Verify clickable [✕ Esc] close pill is registered in top-right border
            let close_region = hitmap
                .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
                .expect("expected [✕ Esc] close pill hit target");
            assert_eq!(close_region.y, rect.y);
            assert!(close_region.right() <= rect.right());

            // Verify vertical scrollbar rendered on right-most inner column
            let sb_x = rect.right() - 2;
            let buf = terminal.backend().buffer();
            let has_sb_cell = (rect.y + 1..rect.bottom() - 1).any(|y| {
                let sym = buf[(sb_x, y)].symbol();
                !sym.trim().is_empty()
            });
            assert!(
                has_sb_cell,
                "expected scrollbar glyph in right inner column for {proto:?}"
            );

            if proto == TerminalImageProtocol::Sixel {
                let pg0 = ui
                    .pending_pixel_graphic
                    .clone()
                    .expect("expected pending pixel graphic for Sixel");
                assert!(pg0.cache_key.starts_with(&format!("{base_key}#crop:0,0,")));
            }

            // MouseScroll down + j + PageDown + ScrollBottom clamps to exact max_scroll_y
            assert_eq!(
                overlay.handle(&Action::MouseScroll {
                    delta: 3,
                    column: 50,
                    row: 15,
                }),
                OverlayOutcome::Handled
            );
            assert_eq!(overlay.handle(&Action::Input('j')), OverlayOutcome::Handled);
            assert_eq!(overlay.handle(&Action::PageDown), OverlayOutcome::Handled);
            assert_eq!(
                overlay.handle(&Action::ScrollBottom),
                OverlayOutcome::Handled
            );
            assert_eq!(overlay.scroll_y, u16::MAX);

            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphic = None;
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            assert_eq!(overlay.scroll_y, expected_max_scroll);

            if proto == TerminalImageProtocol::Sixel {
                let pg_bottom = ui
                    .pending_pixel_graphic
                    .clone()
                    .expect("expected pending pixel graphic at bottom for Sixel");
                assert!(pg_bottom
                    .cache_key
                    .starts_with(&format!("{base_key}#crop:0,{expected_max_scroll},")));
            }

            // Immediate upward response on wheel up & k
            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 50,
                row: 15,
            });
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(3));
            overlay.handle(&Action::Input('k'));
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(5));

            // ScrollTop resets both axes
            overlay.handle(&Action::Input('l'));
            assert!(overlay.scroll_x > 0);
            overlay.handle(&Action::ScrollTop);
            assert_eq!(overlay.scroll_y, 0);
            assert_eq!(overlay.scroll_x, 0);
        }

        // Verify prev/next/close shortcut actions in handle()
        let mut ov = make_preview_overlay();
        assert_eq!(ov.handle(&Action::OpenImagePreview), OverlayOutcome::Close);
        assert_eq!(
            ov.handle(&Action::OnenoteNextSection),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(ov.index, 1);
        assert_eq!(
            ov.handle(&Action::OnenoteOpenWeb),
            OverlayOutcome::Dispatch(Action::OpenUrl("https://example.com/second.png".to_string()))
        );
    }
}
