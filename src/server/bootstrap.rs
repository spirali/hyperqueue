use std::future::Future;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Context;
use cli_table::{print_stdout, Cell, Style, Table};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::LocalSet;

use crate::client::globalsettings::GlobalSettings;
use crate::common::serverdir::{AccessRecord, ServerDir, SYMLINK_PATH};
use crate::common::setup::setup_interrupt;
use crate::server::rpc::Backend;
use crate::server::state::StateRef;
use crate::transfer::auth::generate_key;
use crate::transfer::connection::{ClientConnection, HqConnection};
use std::time::Duration;

const DEFAULT_AUTOALLOC_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

enum ServerStatus {
    Offline(AccessRecord),
    Online(AccessRecord),
}

pub struct ServerConfig {
    pub host: String,
    pub idle_timeout: Option<Duration>,
    pub autoalloc_interval: Option<Duration>,
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
        Err(_) | Ok(ServerStatus::Offline(_)) => {
            log::info!("No online server found, starting a new server");
            start_server(gsettings, server_cfg).await
        }
        Ok(ServerStatus::Online(_)) => anyhow::bail!(
            "Server at {0} is already online, please stop it first using \
            `hq stop --server-dir {0}`",
            gsettings.server_directory().display()
        ),
    }
}

pub async fn get_client_connection(server_directory: &Path) -> anyhow::Result<ClientConnection> {
    let sd = ServerDir::open(server_directory).context("Invalid server directory")?;
    let access_record = sd.read_access_record().with_context(|| {
        format!(
            "No running instance of HQ found (cannot read access record from {:?})",
            sd.access_filename()
        )
    })?;
    let connection = HqConnection::connect_to_server(&access_record)
        .await
        .with_context(|| {
            format!(
                "Cannot connect to HQ server at port {}:{}",
                access_record.host(),
                access_record.server_port()
            )
        })?;
    Ok(connection)
}

async fn get_server_status(server_directory: &Path) -> crate::Result<ServerStatus> {
    let record = ServerDir::open(server_directory).and_then(|sd| sd.read_access_record())?;

    if HqConnection::connect_to_server(&record).await.is_err() {
        return Ok(ServerStatus::Offline(record));
    }

    Ok(ServerStatus::Online(record))
}

async fn initialize_server(
    gsettings: &GlobalSettings,
    end_flag: Arc<Notify>,
    server_cfg: ServerConfig,
) -> anyhow::Result<impl Future<Output = anyhow::Result<()>>> {
    let server_directory = gsettings.server_directory();
    let client_listener = TcpListener::bind("0.0.0.0:0")
        .await
        .with_context(|| "Cannot create HQ server socket".to_string())?;
    let server_port = client_listener.local_addr()?.port();

    let hq_secret_key = Arc::new(generate_key());
    let tako_secret_key = Arc::new(generate_key());

    let state_ref = StateRef::new(
        server_cfg
            .autoalloc_interval
            .unwrap_or(DEFAULT_AUTOALLOC_REFRESH_INTERVAL),
    );
    let (tako_server, tako_future) = Backend::start(
        state_ref.clone(),
        tako_secret_key.clone(),
        server_cfg.idle_timeout,
    )
    .await?;

    let record = AccessRecord::new(
        server_cfg.host,
        server_port,
        tako_server.worker_port(),
        hq_secret_key.clone(),
        tako_secret_key.clone(),
    );

    let server_dir = ServerDir::create(server_directory, &record)?;
    print_access_record(gsettings, server_directory, &record);

    let stop_notify = Rc::new(Notify::new());
    let stop_cloned = stop_notify.clone();

    let key = hq_secret_key;
    let fut = async move {
        tokio::select! {
            _ = end_flag.notified() => {
                log::info!("Received SIGINT");
                Ok(())
            },
            _ = stop_notify.notified() => {
                log::info!("Stopping after Stop command from client");
                Ok(())
            },
            _ = crate::server::client::handle_client_connections(
                state_ref.clone(),
                tako_server,
                server_dir,
                client_listener,
                stop_cloned,
                key
            ) => { Ok(()) }
            _ = crate::server::autoalloc::autoalloc_process(state_ref) => { Ok(()) }
            r = tako_future => { r.map_err(|e| e.into()) }
        }
    };
    Ok(fut)
}

async fn start_server(
    gsettings: &GlobalSettings,
    server_config: ServerConfig,
) -> anyhow::Result<()> {
    let end_flag = setup_interrupt();
    let fut = initialize_server(gsettings, end_flag, server_config).await?;
    let local_set = LocalSet::new();
    local_set.run_until(fut).await?;

    // Delete symlink to mark that the server is no longer active
    std::fs::remove_file(gsettings.server_directory().join(SYMLINK_PATH)).ok();

    Ok(())
}

