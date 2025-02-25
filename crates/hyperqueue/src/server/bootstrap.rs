use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::LocalSet;

use crate::client::globalsettings::GlobalSettings;
use crate::common::error::HqError;
use crate::common::serverdir::{
    default_server_directory, ConnectAccessRecordPart, FullAccessRecord, ServerDir, SYMLINK_PATH,
};
use crate::server::autoalloc::{create_autoalloc_service, QueueId};
use crate::server::backend::Backend;
use crate::server::event::journal::start_event_streaming;
use crate::server::event::journal::JournalWriter;
use crate::server::event::streamer::EventStreamer;
use crate::server::restore::StateRestorer;
use crate::server::state::StateRef;
use crate::server::Senders;
use crate::transfer::auth::generate_key;
use crate::transfer::connection::ClientSession;
use crate::transfer::messages::ServerInfo;
use crate::HQ_VERSION;
use chrono::Utc;
use orion::kdf::SecretKey;
use rand::distributions::Alphanumeric;
use rand::Rng;
use std::time::Duration;
use tako::gateway::{FromGatewayMessage, ToGatewayMessage};
use tako::WorkerId;

enum ServerStatus {
    Offline,
    Online,
}

pub struct ServerConfig {
    pub client_host: String,
    pub worker_host: String,
    pub idle_timeout: Option<Duration>,
    pub client_port: Option<u16>,
    pub worker_port: Option<u16>,
    pub journal_path: Option<PathBuf>,
    pub journal_flush_period: Duration,
    pub worker_secret_key: Option<Arc<SecretKey>>,
    pub client_secret_key: Option<Arc<SecretKey>>,
    pub server_uid: Option<String>,
}

/// This function initializes the HQ server.
///
/// It takes a path to a directory and tries to find metadata of a running HQ server either directly
/// inside the directory or in a path linked to by a symlink named [`SYMLINK_PATH`].
///
/// If no metadata is found or the metadata links to a server that seems to be offline, a new
/// server will be started.
///
/// If an already running server is found, an error will be returned.
pub async fn init_hq_server(
    gsettings: &GlobalSettings,
    server_cfg: ServerConfig,
) -> anyhow::Result<()> {
    match get_server_status(gsettings.server_directory()).await {
        Err(_) | Ok(ServerStatus::Offline) => {
            log::info!("No online server found, starting a new server");
            start_server(gsettings, server_cfg).await
        }
        Ok(ServerStatus::Online) => anyhow::bail!(
            "Server at {0} is already online, please stop it first using \
            `hq server stop --server-dir {0}`",
            gsettings.server_directory().display()
        ),
    }
}

pub async fn get_client_session(server_directory: &Path) -> anyhow::Result<ClientSession> {
    let default_home = default_server_directory();
    let sd = ServerDir::open(server_directory).context("Invalid server directory")?;
    let server_dir_msg = if default_home != server_directory {
        format!(" --server-dir {}", server_directory.to_str().unwrap())
    } else {
        String::new()
    };
    let access_record_r = sd.read_client_access_record();

    if let Err(HqError::VersionError(msg)) = access_record_r {
        anyhow::bail!(msg);
    }
    let access_record = access_record_r.with_context(|| {
        format!(
            "No running instance of HQ found at {:?}.\n\
            Try to start the server: `hq server start{}` or use a different server directory.",
            sd.directory(),
            server_dir_msg,
        )
    })?;

    let session = ClientSession::connect_to_server(&access_record)
        .await
        .with_context(|| {
            format!(
                "Access token found but HQ server {}:{} is unreachable.\n\
                Try to (re)start the server using `hq server start{}`",
                access_record.client.host, access_record.client.port, server_dir_msg,
            )
        })?;

    Ok(session)
}

async fn get_server_status(server_directory: &Path) -> crate::Result<ServerStatus> {
    let record = ServerDir::open(server_directory).and_then(|sd| sd.read_client_access_record())?;

    if ClientSession::connect_to_server(&record).await.is_err() {
        return Ok(ServerStatus::Offline);
    }

    Ok(ServerStatus::Online)
}

pub fn generate_server_uid() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(char::from)
        .collect()
}

