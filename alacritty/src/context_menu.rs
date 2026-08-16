//! Right-click pane context menu.

use crate::event::TabAction;

pub const MENU_MIN_WIDTH: f32 = 200.0;
pub const MENU_PAD: f32 = 8.0;

const LABEL_X_SLACK: f32 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMenuItem {
    pub label: &'static str,
    pub action: TabAction,
}

impl ContextMenuItem {
    pub fn base_items(is_split: bool, pane_count: usize) -> Vec<ContextMenuItem> {
        let mut items = vec![
            ContextMenuItem { label: "Split horizontal", action: TabAction::SplitPaneHorizontal },
            ContextMenuItem { label: "Split vertical", action: TabAction::SplitPaneVertical },
        ];
        if pane_count > 1 {
            items.push(ContextMenuItem { label: "Close pane", action: TabAction::ClosePane });
        }
        if is_split {
            items.push(ContextMenuItem { label: "Zoom pane", action: TabAction::TogglePaneZoom });
        }
        items
    }
}

pub fn menu_width_for(items: &[ContextMenuItem], cell_width: f32) -> f32 {
    let max_chars = items.iter().map(|i| i.label.chars().count()).max().unwrap_or(12);
    let content = (max_chars as f32) * cell_width + 2.0 * MENU_PAD + LABEL_X_SLACK;
    content.max(MENU_MIN_WIDTH)
}

#[derive(Debug, Clone)]
pub struct ContextMenuState {
    open: bool,
    x: f32,
    y: f32,
    width: f32,
    items: Vec<ContextMenuItem>,
    hovered: Option<usize>,
}

impl Default for ContextMenuState {
    fn default() -> Self {
        Self {
            open: false,
            x: 0.0,
            y: 0.0,
            width: MENU_MIN_WIDTH,
            items: Vec::new(),
            hovered: None,
        }
    }
}

impl ContextMenuState {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open_at(
        &mut self,
        mouse_x: f32,
        mouse_y: f32,
        items: Vec<ContextMenuItem>,
        bounds: (f32, f32, f32, f32),
        row_h: f32,
        cell_width: f32,
    ) {
        let (x0, y0, x1, y1) = bounds;
        let menu_w = menu_width_for(&items, cell_width);
        let menu_h = (items.len() as f32) * row_h + 2.0 * MENU_PAD;
        self.width = menu_w;
        self.x = mouse_x.min(x1 - menu_w).max(x0);
        self.y = mouse_y.min(y1 - menu_h).max(y0);
        self.items = items;
        self.open = true;
        self.hovered = self.row_at(mouse_x, mouse_y, row_h);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.hovered = None;
    }

    pub fn x(&self) -> f32 {
        self.x
    }