pub fn print_access_record(gsettings: &GlobalSettings, server_dir: &Path, record: &AccessRecord) {
    let rows = vec![
        vec![
            "Server directory".cell().bold(true),
            server_dir.display().cell(),
        ],
        vec!["Host".cell().bold(true), record.host().cell()],
        vec!["Pid".cell().bold(true), record.pid().cell()],
        vec!["HQ port".cell().bold(true), record.server_port().cell()],
        vec![
            "Workers port".cell().bold(true),
            record.worker_port().cell(),
        ],
        vec![
            "Start date".cell().bold(true),
            record.start_date().format("%F %T %Z").cell(),
        ],
        vec!["Version".cell().bold(true), record.version().cell()],
    ];
    let table = rows.table().color_choice(gsettings.color_policy());
    assert!(print_stdout(table).is_ok());
}

pub async fn print_server_info(gsettings: &GlobalSettings) -> anyhow::Result<()> {
    match get_server_status(gsettings.server_directory()).await {
        Err(_) | Ok(ServerStatus::Offline(_)) => anyhow::bail!("No online server found"),
        Ok(ServerStatus::Online(record)) => {
            print_access_record(gsettings, gsettings.server_directory(), &record)
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempdir::TempDir;
    use tokio::sync::Notify;

    use crate::client::commands::stop::stop_server;
    use crate::common::fsutils::test_utils::run_concurrent;
    use crate::common::serverdir::{store_access_record, AccessRecord, ServerDir, SYMLINK_PATH};
    use crate::server::bootstrap::{
        get_client_connection, get_server_status, initialize_server, ServerConfig,
    };

    use super::ServerStatus;
    use crate::client::globalsettings::GlobalSettings;
    use cli_table::ColorChoice;
    use std::future::Future;
    use std::path::Path;

    pub async fn init_test_server(
        tmp_dir: &Path,
    ) -> (impl Future<Output = anyhow::Result<()>>, Arc<Notify>) {
        let gsettings = GlobalSettings::new(tmp_dir.to_path_buf(), ColorChoice::Never);
        let server_cfg = ServerConfig {
            host: "localhost".to_string(),
            idle_timeout: None,
            autoalloc_interval: None,
        };
        let notify = Arc::new(Notify::new());
        (
            initialize_server(&gsettings, notify.clone(), server_cfg)
                .await
                .unwrap(),
            notify,
        )
    }

    #[tokio::test]
    async fn test_status_empty_directory() {
        let tmp_dir = TempDir::new("foo").unwrap();
        assert!(get_server_status(&tmp_dir.into_path()).await.is_err());
    }

    #[tokio::test]
    async fn test_status_directory_with_access_file() {
        let tmp_dir = TempDir::new("foo").unwrap();
        let tmp_path = tmp_dir.into_path();
        let server_dir = ServerDir::open(&tmp_path).unwrap();
        let record =
            AccessRecord::new("foo".into(), 42, 43, Default::default(), Default::default());
        store_access_record(&record, server_dir.access_filename()).unwrap();

        let res = get_server_status(&tmp_path).await.unwrap();
        assert!(matches!(res, ServerStatus::Offline(_)));
    }

    #[tokio::test]
    async fn test_status_directory_with_symlink() {
        let tmp_dir = TempDir::new("foo").unwrap().into_path();
        let actual_dir = tmp_dir.join("server-dir");
        std::fs::create_dir(&actual_dir).unwrap();
        std::os::unix::fs::symlink(&actual_dir, tmp_dir.join(SYMLINK_PATH)).unwrap();

        let server_dir = ServerDir::open(&actual_dir).unwrap();
        let record =
            AccessRecord::new("foo".into(), 42, 43, Default::default(), Default::default());
        store_access_record(&record, server_dir.access_filename()).unwrap();

        //let server_dir = ServerDir::open(&tmp_dir).unwrap();
        let res = get_server_status(&tmp_dir).await.unwrap();
        assert!(matches!(res, ServerStatus::Offline(_)));
    }

    #[tokio::test]
    async fn test_start_stop() {
        let tmp_dir = TempDir::new("foo").unwrap().into_path();
        let (fut, _) = init_test_server(&tmp_dir).await;
        let (set, handle) = run_concurrent(fut, async {
            let mut connection = get_client_connection(&tmp_dir).await.unwrap();
            stop_server(&mut connection).await.unwrap();
        })
        .await;
        set.run_until(handle).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_stop_on_end_condition() {
        let tmp_dir = TempDir::new("foo").unwrap().into_path();
        let (fut, notify) = init_test_server(&tmp_dir).await;
        notify.notify_one();
        fut.await.unwrap();
    }
}