pub async fn initialize_server(
    gsettings: &GlobalSettings,
    mut server_cfg: ServerConfig,
    worker_id_initial_value: WorkerId,
    queue_id_initial_value: QueueId,
    truncate_log: Option<u64>,
) -> anyhow::Result<(
    impl Future<Output = anyhow::Result<()>> + use<>,
    Arc<Notify>,
    StateRef,
    Senders,
)> {
    let server_directory = gsettings.server_directory();

    let client_listener = TcpListener::bind(SocketAddr::new(
        Ipv4Addr::UNSPECIFIED.into(),
        server_cfg.client_port.unwrap_or(0),
    ))
    .await
    .with_context(|| "Cannot create HQ server socket".to_string())?;
    let client_port = client_listener.local_addr()?.port();

    let server_uid = server_cfg
        .server_uid
        .take()
        .unwrap_or_else(generate_server_uid);
    let worker_key = server_cfg
        .worker_secret_key
        .take()
        .unwrap_or_else(|| Arc::new(generate_key()));
    let client_key = server_cfg
        .client_secret_key
        .take()
        .unwrap_or_else(|| Arc::new(generate_key()));

    let state_ref = StateRef::new(ServerInfo {
        version: HQ_VERSION.to_string(),
        server_uid: server_uid.clone(),
        client_host: server_cfg.client_host.clone(),
        worker_host: server_cfg.worker_host.clone(),
        client_port,
        worker_port: 0, // Will be set later
        pid: std::process::id(),
        start_date: Utc::now(),
    });

    let (events, event_stream_fut) =
        prepare_event_management(&server_cfg, &server_uid, truncate_log).await?;

    let (autoalloc_service, autoalloc_process) =
        create_autoalloc_service(state_ref.clone(), queue_id_initial_value, events.clone());

    let (backend, comm_fut) = Backend::start(
        state_ref.clone(),
        events.clone(),
        autoalloc_service.clone(),
        worker_key.clone(),
        server_cfg.idle_timeout,
        server_cfg.worker_port,
        worker_id_initial_value,
    )
    .await?;

    let worker_port = backend.worker_port();

    state_ref.get_mut().set_worker_port(worker_port);

    let record = FullAccessRecord::new(
        ConnectAccessRecordPart {
            host: server_cfg.client_host,
            port: client_port,
            secret_key: client_key.clone(),
        },
        ConnectAccessRecordPart {
            host: server_cfg.worker_host,
            port: worker_port,
            secret_key: worker_key.clone(),
        },
        server_uid,
    );

    let server_dir = ServerDir::create(server_directory, &record)?;

    gsettings
        .printer()
        .print_server_description(Some(server_directory), state_ref.get().server_info());

    let end_flag = Arc::new(Notify::new());
    let end_flag_check = end_flag.clone();
    let end_flag_ret = end_flag.clone();

    let stop_check = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                log::info!("Received SIGINT, attempting to stop");
            }
            _ = end_flag_check.notified() => {
                log::info!("Received Stop command from client");
            }
        };
        log::info!("Stopping server");
    };

    let key = client_key;
    let state_ref2 = state_ref.clone();

    let senders = Senders {
        backend,
        events,
        autoalloc: autoalloc_service.clone(),
    };

    let senders2 = senders.clone();
    let fut = async move {
        tokio::pin! {
            let autoalloc_process = autoalloc_process;
        };
        let events = senders.events.clone();

        let result = tokio::select! {
            _ = stop_check => {
                Ok(())
            }
            _ = crate::server::client::handle_client_connections(
                state_ref.clone(),
                &senders,
                server_dir,
                client_listener,
                end_flag,
                key
            ) => { Ok(()) }
            _ = &mut autoalloc_process => { Ok(()) }
            r = comm_fut => { r.map_err(|e| e.into()) }
        };

        log::debug!("Shutting down auto allocation");
        autoalloc_service.quit_service();
        autoalloc_process.await;

        log::debug!("Shutting down event streaming");

        events.on_server_stop();
        event_stream_fut.await;

        result
    };
    Ok((fut, end_flag_ret, state_ref2, senders2))
}

async fn prepare_event_management(
    server_cfg: &ServerConfig,
    server_uid: &str,
    truncate_log: Option<u64>,
) -> anyhow::Result<(EventStreamer, Pin<Box<dyn Future<Output = ()>>>)> {
    Ok(if let Some(ref log_path) = server_cfg.journal_path {
        let writer = JournalWriter::create_or_append(log_path, truncate_log).map_err(|error| {
            anyhow!(
                "Cannot create event log file at `{}`: {error:?}",
                log_path.display()
            )
        })?;

        let (tx, stream_fut) =
            start_event_streaming(writer, log_path, server_cfg.journal_flush_period);
        let streamer = EventStreamer::new(Some(tx));
        streamer.on_server_start(server_uid);
        (streamer, Box::pin(stream_fut))
    } else {
        (
            EventStreamer::new(None),
            Box::pin(futures::future::ready(())),
        )
    })
}

