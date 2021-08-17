use futures::FutureExt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, Context};
use bstr::{BString, ByteSlice};
use clap::Clap;
use futures::TryFutureExt;
use humantime::format_rfc3339;
use tako::messages::common::WorkerConfiguration;
use tako::messages::common::{ProgramDefinition, StdioDef};
use tako::worker::launcher::{command_from_definitions, pin_program};
use tako::worker::rpc::run_worker;
use tako::worker::task::TaskRef;
use tempdir::TempDir;
use tokio::net::lookup_host;
use tokio::task::LocalSet;

use crate::client::globalsettings::GlobalSettings;
use crate::common::env::{HQ_CPUS, HQ_INSTANCE_ID, HQ_JOB_ID, HQ_PIN, HQ_SUBMIT_DIR, HQ_TASK_ID};
use crate::common::error::error;
use crate::common::manager::info::{ManagerInfo, ManagerType, WORKER_EXTRA_MANAGER_KEY};
use crate::common::manager::pbs;
use crate::common::serverdir::ServerDir;
use crate::common::timeutils::ArgDuration;
use crate::transfer::messages::TaskBody;
use crate::transfer::stream::ChannelId;
use crate::worker::hwdetect::detect_resource;
use crate::worker::output::print_worker_configuration;
use crate::worker::parser::parse_cpu_definition;
use crate::worker::streamer::StreamSender;
use crate::worker::streamer::StreamerRef;
use crate::{JobId, JobTaskId};
use hashbrown::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::ExitStatus;
use std::rc::Rc;
use std::time::Duration;
use tako::common::error::DsError;
use tako::worker::taskenv::{StopReason, TaskResult};
use tako::InstanceId;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

const STDIO_BUFFER_SIZE: usize = 16 * 1024; // 16kB

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

    /// How often should the worker announce its existence to the server. (default: "8s")
    #[clap(long, default_value = "8s")]
    heartbeat: ArgDuration,

    #[clap(long)]
    idle_timeout: Option<ArgDuration>,

    /// What HPC job manager should be used by the worker.
    #[clap(long, default_value = "detect", possible_values = &["detect", "slurm", "pbs", "none"])]
    manager: ManagerOpts,
}

/// Replace placeholders in user-defined program attributes
fn replace_placeholders(program: &mut ProgramDefinition) {
    let date = format_rfc3339(std::time::SystemTime::now()).to_string();
    let submit_dir = PathBuf::from(
        program.env[&BString::from(HQ_SUBMIT_DIR)]
            .to_os_str()
            .unwrap_or_default(),
    );

    let mut placeholder_map = HashMap::new();
    placeholder_map.insert(
        "%{JOB_ID}",
        program.env[&BString::from(HQ_JOB_ID)].to_string(),
    );
    placeholder_map.insert(
        "%{TASK_ID}",
        program.env[&BString::from(HQ_TASK_ID)].to_string(),
    );
    placeholder_map.insert(
        "%{INSTANCE_ID}",
        program.env[&BString::from(HQ_INSTANCE_ID)].to_string(),
    );
    placeholder_map.insert(
        "%{SUBMIT_DIR}",
        program.env[&BString::from(HQ_SUBMIT_DIR)].to_string(),
    );
    placeholder_map.insert("%{DATE}", date);

    let replace = |replacement_map: &HashMap<&str, String>, path: &PathBuf| -> PathBuf {
        let mut result: String = path.to_str().unwrap().into();
        for (placeholder, replacement) in replacement_map.iter() {
            result = result.replace(placeholder, replacement);
        }
        result.into()
    };

    // Replace CWD
    program.cwd = program
        .cwd
        .as_ref()
        .map(|cwd| submit_dir.join(replace(&placeholder_map, cwd)))
        .or_else(|| Some(std::env::current_dir().unwrap()));

    // Replace STDOUT and STDERR
    placeholder_map.insert(
        "%{CWD}",
        program.cwd.as_ref().unwrap().to_str().unwrap().to_string(),
    );

    program.stdout = std::mem::take(&mut program.stdout)
        .map_filename(|path| submit_dir.join(replace(&placeholder_map, &path)));
    program.stderr = std::mem::take(&mut program.stderr)
        .map_filename(|path| submit_dir.join(replace(&placeholder_map, &path)));
}

