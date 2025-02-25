use bincode::Options;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::internal::common::error::DsError;
use crate::internal::common::resources::ResourceDescriptor;
use crate::internal::server::core::CoreRef;
use crate::internal::worker::configuration::OverviewConfiguration;
use crate::launcher::{StopReason, TaskBuildContext, TaskResult};
use crate::program::ProgramDefinition;
use crate::worker::WorkerConfiguration;
use derive_builder::Builder;
use orion::auth::SecretKey;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::task::LocalSet;

use crate::internal::worker::rpc::run_worker;
use crate::launcher::{command_from_definitions, TaskLaunchData, TaskLauncher};
use crate::resources::ResourceDescriptorItem;
use crate::worker::ServerLostPolicy;
use crate::WorkerId;

pub enum WorkerSecretKey {
    Server,
    Custom(Option<SecretKey>),
}

impl Default for WorkerSecretKey {
    fn default() -> Self {
        Self::Server
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct WorkerConfig {
    #[builder(default)]
    idle_timeout: Option<Duration>,
    #[builder(default = "Some(Duration::from_millis(250))")]
    send_overview_interval: Option<Duration>,
    #[builder(default)]
    secret_key: WorkerSecretKey,
    #[builder(default = "Duration::from_millis(250)")]
    heartbeat_interval: Duration,
    #[builder(
        default = "ResourceDescriptor::new(vec![ResourceDescriptorItem::range(\"cpus\", 0, 0)])"
    )]
    resources: ResourceDescriptor,
}

pub(super) fn create_worker_configuration(
    builder: WorkerConfigBuilder,
) -> (WorkerConfiguration, WorkerSecretKey) {
    let WorkerConfig {
        idle_timeout,
        send_overview_interval,
        secret_key,
        heartbeat_interval,
        resources,
    } = builder.build().unwrap();
    (
        WorkerConfiguration {
            resources: ResourceDescriptor {
                resources: resources.resources,
            },
            listen_address: "".to_string(),
            hostname: "".to_string(),
            group: "".to_string(),
            work_dir: Default::default(),
            heartbeat_interval,
            overview_configuration: send_overview_interval.map(|send_interval| {
                OverviewConfiguration {
                    send_interval,
                    gpu_families: Default::default(),
                }
            }),
            idle_timeout,
            on_server_lost: ServerLostPolicy::Stop,
            time_limit: None,
            extra: Default::default(),
        },
        secret_key,
    )
}

/// This data is available for tests
pub struct WorkerHandle {
    pub workdir: PathBuf,
    pub id: WorkerId,
    control_tx: tokio::sync::mpsc::Sender<WorkerControlMessage>,
}

impl WorkerHandle {
    pub async fn pause(&self) -> WorkerPauseToken {
        let (unpause_tx, unpause_rx) = tokio::sync::oneshot::channel();
        self.control_tx
            .send(WorkerControlMessage::Pause(unpause_rx))
            .await
            .unwrap_or_else(|_| panic!("Could not send pause message"));
        WorkerPauseToken { unpause_tx }
    }
}

#[allow(unused)]
pub struct WorkerPauseToken {
    unpause_tx: tokio::sync::oneshot::Sender<()>,
}

impl WorkerPauseToken {
    #[allow(unused)]
    pub fn resume(self) {
        self.unpause_tx.send(()).unwrap();
    }
}

/// This data has to live as long as the worker lives
pub struct WorkerContext {
    #[allow(dead_code)]
    tmpdir: TempDir,
    end_flag: tokio::sync::oneshot::Sender<()>,
    thread_handle: JoinHandle<()>,
}

impl WorkerContext {
    pub(super) async fn kill(self) {
        self.end_flag.send(()).unwrap();
        tokio::task::spawn_blocking(|| self.thread_handle.join().unwrap())
            .await
            .unwrap();
    }
}

enum WorkerControlMessage {
    Pause(tokio::sync::oneshot::Receiver<()>),
}