    pub fn y(&self) -> f32 {
        self.y
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn items(&self) -> &[ContextMenuItem] {
        &self.items
    }

    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    pub fn set_hover_at(&mut self, mouse_x: f32, mouse_y: f32, row_h: f32) -> bool {
        if !self.open {
            return false;
        }
        let next = self.row_at(mouse_x, mouse_y, row_h);
        if next != self.hovered {
            self.hovered = next;
            true
        } else {
            false
        }
    }

    fn row_at(&self, click_x: f32, click_y: f32, row_h: f32) -> Option<usize> {
        if click_x < self.x || click_x > self.x + self.width {
            return None;
        }
        let row_top = self.y + MENU_PAD;
        if click_y < row_top {
            return None;
        }
        let row_index = ((click_y - row_top) / row_h) as usize;
        if row_index >= self.items.len() {
            return None;
        }
        Some(row_index)
    }

    pub fn hit_test(&self, click_x: f32, click_y: f32, row_h: f32) -> Option<TabAction> {
        if !self.open {
            return None;
        }
        let row = self.row_at(click_x, click_y, row_h)?;
        self.items.get(row).map(|item| item.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RH: f32 = 20.0;
    const CW: f32 = 10.0;
    const BOUNDS: (f32, f32, f32, f32) = (0.0, 0.0, 1000.0, 1000.0);

    #[test]
    fn base_items_includes_splits_always() {
        let items = ContextMenuItem::base_items(false, 1);
        assert!(items.iter().any(|i| i.label == "Split horizontal"));
        assert!(items.iter().any(|i| i.label == "Split vertical"));
        assert!(!items.iter().any(|i| i.label == "Close pane"));
        assert!(!items.iter().any(|i| i.label == "Zoom pane"));
    }

    #[test]
    fn close_and_zoom_gated_by_layout() {
        let items = ContextMenuItem::base_items(true, 2);
        assert!(items.iter().any(|i| i.label == "Close pane"));
        assert!(items.iter().any(|i| i.label == "Zoom pane"));

        let items = ContextMenuItem::base_items(false, 2);
        assert!(items.iter().any(|i| i.label == "Close pane"));
        assert!(!items.iter().any(|i| i.label == "Zoom pane"));
    }

    #[test]
    fn open_clamps_to_window_bounds() {
        let mut menu = ContextMenuState::default();
        let items = ContextMenuItem::base_items(true, 2);
        let w = menu_width_for(&items, CW);
        menu.open_at(990.0, 990.0, items, BOUNDS, RH, CW);
        let h = (4.0 * RH) + 16.0;
        assert!(menu.x() <= 1000.0 - w);
        assert!(menu.y() <= 1000.0 - h);
        assert!((menu.width() - w).abs() < f32::EPSILON);
    }

    #[test]
    fn open_clamps_to_custom_origin() {
        let mut menu = ContextMenuState::default();
        let items = ContextMenuItem::base_items(false, 1);
        menu.open_at(10.0, 10.0, items, (50.0, 40.0, 500.0, 500.0), RH, CW);
        assert!((menu.x() - 50.0).abs() < f32::EPSILON);
        assert!((menu.y() - 40.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hit_test_finds_clicked_row() {
        let mut menu = ContextMenuState::default();
        let items = ContextMenuItem::base_items(true, 2);
        menu.open_at(100.0, 100.0, items, BOUNDS, RH, CW);

        assert_eq!(
            menu.hit_test(150.0, 110.0, RH),
            Some(TabAction::SplitPaneHorizontal)
        );
        assert_eq!(menu.hit_test(150.0, 150.0, RH), Some(TabAction::ClosePane));
        assert_eq!(menu.hit_test(200.0, 170.0, RH), Some(TabAction::TogglePaneZoom));
    }

    #[test]
    fn hit_test_misses_outside_menu() {
        let mut menu = ContextMenuState::default();
        menu.open_at(100.0, 100.0, ContextMenuItem::base_items(false, 1), BOUNDS, RH, CW);
        assert_eq!(menu.hit_test(50.0, 110.0, RH), None);
        assert_eq!(menu.hit_test(150.0, 300.0, RH), None);
        assert_eq!(menu.hit_test(150.0, 50.0, RH), None);
    }

    #[test]
    fn close_resets_open() {
        let mut menu = ContextMenuState::default();
        menu.open_at(10.0, 10.0, ContextMenuItem::base_items(false, 1), BOUNDS, RH, CW);
        assert!(menu.is_open());
        menu.close();
        assert!(!menu.is_open());
    }

    #[test]
    fn menu_width_fits_longest_label() {
        let items = ContextMenuItem::base_items(true, 2);
        let w = menu_width_for(&items, 12.0);
        assert!(w >= 17.0 * 12.0);
    }

    #[test]
    fn hover_tracks_row_under_cursor() {
        let mut menu = ContextMenuState::default();
        menu.open_at(100.0, 100.0, ContextMenuItem::base_items(true, 2), BOUNDS, RH, CW);
        assert_eq!(menu.hovered(), None);

        assert!(menu.set_hover_at(150.0, 110.0, RH));
        assert_eq!(menu.hovered(), Some(0));

        assert!(menu.set_hover_at(150.0, 150.0, RH));
        assert_eq!(menu.hovered(), Some(2));

        assert!(!menu.set_hover_at(160.0, 155.0, RH));
        assert_eq!(menu.hovered(), Some(2));

        assert!(menu.set_hover_at(50.0, 150.0, RH));
        assert_eq!(menu.hovered(), None);
    }
}
