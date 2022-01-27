use termion::event::Key;
use tokio::sync::mpsc::UnboundedSender;

use crate::dashboard::ui::screen::Screen;
use crate::dashboard::ui::screens::home::cluster_overview_chart::ClusterOverviewChart;
use crate::dashboard::ui::screens::home::worker_utilization_table::WorkerUtilTable;
use crate::dashboard::ui::styles::style_header_text;
use crate::dashboard::ui::terminal::DashboardFrame;
use crate::dashboard::ui::widgets::text::draw_text;

use crate::dashboard::data::DashboardData;
use crate::dashboard::events::DashboardEvent;
use crate::dashboard::state::DashboardScreenState;
use tako::WorkerId;
use tui::layout::{Constraint, Direction, Layout, Rect};

pub struct ClusterOverviewScreen {
    worker_util_table: WorkerUtilTable,
    cluster_overview: ClusterOverviewChart,
    screen_switcher: UnboundedSender<DashboardEvent>,
}

impl ClusterOverviewScreen {
    pub fn new(screen_controller: UnboundedSender<DashboardEvent>) -> Self {
        Self {
            worker_util_table: Default::default(),
            cluster_overview: Default::default(),
            screen_switcher: screen_controller,
        }
    }
}

impl Screen for ClusterOverviewScreen {
    fn draw(&mut self, frame: &mut DashboardFrame) {
        let layout = HomeLayout::new(frame);
        draw_text("HQ top", layout.header_chunk, frame, style_header_text());

        self.cluster_overview.draw(layout.worker_count_chunk, frame);
        self.worker_util_table
            .draw(layout.worker_util_table_chunk, frame);
    }

    fn update(&mut self, data: &DashboardData) {
        self.worker_util_table.update(data);
        self.cluster_overview.update(data);
    }

    /// Handles key presses for the components of the screen
    fn handle_key(&mut self, key: Key) {
        match key {
            Key::Down => self.worker_util_table.select_next_worker(),
            Key::Up => self.worker_util_table.select_previous_worker(),
            Key::Right => {
                if let Some(id) = self.worker_util_table.get_selected_item() {
                    change_to_worker_overview_screen(self.screen_switcher.clone(), id);
                    //todo: get rid of clone?
                }
            }
            _ => {}
        }
    }
}

fn change_to_worker_overview_screen(sender: UnboundedSender<DashboardEvent>, id: WorkerId) {
    if let Err(err) = sender.send(DashboardEvent::ScreenChange(
        DashboardScreenState::WorkerOverviewScreen(id),
    )) {
        log::error!("Error in switching screen: {}", err);
    }
}

/**
*  __________________________
   |     Chart |    Info   |
   |--------Header---------|
   |-----------------------|
   |          BODY         |
   -------------------------
 **/
struct HomeLayout {
    worker_count_chunk: Rect,
    _task_timeline_chart: Rect,
    header_chunk: Rect,
    worker_util_table_chunk: Rect,
}

impl HomeLayout {
    fn new(frame: &DashboardFrame) -> Self {
        let base_chunks = tui::layout::Layout::default()
            .constraints(vec![
                Constraint::Percentage(30),
                Constraint::Percentage(10),
                Constraint::Percentage(30),
            ])
            .direction(Direction::Vertical)
            .split(frame.size());

        let info_chunks = Layout::default()
            .constraints(vec![Constraint::Percentage(30), Constraint::Percentage(70)])
            .direction(Direction::Horizontal)
            .margin(0)
            .split(base_chunks[0]);

        Self {
            worker_count_chunk: info_chunks[0],
            _task_timeline_chart: info_chunks[1],
            header_chunk: base_chunks[1],
            worker_util_table_chunk: base_chunks[2],
        }
    }
}

pub fn vertical_chunks(constraints: Vec<Constraint>, size: Rect) -> Vec<Rect> {
    tui::layout::Layout::default()
        .constraints(constraints.as_ref())
        .direction(Direction::Vertical)
        .split(size)
}

pub fn horizontal_chunks_with_margin(
    constraints: Vec<Constraint>,
    size: Rect,
    margin: u16,
) -> Vec<Rect> {
    Layout::default()
        .constraints(constraints.as_ref())
        .direction(Direction::Horizontal)
        .margin(margin)
        .split(size)
}
