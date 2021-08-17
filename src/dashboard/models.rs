use tui::widgets::TableState;

pub trait Scrollable {
    fn handle_scroll(&mut self, up: bool, page: bool) {
        // support page up/down
        let inc_or_dec = if page { 10 } else { 1 };
        if up {
            self.scroll_up(inc_or_dec);
        } else {
            self.scroll_down(inc_or_dec);
        }
    }
    fn scroll_down(&mut self, inc_or_dec: usize);
    fn scroll_up(&mut self, inc_or_dec: usize);
}

#[derive(Clone)]
pub struct StatefulTable<T> {
    pub state: TableState,
    pub items: Vec<T>,
}

impl<T> StatefulTable<T> {
    pub fn new() -> StatefulTable<T> {
        StatefulTable {
            state: TableState::default(),
            items: Vec::new(),
        }
    }

    pub fn set_items(&mut self, items: Vec<T>) {
        let item_len = items.len();
        self.items = items;
        if !self.items.is_empty() {
            let i = self.state.selected().map_or(0, |i| {
                if i > 0 && i < item_len {
                    i
                } else if i >= item_len {
                    item_len - 1
                } else {
                    0
                }
            });
            self.state.select(Some(i));
        }
    }
}

impl<T> Scrollable for StatefulTable<T> {
    fn scroll_down(&mut self, increment: usize) {
        if let Some(i) = self.state.selected() {
            if (i + increment) < self.items.len() {
                self.state.select(Some(i + increment));
            } else {
                self.state.select(Some(self.items.len().saturating_sub(1)));
            }
        }
    }

    fn scroll_up(&mut self, decrement: usize) {
        if let Some(i) = self.state.selected() {
            if i != 0 {
                self.state.select(Some(i.saturating_sub(decrement)));
            }
        }
    }
}
