use crate::common::utils::str::pluralize;
use crate::server::event::journal::prune::prune_journal;
use crate::server::event::journal::write::JournalWriter;
use crate::server::event::journal::JournalReader;
use crate::server::event::payload::EventPayload;
use crate::server::event::Event;
use crate::JobId;
use std::ffi::OsString;
use std::fs::{remove_file, rename};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tako::{Set, WorkerId};
use tokio::sync::mpsc;

pub enum EventStreamMessage {
    Event(Event),
    RegisterListener(mpsc::UnboundedSender<Event>),
    PruneJournal {
        callback: tokio::sync::oneshot::Sender<()>,
        live_jobs: Set<JobId>,
        live_workers: Set<WorkerId>,
    },
    FlushJournal(tokio::sync::oneshot::Sender<()>),
}

pub type EventStreamSender = mpsc::UnboundedSender<EventStreamMessage>;
pub type EventStreamReceiver = mpsc::UnboundedReceiver<EventStreamMessage>;

fn create_event_stream_queue() -> (EventStreamSender, EventStreamReceiver) {
    mpsc::unbounded_channel()
}

/// Start event streaming into a log file.
/// Streaming is running on another thread to reduce overhead and interference.
///
/// Returns a future that resolves once the event streaming thread finishes.
/// The thread will finish if there is some I/O error or if the `receiver` is closed.
pub fn start_event_streaming(
    writer: JournalWriter,
    log_path: &Path,
    flush_period: Duration,
) -> (EventStreamSender, impl Future<Output = ()> + use<>) {
    let (tx, rx) = create_event_stream_queue();
    let log_path = log_path.to_path_buf();
    let handle = std::thread::spawn(move || {
        let process = streaming_process(writer, rx, &log_path, flush_period);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        if let Err(error) = runtime.block_on(process) {
            log::error!("Event streaming has ended with an error: {error:?}");
        } else {
            log::debug!("Event streaming has finished successfully");
        }
    });
    let end_fut = async move {
        handle.join().expect("Event streaming thread has crashed");
    };
    (tx, end_fut)
}

async fn streaming_process(
    mut writer: JournalWriter,
    mut receiver: EventStreamReceiver,
    journal_path: &Path,
    flush_period: Duration,
) -> anyhow::Result<()> {
    let mut flush_fut = tokio::time::interval(flush_period);
    let mut events = 0;
    loop {
        tokio::select! {
            _ = flush_fut.tick() => {
                writer.flush()?;
            }
            res = receiver.recv() => {
                match res {
                    Some(EventStreamMessage::Event(event)) => {
                        log::trace!("Event: {event:?}");
                        let end = matches!(event.payload, EventPayload::ServerStop);
                        writer.store(event)?;
                        events += 1;
                        if end {
                            writer.flush()?;
                            break
                        }
                    }
                    Some(EventStreamMessage::RegisterListener(tx)) => {
                        /* We are blocking the thread here, but it is intended.
                           But we are blocking just a thread managing log file, not the whole HQ
                           And while this read is performed, we cannot allow modification of the file,
                           so the writing to the file has to be paused anyway */
                        writer.flush()?;
                        let mut reader = JournalReader::open(journal_path)?;
                        for event in &mut reader {
                            tx.send(event?).unwrap()
                        }
                    },
                    Some(EventStreamMessage::PruneJournal { live_jobs, live_workers, callback })  => {
                        writer.flush()?;
                        drop(writer);
                        let mut tmp_path: OsString = journal_path.into();
                        tmp_path.push(".tmp");
                        let tmp_path: PathBuf = tmp_path.into();
                        {
                            let mut reader = JournalReader::open(journal_path)?;
                            let mut writer = JournalWriter::create(&tmp_path)?;
                            if let Err(e) = prune_journal(&mut reader, &mut writer, &live_jobs, &live_workers) {
                                remove_file(&tmp_path)?;
                                return Err(e.into())
                            }
                        }
                        rename(&tmp_path, journal_path)?;
                        writer = JournalWriter::create_or_append(journal_path, None)?;
                        let _ = callback.send(());
                    },
                    Some(EventStreamMessage::FlushJournal(callback))  => {
                        writer.flush()?;
                        let _ = callback.send(());
                    },
                    None => break
                }
            }
        }
    }
    writer.finish()?;

    log::debug!(
        "Written {events} {} into the event log.",
        pluralize("event", events)
    );

    Ok(())
}
