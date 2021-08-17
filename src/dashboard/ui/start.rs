use crate::client::globalsettings::GlobalSettings;
use crate::common::error::HqError;
use crate::common::WrappedRcRefCell;
use crate::dashboard::dashboard_events::{DashboardEvent, DashboardEventHandler};
use crate::dashboard::dashboard_manager::THandler;
use crate::dashboard::dashboard_state::{CurrentActiveUi, DashboardState};
use crate::dashboard::ui::dashboard_ui::DashboardPainter;
use crate::rpc_call;
use crate::server::bootstrap::get_client_connection;
use crate::transfer::connection::ClientConnection;
use crate::transfer::messages::{FromClientMessage, ToClientMessage};
use std::cell::RefCell;
use std::rc::Rc;
use tako::messages::gateway::{CollectedOverview, OverviewRequest};
use tako::messages::worker::WorkerOverview;
use termion::event::Key;
use tui::layout::{Constraint, Direction, Layout};
use tui::style::{Color, Style};
use tui::widgets::{BarChart, Block, Borders};

//todo: uses events on the ui_state to update the ui, draws the ui!
pub async fn start_ui_loop(
    painter: WrappedRcRefCell<DashboardPainter>,
    event_handler: &DashboardEventHandler,
    state: WrappedRcRefCell<DashboardState>,
    global_settings: &GlobalSettings,
) -> Result<(), anyhow::Error> {
    loop {
        match event_handler.receive_event()? {
            DashboardEvent::KeyPressEvent(input) => {
                if input == Key::Char('q') {
                    /// Quits the dashboard
                    break Ok(());
                }
            }

            DashboardEvent::Tick => {
                let overview = get_hw_overview(global_settings).await?;

                ///Draw the correct dashboard ui according to the current ui state
                match state.get().ui_state {
                    CurrentActiveUi::WorkerHwMonitorScreen => {
                        painter.get_mut().draw_dashboard(overview);
                    }
                }
            }

            DashboardEvent::ChangeUIStateEvent(new_state) => {
                //todo: change what is being drawn on the dashboard by changing the ui state!
            }
        }
    }
}

async fn get_hw_overview(global_settings: &GlobalSettings) -> Result<CollectedOverview, HqError> {
    let mut connection = get_client_connection(global_settings.server_directory()).await?;

    let response = rpc_call!(
        connection,
        FromClientMessage::GetCollectedOverview(OverviewRequest {
            enable_hw_overview: true
        }),
        ToClientMessage::OverviewResponse(response) => response
    )
    .await;
    response
}
