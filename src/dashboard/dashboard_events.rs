use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::dashboard::dashboard_state::CurrentActiveUi;
use std::sync::mpsc::Sender;
use termion::event::Key;
use termion::input::TermRead;
use tokio::task::JoinHandle;

pub enum DashboardEvent {
    /// The event when a key is pressed
    KeyPressEvent(Key),
    /// Changes what is being drawn in the terminal
    ChangeUIStateEvent(CurrentActiveUi),
    /// Updates the dashboard with the latest data
    Tick,
}

/// A small event handler that wrap termion input and tick events. Each event
/// type is handled in its own thread and returned to a common `Receiver`
/// Should run an event loop that gets the new data and
pub struct DashboardEventHandler {
    receiver: mpsc::Receiver<DashboardEvent>,
    sender: mpsc::Sender<DashboardEvent>,
    //these insert data into the channel
    key_event_sender: thread::JoinHandle<()>,
    ui_clock_sender: thread::JoinHandle<()>,
}

impl DashboardEventHandler {
    pub fn new() -> DashboardEventHandler {
        let (tx, rx) = mpsc::channel();
        DashboardEventHandler {
            receiver: rx,
            sender: tx.clone(),
            ui_clock_sender: provide_clock(tx.clone()),
            key_event_sender: key_event_listener(tx),
        }
    }

    pub fn receive_event(&self) -> Result<DashboardEvent, mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn send_ui_state_update_event(&self, event: DashboardEvent) {
        self.sender.send(event); //todo: handle send error!
    }
}

///Periodic updates to the dashboard
fn provide_clock(tx: Sender<DashboardEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        if let Err(err) = tx.send(DashboardEvent::Tick) {
            eprintln!("{}", err);
            break;
        }
        thread::sleep(Duration::from_millis(250));
    })
}

///Handles key press events when the dashboard_ui is active
fn key_event_listener(tx: Sender<DashboardEvent>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let stdin = io::stdin();
        for evt in stdin.keys() {
            if let Ok(key) = evt {
                //todo: instead of sending KeyPressEvent, resolve here to the correct next state
                //todo: and send a ChangeUIStateEvent?

                if let Err(err) = tx.send(DashboardEvent::KeyPressEvent(key)) {
                    eprintln!("{}", err);
                    return;
                }
            }
        }
    })
}
