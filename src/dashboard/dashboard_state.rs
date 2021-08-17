pub struct DashboardState {
    pub config: Option<DashboardConfig>, //todo: this has to be some general structure for dashboard configuration.
    pub ui_state: CurrentActiveUi,
}

/**
 * What is currently being actively drawn on the dashboard
 * fixme: Ui directly polls for data for now
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
        Self {
            config: None,
            ui_state: CurrentActiveUi::WorkerHwMonitorScreen, //default initial screen for the dashboard
        }
    }
}
