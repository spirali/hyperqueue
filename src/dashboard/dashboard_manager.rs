use std::io::Stdout;
use std::{error::Error, io};

use termion::raw::RawTerminal;
use termion::{input::MouseTerminal, raw::IntoRawMode, screen::AlternateScreen};
use tokio::task::LocalSet;
use tui::{backend::TermionBackend, Terminal};

use crate::client::globalsettings::GlobalSettings;
use crate::common::WrappedRcRefCell;
use crate::dashboard::dashboard_events::DashboardEventHandler;
use crate::dashboard::dashboard_state::DashboardState;
use crate::dashboard::ui::dashboard_ui::DashboardPainter;
use crate::dashboard::ui::start::start_ui_loop;

/// Allows drawing to the terminal
pub type THandler = Terminal<TermionBackend<AlternateScreen<MouseTerminal<RawTerminal<Stdout>>>>>;

/// The dashboard ui core structure
pub struct DashboardManager {
    pub dashboard_state: WrappedRcRefCell<DashboardState>,
    global_settings: GlobalSettings,
    terminal: WrappedRcRefCell<DashboardPainter>,
    event_handler: DashboardEventHandler,
}

impl DashboardManager {
    /// Initialises with the connection to the terminal and provides an event_handler to
    /// update the UI
    pub fn new(global_settings: GlobalSettings) -> Result<Self, io::Error> {
        let dashboard_state = WrappedRcRefCell::wrap(DashboardState::new());

        Ok(Self {
            dashboard_state,
            global_settings,
            terminal: WrappedRcRefCell::wrap(DashboardPainter::init(initialize_terminal()?)),
            event_handler: DashboardEventHandler::new(),
        })
    }

    /// Starts the tui
    pub async fn start_dashboard(self) -> Result<(), Box<dyn Error>> {
        let local_set = LocalSet::new();
        local_set
            .run_until(start_ui_loop(
                self.terminal,
                &self.event_handler,
                self.dashboard_state.clone(),
                &self.global_settings,
            ))
            .await;
        Ok(())
    }
}

fn initialize_terminal() -> futures::io::Result<THandler> {
    let stdout = AlternateScreen::from(MouseTerminal::from(io::stdout().into_raw_mode()?));
    let backend = TermionBackend::new(stdout);
    Terminal::new(backend)
}