async fn resend_stdio(
    job_id: JobId,
    job_task_id: JobTaskId,
    channel: ChannelId,
    stdio: Option<impl tokio::io::AsyncRead + Unpin>,
    stream: Rc<StreamSender>,
) -> tako::Result<()> {
    if let Some(mut stdio) = stdio {
        log::debug!("Starting stream {}/{}/1", job_id, job_task_id);
        loop {
            let mut buffer = vec![0; STDIO_BUFFER_SIZE];
            let size = stdio.read(&mut buffer[..]).await?;
            if size == 0 {
                break;
            };
            buffer.truncate(size);
            stream.send_data(channel, buffer).await?;
        }
    }
    Ok(())
}

async fn launcher_main(
    streamer_ref: StreamerRef,
    task_ref: TaskRef,
    end_receiver: tokio::sync::oneshot::Receiver<StopReason>,
) -> tako::Result<TaskResult> {
    log::debug!(
        "Starting program launcher {} {:?} {:?}",
        task_ref.get().id,
        &task_ref.get().configuration.resources,
        task_ref.get().resource_allocation()
    );

    let (program, job_id, job_task_id, instance_id): (
        ProgramDefinition,
        JobId,
        JobTaskId,
        InstanceId,
    ) = {
        let task = task_ref.get();
        let body: TaskBody = tako::transfer::auth::deserialize(&task.configuration.body)?;
        let allocation = task
            .resource_allocation()
            .expect("Missing resource allocation for running task");
        let mut program = body.program;

        if body.pin {
            pin_program(&mut program, allocation);
            program.env.insert(HQ_PIN.into(), "1".into());
        }

        program
            .env
            .insert(HQ_CPUS.into(), allocation.comma_delimited_cpu_ids().into());

        program
            .env
            .insert(HQ_INSTANCE_ID.into(), task.instance_id.to_string().into());

        replace_placeholders(&mut program);
        (program, body.job_id, body.task_id, task.instance_id)
    };

    run_task(
        streamer_ref,
        &program,
        job_id,
        job_task_id,
        instance_id,
        end_receiver,
    )
    .await
}

/// Zero-worker mode measures pure overhead of HyperQueue.
/// In this mode the task is not executed at all.
#[cfg(feature = "zero-worker")]
async fn run_task(
    _streamer_ref: StreamerRef,
    _program: &ProgramDefinition,
    _job_id: JobId,
    _job_task_id: JobTaskId,
    _instance_id: InstanceId,
    _end_receiver: tokio::sync::oneshot::Receiver<StopReason>,
) -> tako::Result<TaskResult> {
    Ok(TaskResult::Finished)
}

#[cfg(not(feature = "zero-worker"))]
async fn run_task(
    streamer_ref: StreamerRef,
    program: &ProgramDefinition,
    job_id: JobId,
    job_task_id: JobTaskId,
    instance_id: InstanceId,
    end_receiver: tokio::sync::oneshot::Receiver<StopReason>,
) -> tako::Result<TaskResult> {
    let mut command = command_from_definitions(program)?;

    let status_to_result = |status: ExitStatus| {
        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return tako::Result::Err(DsError::GenericError(format!(
                "Program terminated with exit code {}",
                code
            )));
        } else {
            Ok(TaskResult::Finished)
        }
    };

    if matches!(program.stdout, StdioDef::Pipe) || matches!(program.stderr, StdioDef::Pipe) {
        let streamer_error =
            |e: DsError| DsError::GenericError(format!("Streamer: {:?}", e.to_string()));
        let mut child = command.spawn()?;
        let (close_sender, close_responder) = oneshot::channel();
        let stream = Rc::new(streamer_ref.get_mut().get_stream(
            &streamer_ref,
            job_id,
            job_task_id,
            instance_id,
            close_sender,
        ));

        stream.send_stream_start().await.map_err(streamer_error)?;

        let stream2 = stream.clone();

        let main_fut = async move {
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let response = tokio::try_join!(
                child.wait().map_err(DsError::from),
                resend_stdio(job_id, job_task_id, 0, stdout, stream2.clone())
                    .map_err(streamer_error),
                resend_stdio(job_id, job_task_id, 1, stderr, stream2).map_err(streamer_error),
            );
            status_to_result(response?.0)
        };

        let guard_fut = async move {
            let result = tokio::select! {
                biased;
                    r = end_receiver => {
                        Ok(r.unwrap().into())
                    }
                    r = main_fut => r
            };
            stream.close().await.map_err(streamer_error)?;
            result
        };

        Ok(tokio::try_join!(
            guard_fut,
            close_responder.map(|r| r
                .map_err(|_| DsError::GenericError("Connection to stream server closed".into()))?
                .map_err(streamer_error))
        )?
        .0)
    } else {
        tokio::select! {
            biased;
                r = end_receiver => {
                    Ok(r.unwrap().into())
                }
                r = command.status() => status_to_result(r?)
        }
    }
}

