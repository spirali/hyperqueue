use std::path::PathBuf;
use std::str::FromStr;

use clap::{Clap, ValueHint};
use cli_table::ColorChoice;

use hyperqueue::client::commands::autoalloc::{command_autoalloc, AutoAllocOpts};
use hyperqueue::client::commands::jobs::{cancel_job, output_job_detail, output_job_list};
use hyperqueue::client::commands::log::{command_log, LogOpts};
use hyperqueue::client::commands::stats::print_server_stats;
use hyperqueue::client::commands::stop::stop_server;
use hyperqueue::client::commands::submit::{
    resubmit_computation, submit_computation, ResubmitOpts, SubmitOpts,
};
use hyperqueue::client::commands::wait::wait_for_job_with_selector;
use hyperqueue::client::commands::worker::{get_worker_info, get_worker_list, stop_worker};
use hyperqueue::client::globalsettings::GlobalSettings;
use hyperqueue::client::status::Status;
use hyperqueue::client::worker::print_worker_info;
use hyperqueue::common::arraydef::IntArray;
use hyperqueue::common::fsutils::absolute_path;
use hyperqueue::common::setup::setup_logging;
use hyperqueue::common::timeutils::ArgDuration;
use hyperqueue::server::bootstrap::{
    get_client_connection, init_hq_server, print_server_info, ServerConfig,
};
use hyperqueue::transfer::messages::Selector;
use hyperqueue::worker::hwdetect::{detect_resource, print_resource_descriptor};
use hyperqueue::worker::output::print_worker_configuration;
use hyperqueue::worker::start::{start_hq_worker, WorkerStartOpts};
use hyperqueue::WorkerId;

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

#[allow(clippy::large_enum_variant)]
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
    /// Resubmits all filtered tasks within a job
    Resubmit(ResubmitOpts),
    /// Waits until a job is ended
    Wait(WaitOpts),
    /// Operations with log
    Log(LogOpts),
    /// Auto allocation management
    AutoAlloc(AutoAllocOpts),
}

// Server CLI options
#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct ServerStartOpts {
    /// Hostname/IP of the machine under which is visible to others, default: hostname
    #[clap(long)]
    host: Option<String>,

    /// Duration after which will an idle worker automatically stop
    #[clap(long)]
    idle_timeout: Option<ArgDuration>,

    /// How often should the auto allocator perform its actions
    #[clap(long)]
    autoalloc_interval: Option<ArgDuration>,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct ServerStopOpts {}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct ServerInfoOpts {
    /// Show internal internal state of server
    #[clap(long)]
    stats: bool,
}

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
    /// Show info of running HyperQueue server
    Info(ServerInfoOpts),
}

enum SelectorArg {
    All,
    Last,
    Id(IntArray),
}

impl FromStr for SelectorArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "last" => Ok(SelectorArg::Last),
            "all" => Ok(SelectorArg::All),
            _ => Ok(SelectorArg::Id(IntArray::from_str(s)?)),
        }
    }
}

