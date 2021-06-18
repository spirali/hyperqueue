use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context};
use clap::Clap;
use tako::messages::common::{LauncherDefinition, ProgramDefinition, WorkerConfiguration};
use tako::worker::rpc::run_worker;
use tempdir::TempDir;
use tokio::task::LocalSet;

use crate::client::globalsettings::GlobalSettings;
use crate::common::env::create_hq_env;
use crate::common::error::error;
use crate::common::serverdir::ServerDir;
use crate::worker::hwdetect::detect_resource;
use crate::worker::output::print_worker_configuration;
use crate::worker::parser::parse_cpu_definition;
use crate::Map;
use tako::worker::launcher::pin_program;
use tako::worker::task::Task;

#[derive(Clap)]
pub enum ManagerOpts {
    Detect,
    None,
    Pbs,
    Slurm,
}

impl FromStr for ManagerOpts {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "detect" => Self::Detect,
            "none" => Self::None,
            "pbs" => Self::Pbs,
            "slurm" => Self::Slurm,
            _ => {
                return error(
                    "Invalid manager value. Allowed values are 'detect', 'none', 'pbs', 'slurm'"
                        .to_string(),
                )
            }
        })
    }
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
pub struct WorkerStartOpts {
    /// How many cores should be allocated for the worker
    #[clap(long)]
    cpus: Option<String>,

    /// How often should the worker announce its existence to the server. [ms]
    #[clap(long, default_value = "8000")]
    heartbeat: u32,

    /// What HPC job manager should be used by the worker.
    #[clap(long, default_value = "detect", possible_values = &["detect", "slurm", "pbs", "none"])]
    manager: ManagerOpts,
}

#[allow(clippy::unnecessary_wraps)]
fn launcher_setup(task: &Task, def: LauncherDefinition) -> tako::Result<ProgramDefinition> {
    let allocation = task
        .resource_allocation()
        .expect("Missing resource allocation for running task");
    let mut program = def.program;

    if def.pin {
        pin_program(&mut program, &allocation);
        program.env.insert(create_hq_env("PIN"), "1".into());
    }

    program.env.insert(
        create_hq_env("CPUS"),
        allocation.comma_delimited_cpu_ids().into(),
    );
    Ok(program)
}

pub async fn start_hq_worker(
    gsettings: &GlobalSettings,
    opts: WorkerStartOpts,
) -> anyhow::Result<()> {
    log::info!("Starting hyperqueue worker {}", env!("CARGO_PKG_VERSION"));
    let server_dir =
        ServerDir::open(gsettings.server_directory()).context("Cannot load server directory")?;
    let record = server_dir.read_access_record().with_context(|| {
        format!(
            "Cannot load access record from {:?}",
            server_dir.access_filename()
        )
    })?;
    let server_address = format!("{}:{}", record.host(), record.worker_port());
    log::info!("Connecting to: {}", server_address);

    let configuration = gather_configuration(opts)?;
    let ((worker_id, configuration), worker_future) = run_worker(
        &server_address,
        configuration,
        Some(record.tako_secret_key().clone()),
        Box::new(launcher_setup),
    )
    .await?;
    print_worker_configuration(gsettings, worker_id, configuration);
    let local_set = LocalSet::new();
    local_set.run_until(worker_future).await;
    Ok(())
}

fn try_get_pbs_info() -> anyhow::Result<Map<String, String>> {
    log::debug!("Detecting PBS environment");

    std::env::var("PBS_ENVIRONMENT")
        .map_err(|_| anyhow!("PBS_ENVIRONMENT not found. The process is not running under PBS"))?;

    let manager_job_id = std::env::var("PBS_JOBID").unwrap_or_else(|_| "unknown".to_string());

    let mut result = Map::with_capacity(2);
    result.insert("MANAGER".to_string(), "PBS".to_string());
    result.insert("MANAGER_JOB_ID".to_string(), manager_job_id);

    // TODO: Run "qstat -f -F json $PBS_JOBID" to get walltime

    log::info!("PBS environment detected");
    Ok(result)
}

fn try_get_slurm_info() -> anyhow::Result<Map<String, String>> {
    log::debug!("Detecting SLURM environment");

    let manager_job_id = std::env::var("SLURM_JOB_ID")
        .or_else(|_| std::env::var("SLURM_JOBID"))
        .map_err(|_| {
            anyhow!("SLURM_JOB_ID/SLURM_JOBID not found. The process is not running under SLURM")
        })?;

    let mut result = Map::with_capacity(2);
    result.insert("MANAGER".to_string(), "SLURM".to_string());
    result.insert("MANAGER_JOB_ID".to_string(), manager_job_id);

    // TODO: Get walltime info

    log::info!("SLURM environment detected");
    Ok(result)
}

fn gather_manager_info(opts: ManagerOpts) -> anyhow::Result<Map<String, String>> {
    match opts {
        ManagerOpts::Detect => {
            log::debug!("Trying to detect manager");
            try_get_pbs_info()
                .or_else(|_| try_get_slurm_info())
                .or_else(|_| Ok(Map::new()))
        }
        ManagerOpts::None => {
            log::debug!("Manager detection disabled");
            Ok(Map::new())
        }
        ManagerOpts::Pbs => try_get_pbs_info(),
        ManagerOpts::Slurm => try_get_slurm_info(),
    }
}

fn gather_configuration(opts: WorkerStartOpts) -> anyhow::Result<WorkerConfiguration> {
    let hostname = gethostname::gethostname()
        .into_string()
        .expect("Invalid hostname");

    let resources = opts
        .cpus
        .map(|cpus| parse_cpu_definition(&cpus))
        .unwrap_or_else(detect_resource)?;

    let heartbeat_interval = Duration::from_millis(opts.heartbeat as u64);

    let (work_dir, log_dir) = {
        let tmpdir = TempDir::new("hq-worker").unwrap().into_path();
        (tmpdir.join("work"), tmpdir.join("logs"))
    };

    let extra = gather_manager_info(opts.manager)?;

    Ok(WorkerConfiguration {
        resources,
        listen_address: Default::default(), // Will be filled during init
        hostname,
        work_dir,
        log_dir,
        heartbeat_interval,
        extra,
    })
}
