use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use signal_hook::consts::{SIGINT, SIGTERM, SIGTSTP};
use signal_hook::iterator::Signals;
use tako::Error as DsError;
use tokio::net::lookup_host;
use tokio::sync::Notify;

use tako::worker::{run_worker, WorkerConfiguration};
use tako::WorkerId;

use crate::common::manager::info::{ManagerInfo, ManagerType};
use crate::common::manager::{pbs, slurm};
use crate::common::serverdir::ServerDir;
use crate::worker::start::{HqTaskLauncher, WORKER_EXTRA_PROCESS_PID};
use crate::worker::streamer::StreamerRef;

/// Listens for SIGTSTP, SIGINT or SIGTERM signals.
/// When any of these signals is received, it notifies the passed Notify object.
struct SignalThread {
    signal_thread: Option<std::thread::JoinHandle<()>>,
    signal_handle: signal_hook::iterator::Handle,
}

impl SignalThread {
    fn new(stop_flag: Arc<Notify>) -> Self {
        let mut signals =
            Signals::new([SIGTSTP, SIGINT, SIGTERM]).expect("Cannot create signal set");
        let signal_handle = signals.handle();
        let signal_thread = std::thread::spawn(move || {
            for signal in &mut signals {
                log::debug!("Received signal {signal}");
                stop_flag.notify_one();
            }
        });
        Self {
            signal_handle,
            signal_thread: Some(signal_thread),
        }
    }
}

impl Drop for SignalThread {
    fn drop(&mut self) {
        self.signal_handle.close();
        self.signal_thread
            .take()
            .unwrap()
            .join()
            .expect("Signal thread crashed");
    }
}

pub struct InitializedWorker {
    pub id: WorkerId,
    pub configuration: WorkerConfiguration,
    future: Pin<Box<dyn Future<Output = Result<(), DsError>>>>,
    // The thread will be dropped once the worker is dropped
    _signal_thread: SignalThread,
}

impl InitializedWorker {
    fn new(
        id: WorkerId,
        configuration: WorkerConfiguration,
        future: Pin<Box<dyn Future<Output = Result<(), DsError>>>>,
        signal_thread: SignalThread,
    ) -> Self {
        Self {
            id,
            configuration,
            future,
            _signal_thread: signal_thread,
        }
    }
}

impl InitializedWorker {
    pub async fn run(self) -> Result<(), DsError> {
        self.future.await
    }
}

pub async fn initialize_worker(
    server_directory: &Path,
    configuration: WorkerConfiguration,
) -> anyhow::Result<InitializedWorker> {
    log::info!("Starting hyperqueue worker {}", crate::HQ_VERSION);
    let server_dir = ServerDir::open(server_directory).context("Cannot load server directory")?;
    let record = server_dir.read_worker_access_record().with_context(|| {
        format!(
            "Cannot load access record from {:?}",
            server_dir.access_filename()
        )
    })?;

    std::fs::create_dir_all(&configuration.work_dir)?;

    let server_address = format!("{}:{}", record.worker.host, record.worker.port);
    log::info!("Connecting to: {}", server_address);

    let server_addresses = lookup_host(&server_address)
        .await
        .context(format!("Cannot resolve server address `{server_address}`"))?
        .collect::<Vec<_>>();
    log::debug!("Resolved server to addresses {server_addresses:?}");

    log::debug!("Starting Tako worker ...");
    let stop_flag = Arc::new(Notify::new());
    let ((worker_id, configuration), worker_future) = run_worker(
        server_addresses,
        configuration,
        Some(record.worker.secret_key.clone()),
        |server_uid, worker_id| {
            let streamer_ref = StreamerRef::new(server_uid, worker_id);
            Box::new(HqTaskLauncher::new(streamer_ref))
        },
        stop_flag.clone(),
    )
    .await?;

    let signal_thread = SignalThread::new(stop_flag);
    let worker = InitializedWorker::new(
        worker_id,
        configuration,
        Box::pin(worker_future),
        signal_thread,
    );
    Ok(worker)
}

/// Utility function that adds common data to an already created worker configuration.
pub fn finalize_configuration(conf: &mut WorkerConfiguration) {
    conf.extra.insert(
        WORKER_EXTRA_PROCESS_PID.to_string(),
        std::process::id().to_string(),
    );
}

pub fn try_get_pbs_info() -> anyhow::Result<ManagerInfo> {
    log::debug!("Detecting PBS environment");

    std::env::var("PBS_ENVIRONMENT")
        .map_err(|_| anyhow!("PBS_ENVIRONMENT not found. The process is not running under PBS"))?;

    let manager_job_id =
        std::env::var("PBS_JOBID").expect("PBS_JOBID not found in environment variables");

    let time_limit = match pbs::get_remaining_timelimit(&manager_job_id) {
        Ok(time_limit) => Some(time_limit),
        Err(error) => {
            log::warn!("Cannot get time-limit from PBS: {error:?}");
            None
        }
    };

    log::info!("PBS environment detected");

    Ok(ManagerInfo::new(
        ManagerType::Pbs,
        manager_job_id,
        time_limit,
    ))
}

pub fn try_get_slurm_info() -> anyhow::Result<ManagerInfo> {
    log::debug!("Detecting SLURM environment");

    let manager_job_id = std::env::var("SLURM_JOB_ID")
        .or_else(|_| std::env::var("SLURM_JOBID"))
        .map_err(|_| {
            anyhow!("SLURM_JOB_ID/SLURM_JOBID not found. The process is not running under SLURM")
        })?;

    let duration = slurm::get_remaining_timelimit(&manager_job_id)
        .expect("Could not get remaining time from scontrol");

    log::info!("SLURM environment detected");

    Ok(ManagerInfo::new(
        ManagerType::Slurm,
        manager_job_id,
        Some(duration),
    ))
}
