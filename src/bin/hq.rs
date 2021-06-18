use std::path::PathBuf;
use std::str::FromStr;

use clap::{Clap, ValueHint};
use cli_table::ColorChoice;

use hyperqueue::client::commands::jobs::{cancel_job, output_job_detail, output_job_list};
use hyperqueue::client::commands::stop::stop_server;
use hyperqueue::client::commands::submit::{submit_computation, SubmitOpts};
use hyperqueue::client::commands::worker::{get_worker_list, stop_worker};
use hyperqueue::client::globalsettings::GlobalSettings;
use hyperqueue::client::job::Status;
use hyperqueue::client::worker::print_worker_info;
use hyperqueue::common::fsutils::absolute_path;
use hyperqueue::common::setup::setup_logging;
use hyperqueue::script::{command_script, ScriptOpts};
use hyperqueue::server::bootstrap::{get_client_connection, init_hq_server};
use hyperqueue::worker::hwdetect::{detect_resource, print_resource_descriptor};
use hyperqueue::worker::start::{start_hq_worker, WorkerStartOpts};
use hyperqueue::{JobId, WorkerId};

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// Common CLI options

#[derive(Clap)]
struct CommonOpts {
    /// Path to a directory that stores HyperQueue access files
    #[clap(long, global = true, value_hint = ValueHint::DirPath)]
    server_dir: Option<PathBuf>,

    /// Console color policy.
    #[clap(long, default_value = "auto", possible_values = & ["auto", "always", "never"])]
    colors: ColorPolicy,
}

// Root CLI options
#[derive(Clap)]
#[clap(about = "HyperQueue CLI")]
#[clap(author, about, version)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct Opts {
    #[clap(flatten)]
    common: CommonOpts,

    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Clap)]
enum SubCommand {
    /// Commands for controlling the HyperQueue server
    Server(ServerOpts),
    /// Display information about all jobs
    Jobs(JobListOpts),
    /// Display detailed information about a specific job
    Job(JobDetailOpts),
    /// Submit a job to HyperQueue
    Submit(SubmitOpts),
    /// Cancel a specific job
    Cancel(CancelOpts),
    /// Commands for controlling HyperQueue workers
    Worker(WorkerOpts),
    Script(ScriptOpts),
}

// Server CLI options
#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct ServerStartOpts {
    /// Hostname/IP of the machine under which is visible to others, default: hostname
    #[clap(long)]
    host: Option<String>,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct ServerStopOpts {}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct ServerOpts {
    #[clap(subcommand)]
    subcmd: ServerCommand,
}

#[derive(Clap)]
enum ServerCommand {
    /// Start the HyperQueue server
    Start(ServerStartOpts),
    /// Stop the HyperQueue server, if it is running
    Stop(ServerStopOpts),
}