impl From<SelectorArg> for Selector {
    fn from(selector_arg: SelectorArg) -> Self {
        match selector_arg {
            SelectorArg::Id(array) => Selector::Specific(array),
            SelectorArg::Last => Selector::LastN(1),
            SelectorArg::All => Selector::All,
        }
    }
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
pub struct WaitOpts {
    /// Select job(s) to wait for
    selector_arg: SelectorArg,
}

// Worker CLI options
#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct WorkerStopOpts {
    /// Select worker(s) to stop
    selector_arg: SelectorArg,
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
struct WorkerAddressOpts {
    worker_id: WorkerId,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct WorkerOpts {
    #[clap(subcommand)]
    subcmd: WorkerCommand,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct WorkerInfoOpts {
    worker_id: WorkerId,
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
    /// Display info about worker
    Info(WorkerInfoOpts),
    /// Print worker's hostname
    Address(WorkerAddressOpts),
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
    /// Numeric job id or `last` to display the most recently submitted job
    selector_arg: SelectorArg,

    // Include task info in the output
    #[clap(long)]
    tasks: bool,
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct CancelOpts {
    /// Select job(s) to cancel
    selector_arg: SelectorArg,
}

// Commands
async fn command_server_start(
    gsettings: GlobalSettings,
    opts: ServerStartOpts,
) -> anyhow::Result<()> {
    let server_cfg = ServerConfig {
        host: opts
            .host
            .unwrap_or_else(|| gethostname::gethostname().into_string().unwrap()),
        idle_timeout: opts.idle_timeout.map(|x| x.into_duration()),
        autoalloc_interval: opts.autoalloc_interval.map(|x| x.into_duration()),
    };
    init_hq_server(&gsettings, server_cfg).await
}

async fn command_server_stop(
    gsettings: GlobalSettings,
    _opts: ServerStopOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    stop_server(&mut connection).await?;
    Ok(())
}

async fn command_server_info(
    gsettings: GlobalSettings,
    opts: ServerInfoOpts,
) -> anyhow::Result<()> {
    if opts.stats {
        let mut connection = get_client_connection(gsettings.server_directory()).await?;
        print_server_stats(&gsettings, &mut connection).await
    } else {
        print_server_info(&gsettings).await
    }
}

async fn command_job_list(gsettings: GlobalSettings, opts: JobListOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    output_job_list(&gsettings, &mut connection, opts.job_filters).await
}

async fn command_job_detail(gsettings: GlobalSettings, opts: JobDetailOpts) -> anyhow::Result<()> {
    if matches!(opts.selector_arg, SelectorArg::All) {
        log::warn!("Specifier all is not implemented for job details, did you mean: job list?");
        return Ok(());
    }

    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    output_job_detail(
        &gsettings,
        &mut connection,
        opts.selector_arg.into(),
        opts.tasks,
    )
    .await
}

async fn command_submit(gsettings: GlobalSettings, opts: SubmitOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    submit_computation(&gsettings, &mut connection, opts).await
}

async fn command_cancel(gsettings: GlobalSettings, opts: CancelOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;

    cancel_job(&gsettings, &mut connection, opts.selector_arg.into()).await
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
    let mut connection = get_client_connection(gsettings.server_directory()).await?;

    stop_worker(&mut connection, opts.selector_arg.into()).await?;
    Ok(())
}

async fn command_worker_list(
    gsettings: GlobalSettings,
    opts: WorkerListOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;

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

async fn command_worker_info(
    gsettings: GlobalSettings,
    opts: WorkerInfoOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    let response = get_worker_info(&mut connection, opts.worker_id).await?;

    if let Some(worker) = response {
        print_worker_configuration(&gsettings, opts.worker_id, worker.configuration);
    } else {
        log::error!("Worker {} not found", opts.worker_id);
    }
    Ok(())
}

async fn command_resubmit(gsettings: GlobalSettings, opts: ResubmitOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    resubmit_computation(&gsettings, &mut connection, opts).await
}

fn command_worker_hwdetect() -> anyhow::Result<()> {
    let descriptor = detect_resource()?;
    print_resource_descriptor(&descriptor);
    Ok(())
}

async fn command_worker_address(
    gsettings: GlobalSettings,
    opts: WorkerAddressOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;
    let response = get_worker_info(&mut connection, opts.worker_id).await?;

    match response {
        Some(info) => println!("{}", info.configuration.hostname),
        None => anyhow::bail!("Worker {} not found", opts.worker_id),
    }

    Ok(())
}

async fn command_wait(gsettings: GlobalSettings, opts: WaitOpts) -> anyhow::Result<()> {
    let mut connection = get_client_connection(gsettings.server_directory()).await?;

    wait_for_job_with_selector(&mut connection, opts.selector_arg.into()).await
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
        SubCommand::Server(ServerOpts {
            subcmd: ServerCommand::Info(opts),
        }) => command_server_info(gsettings, opts).await,

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
            subcmd: WorkerCommand::Info(opts),
        }) => command_worker_info(gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Hwdetect,
        }) => command_worker_hwdetect(),
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Address(opts),
        }) => command_worker_address(gsettings, opts).await,
        SubCommand::Jobs(opts) => command_job_list(gsettings, opts).await,
        SubCommand::Job(opts) => command_job_detail(gsettings, opts).await,
        SubCommand::Submit(opts) => command_submit(gsettings, opts).await,
        SubCommand::Cancel(opts) => command_cancel(gsettings, opts).await,
        SubCommand::Resubmit(opts) => command_resubmit(gsettings, opts).await,
        SubCommand::Wait(opts) => command_wait(gsettings, opts).await,
        SubCommand::Log(opts) => command_log(gsettings, opts),
        SubCommand::AutoAlloc(opts) => command_autoalloc(gsettings, opts).await,
    };
    if let Err(e) = result {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }

    Ok(())
}