fn launcher(
    streamer_ref: &StreamerRef,
    task_ref: &TaskRef,
    end_receiver: tokio::sync::oneshot::Receiver<StopReason>,
) -> Pin<Box<dyn Future<Output = tako::Result<TaskResult>> + 'static>> {
    let task_ref = task_ref.clone();
    let streamer_ref = streamer_ref.clone();
    Box::pin(async move { launcher_main(streamer_ref, task_ref, end_receiver).await })
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

    let server_addr = lookup_host(&server_address)
        .await?
        .next()
        .expect("Invalid server address");

    log::debug!("Starting streamer ...");
    let (streamer_ref, streamer_future) = StreamerRef::start(
        Duration::from_secs(10),
        server_addr,
        record.tako_secret_key().clone(),
    );

    log::debug!("Starting Tako worker ...");
    let ((worker_id, configuration), worker_future) = run_worker(
        server_addr,
        configuration,
        Some(record.tako_secret_key().clone()),
        Box::new(move |task_ref, end_receiver| launcher(&streamer_ref, task_ref, end_receiver)),
    )
    .await?;
    print_worker_configuration(gsettings, worker_id, configuration);

    let local_set = LocalSet::new();
    local_set
        .run_until(async move {
            tokio::select! {
                () = worker_future => {}
                () = streamer_future => {}
            }
        })
        .await;
    Ok(())
}

fn try_get_pbs_info() -> anyhow::Result<ManagerInfo> {
    log::debug!("Detecting PBS environment");

    std::env::var("PBS_ENVIRONMENT")
        .map_err(|_| anyhow!("PBS_ENVIRONMENT not found. The process is not running under PBS"))?;

    let manager_job_id =
        std::env::var("PBS_JOBID").expect("PBS_JOBID not found in environment variables");

    let time_limit = pbs::get_remaining_timelimit(&manager_job_id).unwrap();

    log::info!("PBS environment detected");

    Ok(ManagerInfo::new(
        ManagerType::Pbs,
        manager_job_id,
        time_limit,
    ))
}

fn try_get_slurm_info() -> anyhow::Result<ManagerInfo> {
    log::debug!("Detecting SLURM environment");

    let manager_job_id = std::env::var("SLURM_JOB_ID")
        .or_else(|_| std::env::var("SLURM_JOBID"))
        .map_err(|_| {
            anyhow!("SLURM_JOB_ID/SLURM_JOBID not found. The process is not running under SLURM")
        })?;

    log::info!("SLURM environment detected");

    // TODO: Get walltime info
    let duration = Duration::from_secs(1);
    Ok(ManagerInfo::new(
        ManagerType::Slurm,
        manager_job_id,
        duration,
    ))
}

fn gather_manager_info(opts: ManagerOpts) -> anyhow::Result<Option<ManagerInfo>> {
    match opts {
        ManagerOpts::Detect => {
            log::debug!("Trying to detect manager");
            Ok(try_get_pbs_info().or_else(|_| try_get_slurm_info()).ok())
        }
        ManagerOpts::None => {
            log::debug!("Manager detection disabled");
            Ok(None)
        }
        ManagerOpts::Pbs => Ok(Some(try_get_pbs_info()?)),
        ManagerOpts::Slurm => Ok(Some(try_get_slurm_info()?)),
    }
}

