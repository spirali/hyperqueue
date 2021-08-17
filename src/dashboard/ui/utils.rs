use tui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Span, Spans, Text},
    widgets::{Block, Borders, Paragraph, Row},
    Frame,
};

const DARK_FG_COLOR: Color = Color::White;
const LIGHT_FG_COLOR: Color = Color::Magenta;

pub fn style_highlight() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub fn style_default(light: bool) -> Style {
    if light {
        Style::default().fg(LIGHT_FG_COLOR)
    } else {
        Style::default().fg(DARK_FG_COLOR)
    }
}

pub fn style_secondary() -> Style {
    Style::default().fg(Color::Yellow)
}

pub fn table_header_style(cells: Vec<&str>, light: bool) -> Row {
    Row::new(cells).style(style_default(light)).bottom_margin(0)
}

pub fn vertical_chunks(constraints: Vec<Constraint>, size: Rect) -> Vec<Rect> {
    Layout::default()
        .constraints(constraints.as_ref())
        .direction(Direction::Vertical)
        .split(size)
}

pub fn layout_block_top_border(title: Spans) -> Block {
    Block::default().borders(Borders::TOP).title(title)
}

pub fn title_with_dual_style<'a>(part_1: String, part_2: String, light: bool) -> Spans<'a> {
    Spans::from(vec![
        Span::styled(part_1, style_secondary().add_modifier(Modifier::BOLD)),
        Span::styled(part_2, style_default(light).add_modifier(Modifier::BOLD)),
    ])
}

pub fn loading<B: Backend>(f: &mut Frame<B>, block: Block, area: Rect, is_loading: bool) {
    if is_loading {
        let text = "\n\n Loading ...\n\n".to_owned();
        let mut text = Text::from(text);
        text.patch_style(style_secondary());

        // Contains the text
        let paragraph = Paragraph::new(text).style(style_secondary()).block(block);
        f.render_widget(paragraph, area);
    } else {
        f.render_widget(block, area)
    }
}
