//! Rendered screen region -> clickable target mapping (`HitMap`).
//!
//! Rebuilt every frame; later registrations (drawn on top) win on overlap.

use ratatui::layout::{Position, Rect};

/// Toolbar, header, and modal button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    Search,
    CommandPalette,
    DensityToggle,
    Help,
    Settings,
    Refresh,
    // OneNote toolbar actions
    OnenoteNewNotebook,
    OnenoteNewSection,
    OnenoteNewPage,
    OnenoteNewSubpage,
    OnenoteAppend,
    OnenoteEditBlock,
    OnenoteToggleTodo,
    OnenoteDeletePage,
    OnenoteOpenWeb,
    OnenoteLinks,
    OnenoteToggleHrefs,
    Attachments,
    ImagePreview,
    // Narrow responsive bar pills
    NarrowNotebooks,
    NarrowPages,
    NarrowCanvas,
    // Confirm dialog
    ConfirmOk,
    ConfirmCancel,
}

/// Clickable targets on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// A notebook row in the left sidebar (`nav_pane`).
    NavRow(usize),
    /// A section tab in the OneNote top section ribbon.
    OnenoteSectionTab(usize),
    /// A page row in the OneNote page list.
    OnenotePageItem(usize),
    /// A content block in the OneNote page reader canvas.
    OnenoteBlock(usize),
    /// An interactive `to-do` checkbox inside the OneNote page canvas.
    OnenoteTodoToggle(usize),
    /// An embedded image or file resource chip inside the OneNote page canvas.
    OnenoteResource(usize),
    /// Header / toolbar / status / modal button.
    Button(ButtonId),
    /// Modal / overlay outer frame (prevents clicks inside modal padding from closing the modal).
    OverlayBody,
    /// Item inside the top overlay.
    OverlayItem { index: usize },
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Registers a clickable rectangle. Zero-sized areas are ignored.
    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    /// Registers a single-row clickable span.
    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    /// Resolves `(row, col)` to the top-most target.
    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let position = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| *target)
    }

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, item)| *item == target)
            .map(|(area, _)| *area)
    }

    pub fn regions(&self) -> impl Iterator<Item = &(Rect, HitTarget)> {
        self.regions.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_inside_and_outside_and_overlap() {
        let mut map = HitMap::new();
        map.add(Rect::new(2, 3, 5, 2), HitTarget::NavRow(7));
        assert_eq!(map.hit(3, 2), Some(HitTarget::NavRow(7)));
        assert_eq!(map.hit(4, 6), Some(HitTarget::NavRow(7)));
        assert_eq!(map.hit(2, 2), None);

        map.add(Rect::new(0, 0, 20, 4), HitTarget::OnenotePageItem(1));
        map.add_line(2, 2, 8, HitTarget::OnenoteTodoToggle(0));
        assert_eq!(map.hit(2, 3), Some(HitTarget::OnenoteTodoToggle(0)));
        assert_eq!(map.hit(0, 0), Some(HitTarget::OnenotePageItem(1)));
    }
}
