use tako::messages::gateway::CollectedOverview;

//todo: keep a sender to send new data events to
pub struct DashboardState {
    pub config: Option<DashboardConfig>, //todo: this has to be some general structure for dashboard configuration.
    pub ui_state: CurrentActiveUi,
}

/**
 * What is currently being actively drawn on the dashboard
 * fixme: Ui directly polls for data for now
 * This is changed upon a KeyEvent
 */
pub enum CurrentActiveUi {
    WorkerHwMonitorScreen,
}

#[derive(Debug, Default)]
pub struct DashboardConfig {
    //todo: things like color, tick frequency, etc...
}

impl DashboardState {
    pub fn new() -> DashboardState {
        //todo: create empty vector for the initial screen for WorkerHWMonitoringData, pass it
        Self {
            config: None,
            ui_state: CurrentActiveUi::WorkerHwMonitorScreen, //default initial screen for the dashboard
        }
    }
}