// Worker CLI options
#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct WorkerStopOpts {
    worker_id: WorkerId,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct WorkerListOpts {
    /// shows running workers
    #[clap(long)]
    running: bool,

    /// shows offline workers
    #[clap(long)]
    offline: bool,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct WorkerOpts {
    #[clap(subcommand)]
    subcmd: WorkerCommand,
}

#[derive(Clap)]
enum WorkerCommand {
    /// Start worker
    Start(WorkerStartOpts),
    /// Stop worker
    Stop(WorkerStopOpts),
    /// Display information about all workers
    List(WorkerListOpts),
    /// Hwdetect
    Hwdetect,
}

// Job CLI options

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct JobListOpts {
    job_filters: Vec<Status>,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct JobDetailOpts {
    job_id: JobId,

    #[clap(long)]
    tasks: bool,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct CancelOpts {
    job_id: JobId,
}

// Commands
async fn command_server_start(
    gsettings: GlobalSettings,
    opts: ServerStartOpts,
) -> anyhow::Result<()> {
    init_hq_server(&gsettings, opts.host).await
}

async fn command_server_stop(
    gsettings: GlobalSettings,
    _opts: ServerStopOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;
    stop_server(&mut connection).await?;
    Ok(())
}

async fn command_job_list(gsettings: GlobalSettings, opts: JobListOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;
    output_job_list(&gsettings, &mut connection, opts.job_filters)
        .await
        .map_err(|e| e.into())
}

async fn command_job_detail(gsettings: GlobalSettings, opts: JobDetailOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;
    output_job_detail(&gsettings, &mut connection, opts.job_id, opts.tasks)
        .await
        .map_err(|e| e.into())
}

async fn command_submit(gsettings: GlobalSettings, opts: SubmitOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;
    submit_computation(&gsettings, &mut connection, opts).await
}

async fn command_cancel(gsettings: GlobalSettings, opts: CancelOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;
    cancel_job(&gsettings, &mut connection, opts.job_id)
        .await
        .map_err(|e| e.into())
}

async fn command_worker_start(
    gsettings: GlobalSettings,
    opts: WorkerStartOpts,
) -> anyhow::Result<()> {
    start_hq_worker(&gsettings, opts).await
}

async fn command_worker_stop(
    gsettings: GlobalSettings,
    opts: WorkerStopOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;
    stop_worker(&mut connection, opts.worker_id)
        .await
        .map_err(|e| e.into())
}

async fn command_worker_list(
    gsettings: GlobalSettings,
    opts: WorkerListOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(&gsettings.server_directory()).await?;

    // If --running and --offline was not set then show all workers
    let (online, offline) = if !opts.running && !opts.offline {
        (true, true)
    } else {
        (opts.running, opts.offline)
    };
    let workers = get_worker_list(&mut connection, online, offline).await?;
    print_worker_info(workers, &gsettings);
    Ok(())
}

fn command_worker_hwdetect() -> anyhow::Result<()> {
    let descriptor = detect_resource()?;
    print_resource_descriptor(&descriptor);
    Ok(())
}

pub enum ColorPolicy {
    Auto,
    Always,
    Never,
}

impl FromStr for ColorPolicy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "auto" => Self::Auto,
            "always" => Self::Always,
            "never" => Self::Never,
            _ => anyhow::bail!("Invalid color policy"),
        })
    }
}

fn make_global_settings(opts: CommonOpts) -> GlobalSettings {
    fn default_server_directory_path() -> PathBuf {
        let mut home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        home.push(".hq-server");
        home
    }

    let color_policy = match opts.colors {
        ColorPolicy::Always => ColorChoice::AlwaysAnsi,
        ColorPolicy::Auto => {
            if atty::is(atty::Stream::Stdout) {
                ColorChoice::Auto
            } else {
                ColorChoice::Never
            }
        }
        ColorPolicy::Never => ColorChoice::Never,
    };

    let server_dir = absolute_path(
        opts.server_dir
            .unwrap_or_else(default_server_directory_path),
    );

    GlobalSettings::new(server_dir, color_policy)
}

fn set_colored_settings(settings: &GlobalSettings) {
    match settings.color_policy() {
        ColorChoice::Always | ColorChoice::AlwaysAnsi => colored::control::set_override(true),
        ColorChoice::Never => colored::control::set_override(false),
        _ => {}
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> hyperqueue::Result<()> {
    let top_opts: Opts = Opts::parse();
    setup_logging();

    let gsettings = make_global_settings(top_opts.common);
    set_colored_settings(&gsettings);

    let result = match top_opts.subcmd {
        SubCommand::Server(ServerOpts {
            subcmd: ServerCommand::Start(opts),
        }) => command_server_start(gsettings, opts).await,
        SubCommand::Server(ServerOpts {
            subcmd: ServerCommand::Stop(opts),
        }) => command_server_stop(gsettings, opts).await,

        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Start(opts),
        }) => command_worker_start(gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Stop(opts),
        }) => command_worker_stop(gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::List(opts),
        }) => command_worker_list(gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Hwdetect,
        }) => command_worker_hwdetect(),
        SubCommand::Jobs(opts) => command_job_list(gsettings, opts).await,
        SubCommand::Job(opts) => command_job_detail(gsettings, opts).await,
        SubCommand::Submit(opts) => command_submit(gsettings, opts).await,
        SubCommand::Cancel(opts) => command_cancel(gsettings, opts).await,
        SubCommand::Script(opts) => command_script(gsettings, opts),
    };
    if let Err(e) = result {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }

    Ok(())
}
