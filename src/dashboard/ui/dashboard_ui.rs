use crate::dashboard::dashboard_manager::THandler;
use crate::dashboard::models::StatefulTable;
use crate::dashboard::ui::hwutil_table::worker_utilization_table;
use crate::dashboard::ui::utils::{
    layout_block_top_border, loading, style_highlight, table_header_style, title_with_dual_style,
    vertical_chunks,
};
use std::io::Stdout;
use tako::messages::gateway::CollectedOverview;
use termion::input::MouseTerminal;
use termion::raw::RawTerminal;
use termion::screen::AlternateScreen;
use tui::backend::{Backend, TermionBackend};
use tui::layout::{Alignment, Constraint, Rect};
use tui::text::Spans;
use tui::widgets::{BarChart, Block, Borders, Paragraph, Row, Table, Wrap};
use tui::Frame;

pub type FrameType = TermionBackend<AlternateScreen<MouseTerminal<RawTerminal<Stdout>>>>;

/**
 * keeps a handle of the terminal and draws to it
 *
 */
pub struct DashboardPainter {
    terminal: THandler,
    base_chunks: BaseUiChunks,
}

/**
*  _____________________
   |     HEADER        |
   |     BODY          |
   |     FOOTER        |
   ---------------------
**/
#[derive(Clone)]
pub struct BaseUiChunks {
    pub header_chunk: Rect, //todo: split into two
    pub footer_chunk: Rect,
    pub body: Rect,
}

impl DashboardPainter {
    pub fn init(mut terminal: THandler) -> Self {
        let frame = terminal.get_frame();
        let base_chunks = vertical_chunks(
            vec![
                Constraint::Percentage(20),
                Constraint::Percentage(70),
                Constraint::Percentage(10),
            ],
            frame.size(),
        );

        Self {
            terminal,
            base_chunks: BaseUiChunks {
                header_chunk: base_chunks[0],
                footer_chunk: base_chunks[2],
                body: base_chunks[1],
            },
        }
    }

    /// Initialises the dashboard ui and the base chunks
    pub fn draw_dashboard(&mut self, overviews: CollectedOverview) -> Result<(), ()> {
        let body_chunk = self.base_chunks.body.clone();
        let header_chunk = self.base_chunks.header_chunk.clone();

        self.terminal.draw(|terminal_frame| {
            draw_dashboard_header(header_chunk, terminal_frame);
            worker_utilization_table(body_chunk, terminal_frame, overviews);
        });
        Ok(())
    }
}

fn draw_dashboard_header(
    in_rect: Rect,
    frame: &mut Frame<TermionBackend<AlternateScreen<MouseTerminal<RawTerminal<Stdout>>>>>,
) -> Result<(), ()> {
    let text = vec![Spans::from("Hyperqueue Dashboard")];
    let paragraph = Paragraph::new(text)
        .block(Block::default())
        .alignment(Alignment::Right)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, in_rect);
    Ok(())
}
