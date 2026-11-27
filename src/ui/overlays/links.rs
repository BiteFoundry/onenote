//! Link Extractor Overlay (`u`): extracts all URLs (HTML `<a href>`, parsed `RichSpan` links,
//! bare `https://...` / `http://...` links, and `web_url`) from the currently open OneNote page.
//!
//! Controls: `j`/`k`/`↑`/`↓` navigate, `Enter` open in browser, `y` copy URL, `Esc`/`q`/`u` close.

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use scraper::{Html, Selector};

use crate::{
    models::{OnenoteBlockKind, OnenotePageDetail, RichSpan},
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct LinkExtractorOverlay {
    pub context_title: String,
    pub links: Vec<ExtractedLink>,
    pub selected: usize,
}

impl LinkExtractorOverlay {
    pub fn from_onenote_page(detail: &OnenotePageDetail) -> Self {
        let links = extract_links_from_onenote_page(detail);
        Self {
            context_title: detail.summary.title.clone(),
            links,
            selected: 0,
        }
    }

    pub fn current_url(&self) -> Option<&str> {
        let idx = self.selected.min(self.links.len().saturating_sub(1));
        self.links.get(idx).map(|l| l.url.as_str())
    }

    pub fn select(&mut self, index: usize) {
        if index < self.links.len() {
            self.selected = index;
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenLinkExtractor
            | Action::Input('q')
            | Action::Input('u') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::CursorUp | Action::Input('k') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + self.links.len() - 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::Input('j') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom => {
                self.selected = self.links.len().saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(url) = self.current_url() {
                    OverlayOutcome::CloseAndDispatch(Action::OpenUrl(url.to_string()))
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::Input('y') => {
                if let Some(url) = self.current_url() {
                    OverlayOutcome::Dispatch(Action::CopyUrlToClipboard(url.to_string()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input(ch) if ch.is_ascii_digit() && *ch != '0' => {
                let idx = (*ch as usize) - ('1' as usize);
                if idx < self.links.len() {
                    self.selected = idx;
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        hitmap.add(area, HitTarget::OverlayBackdrop);
        let max_w = area.width.saturating_sub(6).clamp(36, 96);
        let max_h = area.height.saturating_sub(4).clamp(9, 22);
        let rect = centered_rect(area, max_w, max_h);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = format!(
            " Links ({}) · {} · Enter Open · y Copy · Esc Close ",
            self.links.len(),
            crate::ui::truncate_width(&self.context_title, 28)
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if self.links.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("No links found on this page.", theme.muted_style())),
                inner,
            );
            return;
        }

        let visible_rows = inner.height.saturating_sub(1) as usize;
        let selected = self.selected.min(self.links.len() - 1);
        let offset = crate::ui::nav_pane::selection_offset(selected, visible_rows, self.links.len());

        for (i, link) in self.links.iter().skip(offset).take(visible_rows).enumerate() {
            let idx = offset + i;
            let row_y = inner.y + i as u16;
            let is_sel = idx == selected;
            let row_style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let prefix = format!("{:>2}. ", idx + 1);
            let label = crate::ui::truncate_width(&link.label, 30);
            let avail_url = (inner.width as usize).saturating_sub(prefix.len() + label.len() + 4);
            let url_disp = crate::ui::truncate_width(&link.url, avail_url);
            let line = Line::from(vec![
                Span::styled(prefix, row_style.add_modifier(Modifier::BOLD)),
                Span::styled(label, row_style),
                Span::styled("  ", row_style),
                Span::styled(
                    url_disp,
                    if is_sel {
                        row_style.add_modifier(Modifier::UNDERLINED)
                    } else {
                        Style::default().bg(theme.bg_elevated).fg(theme.accent_soft)
                    },
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(row_style),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
            hitmap.add_line(inner.x, row_y, inner.width, HitTarget::OverlayItem { index: idx });
        }

        let footer_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "j/k move · 1-9 jump · Enter open in browser · y copy URL · Esc close",
                theme.muted_style(),
            )),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }
}

pub fn extract_links_from_onenote_page(detail: &OnenotePageDetail) -> Vec<ExtractedLink> {
    let mut out: Vec<ExtractedLink> = Vec::new();
    let mut index_by_url: HashMap<String, usize> = HashMap::new();

    // 1. Structured RichSpans from parsed OneNote blocks
    let mut push_spans = |spans: &[RichSpan]| {
        for sp in spans {
            if let Some(url) = sp.style.link_url.as_deref() {
                push_link(&mut out, &mut index_by_url, &sp.text, url);
            }
        }
    };
    for blk in &detail.blocks {
        match &blk.kind {
            OnenoteBlockKind::Heading { spans, .. }
            | OnenoteBlockKind::Paragraph { spans }
            | OnenoteBlockKind::ListItem { spans, .. }
            | OnenoteBlockKind::Quote { spans } => push_spans(spans),
            OnenoteBlockKind::Table { headers, rows } => {
                for cell in headers {
                    push_spans(cell);
                }
                for row in rows {
                    for cell in row {
                        push_spans(cell);
                    }
                }
            }
            _ => {}
        }
    }

    // 2. HTML <a href> anchors in raw_xhtml
    if !detail.raw_xhtml.trim().is_empty() {
        static LINK_HREF_SELECTOR: std::sync::LazyLock<Selector> =
            std::sync::LazyLock::new(|| Selector::parse("a[href]").expect("valid a[href] selector"));
        let doc = Html::parse_document(&detail.raw_xhtml);
        for el in doc.select(&LINK_HREF_SELECTOR) {
            if let Some(href) = el.value().attr("href") {
                let text: String = el.text().collect();
                push_link(&mut out, &mut index_by_url, &text, href);
            }
        }
    }

    // 3. Bare URLs in body_plain
    for url in find_bare_urls(&detail.body_plain) {
        push_link(&mut out, &mut index_by_url, "", &url);
    }

    // 4. Page web_url
    if let Some(web) = detail.summary.web_url.as_deref() {
        push_link(&mut out, &mut index_by_url, "Open page in OneNote Web", web);
    }

    out
}

fn push_link(
    out: &mut Vec<ExtractedLink>,
    index_by_url: &mut HashMap<String, usize>,
    raw_label: &str,
    raw_url: &str,
) {
    let url = clean_url(raw_url);
    if !is_openable_url(&url) {
        return;
    }
    let clean_label = raw_label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let key = url.to_ascii_lowercase();
    if let Some(&existing_idx) = index_by_url.get(&key) {
        if out[existing_idx].label == out[existing_idx].url && !clean_label.is_empty() {
            out[existing_idx].label = clean_label;
        }
        return;
    }
    let label = if clean_label.is_empty() {
        url.clone()
    } else {
        clean_label
    };
    let idx = out.len();
    out.push(ExtractedLink { label, url });
    index_by_url.insert(key, idx);
}

fn clean_url(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(['.', ',', ';', ')', ']', '>', '"', '\''])
        .to_string()
}

fn is_openable_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
        && url.len() > 8
}

fn find_bare_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c| matches!(c, '(' | '<' | '[' | '"' | '\''));
        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            let cleaned = clean_url(trimmed);
            if is_openable_url(&cleaned) {
                urls.push(cleaned);
            }
        }
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OnenotePageSummary, OnenoteScope};

    #[test]
    fn extracts_deduplicated_links_from_onenote_page() {
        let summary = OnenotePageSummary {
            page_key: "me|p1".into(),
            id: "p1".into(),
            scope: OnenoteScope::Me,
            section_id: "s1".into(),
            section_name: None,
            notebook_id: None,
            notebook_name: None,
            title: "Design Links".into(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: Some("https://onedrive.live.com/onenote/p1".into()),
            preview_text: None,
            has_todo: false,
        };
        let detail = crate::parser::parse_onenote_page_xhtml(
            summary,
            r#"<html><body><p>Read <a href="https://docs.example.com/rfc">RFC Doc</a> and visit https://status.example.com.</p></body></html>"#,
        );
        let links = extract_links_from_onenote_page(&detail);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].label, "RFC Doc");
        assert_eq!(links[0].url, "https://docs.example.com/rfc");
        assert_eq!(links[1].url, "https://status.example.com");
        assert_eq!(links[2].url, "https://onedrive.live.com/onenote/p1");
    }
}