pub(super) async fn start_worker(
    core_ref: CoreRef,
    server_secret_key: Option<Arc<SecretKey>>,
    config: WorkerConfigBuilder,
) -> anyhow::Result<(WorkerHandle, WorkerContext)> {
    let port = core_ref.get().get_worker_listen_port();
    let (mut configuration, worker_secret_key) = create_worker_configuration(config);
    let tmpdir = TempDir::with_prefix("tako")?;
    let workdir = tmpdir.path().to_path_buf().join("work");

    configuration.work_dir.clone_from(&workdir);
    std::fs::create_dir_all(&configuration.work_dir).unwrap();

    let server_address: SocketAddr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);

    let (id_tx, id_rx) = tokio::sync::oneshot::channel();
    let (end_tx, mut end_rx) = tokio::sync::oneshot::channel();
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<WorkerControlMessage>(32);

    let secret_key = match worker_secret_key {
        WorkerSecretKey::Server => server_secret_key,
        WorkerSecretKey::Custom(key) => key.map(Arc::new),
    };

    let thread_handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let future = async move {
            let result = run_worker(
                vec![server_address],
                configuration,
                secret_key,
                |_server_uid, _worker_id| Box::new(TestTaskLauncher),
                Arc::new(Notify::new()),
            )
            .await;

            match result {
                Ok(((worker_id, _), worker_future)) => {
                    id_tx.send(Ok(worker_id)).unwrap();

                    tokio::pin! {
                        let worker_future = worker_future;
                    }

                    loop {
                        tokio::select! {
                            _ = &mut worker_future => break,
                            _ = &mut end_rx => break,
                            msg = control_rx.recv() => {
                                if let Some(msg) = msg {
                                    match msg {
                                        WorkerControlMessage::Pause(until) => until.await.unwrap()
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    id_tx.send(Err(e)).unwrap();
                }
            }
        };

        let set = LocalSet::default();
        let set_future = set.run_until(future);

        runtime.block_on(set_future);
        runtime.block_on(set);
    });
    let worker_id = id_rx.await??;

    let handle = WorkerHandle {
        workdir,
        id: worker_id,
        control_tx,
    };
    let ctx = WorkerContext {
        tmpdir,
        thread_handle,
        end_flag: end_tx,
    };
    Ok((handle, ctx))
}

async fn launcher_main(program: ProgramDefinition) -> crate::Result<()> {
    let mut command = command_from_definitions(&program)?;
    let mut process = command.spawn()?;

    if !program.stdin.is_empty() {
        let mut stdin = process.stdin.take().unwrap();
        log::debug!("Writing {} bytes on task stdin", program.stdin.len());
        stdin.write_all(&program.stdin).await?;
    };

    let status = process.wait().await?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return crate::Result::Err(DsError::GenericError(format!(
            "Program terminated with exit code {}",
            code
        )));
    }
    Ok(())
}

struct TestTaskLauncher;

impl TaskLauncher for TestTaskLauncher {
    fn build_task(
        &self,
        ctx: TaskBuildContext,
        stop_receiver: tokio::sync::oneshot::Receiver<StopReason>,
    ) -> crate::Result<TaskLaunchData> {
        let program: ProgramDefinition = {
            log::debug!(
                "Starting program launcher task_id={} res={:?} alloc={:?} body_len={}",
                ctx.task_id(),
                ctx.resources(),
                ctx.allocation(),
                ctx.body().len(),
            );
            bincode::DefaultOptions::new()
                .deserialize(ctx.body())
                .map_err(|_| DsError::GenericError("Body deserialization failed".into()))?
        };

        Ok(TaskLaunchData::from_future(Box::pin(async move {
            tokio::select! {
                biased;
                r = stop_receiver => {
                    Ok(r.unwrap().into())
                }
                r = launcher_main(program) => {
                    r?;
                    Ok(TaskResult::Finished)
                }
            }
        })))
    }
}
