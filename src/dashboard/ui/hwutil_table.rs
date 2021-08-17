use std::io::Stdout;

use tako::messages::common::MemoryStats;
use tako::messages::gateway::CollectedOverview;
use tako::WorkerId;
use termion::input::MouseTerminal;
use termion::raw::RawTerminal;
use termion::screen::AlternateScreen;
use tui::backend::{Backend, TermionBackend};
use tui::layout::{Constraint, Rect};
use tui::widgets::{Cell, Row, Table};
use tui::Frame;

use crate::dashboard::models::StatefulTable;
use crate::dashboard::ui::utils::{
    layout_block_top_border, loading, style_highlight, table_header_style, title_with_dual_style,
};

static HIGHLIGHT: &str = "=> ";

struct WorkerUtilTableCols {
    id: WorkerId,
    num_tasks: i32,
    average_cpu_usage: f32,
    memory_usage: u64,
    collection_timestamp: u64,
}

struct ResourceTableProps<'a, T> {
    title: String,
    inline_help: String,
    resource: &'a mut StatefulTable<T>,
    table_headers: Vec<&'a str>,
    column_widths: Vec<Constraint>,
}

impl WorkerUtilTableCols {
    fn from(overview: CollectedOverview) -> Vec<Self> {
        let mut util_vec: Vec<WorkerUtilTableCols> = vec![];

        for overview in overview.worker_overviews {
            let mut col: WorkerUtilTableCols = Default::default();
            col.id = overview.id;
            col.num_tasks = overview.running_tasks.len() as i32; //fixme: is it correct?
            if let Some(hw_overview) = overview.hw_state {
                let num_cpus = hw_overview
                    .state
                    .worker_cpu_usage
                    .cpu_per_core_percent_usage
                    .len();
                let cpu_usage_sum_per_core = hw_overview
                    .state
                    .worker_cpu_usage
                    .cpu_per_core_percent_usage
                    .into_iter()
                    .reduce(|cpu_a, cpu_b| (cpu_a + cpu_b))
                    .unwrap();
                col.average_cpu_usage = cpu_usage_sum_per_core / num_cpus as f32;

                col.memory_usage =
                    calculate_memory_usage_percent(hw_overview.state.worker_memory_usage);

                col.collection_timestamp = hw_overview.state.timestamp;
            }

            util_vec.push(col);
        }
        util_vec
    }
}

impl Default for WorkerUtilTableCols {
    fn default() -> Self {
        Self {
            id: 0,
            num_tasks: 0,
            average_cpu_usage: 0.0,
            memory_usage: 0,
            collection_timestamp: 0,
        }
    }
}

pub fn worker_utilization_table(
    in_rect: Rect,
    frame: &mut Frame<TermionBackend<AlternateScreen<MouseTerminal<RawTerminal<Stdout>>>>>,
    data: CollectedOverview,
) {
    let mut stateful_table: StatefulTable<WorkerUtilTableCols> = StatefulTable::new();

    let cols = WorkerUtilTableCols::from(data);
    stateful_table.set_items(cols);

    draw_resource_block(
        frame,
        in_rect,
        ResourceTableProps {
            title: "Worker Hw Usage".to_string(),
            inline_help: "".to_string(),
            resource: &mut stateful_table,
            table_headers: vec!["WorkerId", "#Tasks", "Cpu Util", "Mem Util", "At Time"],
            column_widths: vec![
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ],
        },
        |data| {
            Row::new(vec![
                Cell::from(data.id.to_string().to_owned()),
                Cell::from(data.num_tasks.to_string().to_owned()),
                Cell::from(data.average_cpu_usage.to_string().to_owned()),
                Cell::from(data.memory_usage.to_string().to_owned()),
                Cell::from(data.collection_timestamp.to_string().to_owned()),
            ])
        },
        true,
        false,
    );
}

fn draw_resource_block<'a, B, T, F>(
    f: &mut Frame<B>,
    area: Rect,
    table_props: ResourceTableProps<'a, T>,
    row_cell_mapper: F,
    light_theme: bool,
    is_loading: bool,
) where
    B: Backend,
    F: Fn(&T) -> Row<'a>,
{
    let title = title_with_dual_style(table_props.title, table_props.inline_help, light_theme);
    let block = layout_block_top_border(title);

    if !table_props.resource.items.is_empty() {
        let rows = table_props
            .resource
            .items
            .iter()
            //   .map(|c| { Row::new(row_cell_mapper(c)) }.style(style_primary()));
            .map(row_cell_mapper);

        let table = Table::new(rows)
            .header(table_header_style(table_props.table_headers, light_theme))
            .block(block)
            .highlight_style(style_highlight())
            .highlight_symbol(HIGHLIGHT)
            .widths(&table_props.column_widths);

        f.render_stateful_widget(table, area, &mut table_props.resource.state);
    } else {
        loading(f, block, area, is_loading);
    }
}

fn calculate_memory_usage_percent(memory_stats: MemoryStats) -> u64 {
    (((memory_stats.free as f64) / (memory_stats.total as f64)) * 100.00) as u64
    //fixme: ugly
}