async fn start_server(
    gsettings: &GlobalSettings,
    mut server_cfg: ServerConfig,
) -> anyhow::Result<()> {
    let restorer = if server_cfg.journal_path.as_ref().is_some_and(|p| p.exists()) {
        let mut restorer = StateRestorer::default();
        restorer.load_event_file(server_cfg.journal_path.as_ref().unwrap())?;
        if restorer.truncate_size().is_some() {
            log::warn!("Journal contains not fully written data; they will removed from the log");
        }
        let server_uid = restorer.take_server_uid();
        if !server_uid.is_empty() {
            server_cfg.server_uid = Some(server_uid)
        }
        Some(restorer)
    } else {
        None
    };

    let (fut, _, state_ref, senders) = initialize_server(
        gsettings,
        server_cfg,
        restorer
            .as_ref()
            .map(|r| r.worker_id_counter())
            .unwrap_or(WorkerId::new(0)),
        restorer.as_ref().map(|r| r.queue_id_counter()).unwrap_or(1),
        restorer.as_ref().and_then(|r| r.truncate_size()),
    )
    .await?;
    let new_tasks_and_queues = if let Some(restorer) = restorer {
        // This is early state recovery, we restore jobs later as we start futures because restoring
        // jobs already needs a running Tako
        let mut state = state_ref.get_mut();
        state.restore_state(&restorer);
        Some(restorer.restore_jobs_and_queues(&mut state)?)
    } else {
        None
    };
    let local_set = LocalSet::new();

    if let Some((new_tasks, new_queues)) = new_tasks_and_queues {
        let server_dir = gsettings.server_directory().to_path_buf();
        local_set.spawn_local(async move {
            log::debug!("Restoring old tasks into Tako");
            for msg in new_tasks {
                match senders
                    .backend
                    .send_tako_message(FromGatewayMessage::NewTasks(msg))
                    .await
                    .unwrap()
                {
                    ToGatewayMessage::NewTasksResponse(_) => { /* Ok */ }
                    r => panic!("Invalid response: {r:?}"),
                };
            }
            log::debug!("Restoration of old tasks is completed");
            for queue in new_queues {
                senders
                    .autoalloc
                    .add_queue(&server_dir, *queue.params, Some(queue.queue_id))
                    .await
                    .unwrap();
            }
            log::debug!("Restoration of old queues is completed");
        });
    };
    local_set.run_until(fut).await?;

    // Delete symlink to mark that the server is no longer active
    std::fs::remove_file(gsettings.server_directory().join(SYMLINK_PATH)).ok();

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::common::serverdir::{
        store_access_record, ConnectAccessRecordPart, FullAccessRecord, ServerDir, SYMLINK_PATH,
    };
    use crate::server::bootstrap::get_server_status;

    use super::ServerStatus;
    use crate::client::server::client_stop_server;
    use crate::tests::server::run_hq_test;
    use crate::transfer::auth::generate_key;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_status_empty_directory() {
        let tmp_dir = TempDir::with_prefix("foo").unwrap();
        assert!(get_server_status(&tmp_dir.into_path()).await.is_err());
    }

    #[tokio::test]
    async fn test_status_directory_with_access_file() {
        let tmp_dir = TempDir::with_prefix("foo").unwrap();
        let tmp_path = tmp_dir.into_path();
        let server_dir = ServerDir::open(&tmp_path).unwrap();
        let record = FullAccessRecord::new(
            ConnectAccessRecordPart {
                host: "foo".into(),
                port: 42,
                secret_key: Arc::new(generate_key()),
            },
            ConnectAccessRecordPart {
                host: "bar".into(),
                port: 42,
                secret_key: Arc::new(generate_key()),
            },
            "testHQ".into(),
        );
        store_access_record(&record, server_dir.access_filename()).unwrap();

        let res = get_server_status(&tmp_path).await.unwrap();
        assert!(matches!(res, ServerStatus::Offline));
    }

    #[tokio::test]
    async fn test_status_directory_with_symlink() {
        let tmp_dir = TempDir::with_prefix("foo").unwrap().into_path();
        let actual_dir = tmp_dir.join("server-dir");
        std::fs::create_dir(&actual_dir).unwrap();
        std::os::unix::fs::symlink(&actual_dir, tmp_dir.join(SYMLINK_PATH)).unwrap();

        let server_dir = ServerDir::open(&actual_dir).unwrap();
        let record = FullAccessRecord::new(
            ConnectAccessRecordPart {
                host: "foo".into(),
                port: 42,
                secret_key: Arc::new(generate_key()),
            },
            ConnectAccessRecordPart {
                host: "bar".into(),
                port: 42,
                secret_key: Arc::new(generate_key()),
            },
            "testHQ".into(),
        );
        store_access_record(&record, server_dir.access_filename()).unwrap();

        let res = get_server_status(&tmp_dir).await.unwrap();
        assert!(matches!(res, ServerStatus::Offline));
    }

    #[tokio::test]
    async fn test_start_stop() {
        run_hq_test(|mut server| async move {
            server.do_not_notify();
            let mut session = server.client().await;
            client_stop_server(session.connection()).await?;
            Ok(server)
        })
        .await;
    }

    // Just a no-op test to check that the notify logic works.
    #[tokio::test]
    async fn test_stop_on_end_condition() {
        run_hq_test(|server| async move { Ok(server) }).await;
    }
}