fn gather_configuration(opts: WorkerStartOpts) -> anyhow::Result<WorkerConfiguration> {
    log::debug!("Gathering worker configuration information");

    let hostname = gethostname::gethostname()
        .into_string()
        .expect("Invalid hostname");

    let resources = opts
        .cpus
        .map(|cpus| parse_cpu_definition(&cpus))
        .unwrap_or_else(detect_resource)?;

    let (work_dir, log_dir) = {
        let tmpdir = TempDir::new("hq-worker").unwrap().into_path();
        (tmpdir.join("work"), tmpdir.join("logs"))
    };

    let manager_info = gather_manager_info(opts.manager)?;
    let mut extra = HashMap::new();
    if let Some(manager_info) = manager_info {
        extra.insert(
            WORKER_EXTRA_MANAGER_KEY.to_string(),
            serde_json::to_string(&manager_info)?,
        );
    }

    Ok(WorkerConfiguration {
        resources,
        listen_address: Default::default(), // Will be filled during init
        hostname,
        work_dir,
        log_dir,
        heartbeat_interval: opts.heartbeat.into_duration(),
        hw_state_poll_interval: Some(Duration::from_millis(1000)),
        idle_timeout: opts.idle_timeout.map(|x| x.into_duration()),
        extra,
    })
}

#[cfg(test)]
mod tests {
    use hashbrown::HashMap;
    use tako::messages::common::{ProgramDefinition, StdioDef};

    use crate::common::env::{HQ_INSTANCE_ID, HQ_JOB_ID, HQ_SUBMIT_DIR, HQ_TASK_ID};
    use crate::{JobId, JobTaskId};

    use super::replace_placeholders;

    #[test]
    fn test_replace_task_id() {
        let mut program = program_def(
            "dir-%{TASK_ID}",
            Some("%{TASK_ID}.out"),
            Some("%{TASK_ID}.err"),
            "",
            0,
            1,
        );
        replace_placeholders(&mut program);
        assert_eq!(program.cwd, Some("dir-1".into()));
        assert_eq!(program.stdout, StdioDef::File("1.out".into()));
        assert_eq!(program.stderr, StdioDef::File("1.err".into()));
    }

    #[test]
    fn test_replace_job_id() {
        let mut program = program_def(
            "dir-%{JOB_ID}-%{TASK_ID}",
            Some("%{JOB_ID}-%{TASK_ID}.out"),
            Some("%{JOB_ID}-%{TASK_ID}.err"),
            "",
            5,
            1,
        );
        replace_placeholders(&mut program);
        assert_eq!(program.cwd, Some("dir-5-1".into()));
        assert_eq!(program.stdout, StdioDef::File("5-1.out".into()));
        assert_eq!(program.stderr, StdioDef::File("5-1.err".into()));
    }

    #[test]
    fn test_replace_submit_dir() {
        let mut program = program_def(
            "%{SUBMIT_DIR}",
            Some("%{SUBMIT_DIR}/out"),
            Some("%{SUBMIT_DIR}/err"),
            "/submit-dir",
            5,
            1,
        );
        replace_placeholders(&mut program);

        assert_eq!(program.cwd, Some("/submit-dir".into()));
        assert_eq!(program.stdout, StdioDef::File("/submit-dir/out".into()));
        assert_eq!(program.stderr, StdioDef::File("/submit-dir/err".into()));
    }

    #[test]
    fn test_replace_cwd() {
        let mut program = program_def(
            "dir-%{JOB_ID}-%{TASK_ID}",
            Some("%{CWD}.out"),
            Some("%{CWD}.err"),
            "",
            5,
            1,
        );
        replace_placeholders(&mut program);
        assert_eq!(program.cwd, Some("dir-5-1".into()));
        assert_eq!(program.stdout, StdioDef::File("dir-5-1.out".into()));
        assert_eq!(program.stderr, StdioDef::File("dir-5-1.err".into()));
    }

    fn program_def(
        cwd: &str,
        stdout: Option<&str>,
        stderr: Option<&str>,
        submit_dir: &str,
        job_id: JobId,
        task_id: JobTaskId,
    ) -> ProgramDefinition {
        let mut env = HashMap::new();
        env.insert(HQ_SUBMIT_DIR.into(), submit_dir.into());
        env.insert(HQ_JOB_ID.into(), job_id.to_string().into());
        env.insert(HQ_TASK_ID.into(), task_id.to_string().into());
        env.insert(HQ_INSTANCE_ID.into(), "0".into());

        ProgramDefinition {
            args: vec![],
            env,
            stdout: stdout.map(|v| StdioDef::File(v.into())).unwrap_or_default(),
            stderr: stderr.map(|v| StdioDef::File(v.into())).unwrap_or_default(),
            cwd: Some(cwd.into()),
        }
    }
}
