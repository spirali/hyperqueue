use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgMatches, Parser};
use clap_complete::Shell;

use tako::WorkerId;

use crate::client::commands::autoalloc::AutoAllocOpts;
use crate::client::commands::data::DataOpts;
use crate::client::commands::job::{
    JobCancelOpts, JobCatOpts, JobCloseOpts, JobForgetOpts, JobInfoOpts, JobListOpts,
    JobTaskIdsOpts,
};
use crate::client::commands::journal::JournalOpts;
use crate::client::commands::outputlog::OutputLogOpts;
use crate::client::commands::server::ServerOpts;
use crate::client::commands::submit::command::SubmitJobConfOpts;
use crate::client::commands::submit::{JobSubmitFileOpts, JobSubmitOpts};
use crate::client::commands::worker::{WorkerFilter, WorkerStartOpts};
use crate::client::output::outputs::Outputs;
use crate::client::status::Status;
use crate::client::task::TaskOpts;
use crate::common::arraydef::IntArray;
use crate::transfer::messages::{
    IdSelector, SingleIdSelector, TaskIdSelector, TaskSelector, TaskStatusSelector,
};

#[derive(Parser)]
pub struct TaskSelectorArg {
    /// Filter task(s) by ID.
    #[arg(long)]
    pub tasks: Option<IntArray>,

    /// Filter task(s) by status.
    /// You can use multiple states separated by a comma.
    #[arg(long, value_delimiter(','), value_enum)]
    pub task_status: Vec<Status>,
}

pub fn get_task_selector(opt_task_selector_arg: Option<TaskSelectorArg>) -> Option<TaskSelector> {
    opt_task_selector_arg.map(|arg| TaskSelector {
        id_selector: get_task_id_selector(arg.tasks),
        status_selector: get_task_status_selector(arg.task_status),
    })
}

pub fn get_task_id_selector(id_selector_arg: Option<IntArray>) -> TaskIdSelector {
    id_selector_arg
        .map(TaskIdSelector::Specific)
        .unwrap_or(TaskIdSelector::All)
}

fn get_task_status_selector(status_selector_arg: Vec<Status>) -> TaskStatusSelector {
    if status_selector_arg.is_empty() {
        TaskStatusSelector::All
    } else {
        TaskStatusSelector::Specific(status_selector_arg)
    }
}

/// Selects items with the following options:
/// - "last" => only the last item
/// - "all" => all items
/// - "<integer-range>" => see [`crate::common::arraydef::parse_array`]
pub fn parse_last_all_range(value: &str) -> anyhow::Result<IdSelector> {
    match value {
        "last" => Ok(IdSelector::LastN(1)),
        "all" => Ok(IdSelector::All),
        _ => Ok(IdSelector::Specific(IntArray::from_str(value)?)),
    }
}

/// Selects items with the following options:
/// - "last" => only the last item
/// - "<integer-range>" => see [`crate::common::arraydef::parse_array`]
pub fn parse_last_range(value: &str) -> anyhow::Result<IdSelector> {
    match value {
        "last" => Ok(IdSelector::LastN(1)),
        _ => Ok(IdSelector::Specific(IntArray::from_str(value)?)),
    }
}

/// Selects an item with the following options:
/// - "last" => only the last item
/// - "<id>" => item with the specified ID
pub fn parse_last_single_id(value: &str) -> anyhow::Result<SingleIdSelector> {
    match value {
        "last" => Ok(SingleIdSelector::Last),
        _ => Ok(SingleIdSelector::Specific(u32::from_str(value)?)),
    }
}

pub struct OptsWithMatches<Opts> {
    pub opts: Opts,
    pub matches: ArgMatches,
}

impl<Opts> OptsWithMatches<Opts> {
    pub fn new(opts: Opts, matches: ArgMatches) -> Self {
        Self { opts, matches }
    }

    pub fn into_inner(self) -> (Opts, ArgMatches) {
        (self.opts, self.matches)
    }
}

#[derive(clap::ValueEnum, Clone)]
pub enum ColorPolicy {
    /// Use colors if the stdout is detected to be a terminal.
    Auto,
    /// Always use colors.
    Always,
    /// Never use colors.
    Never,
}

// Common CLI options
#[derive(Parser)]
pub struct CommonOpts {
    /// Path to a directory that stores HyperQueue access files
    #[arg(
    long,
    value_hint = clap::ValueHint::DirPath,
    global = true,
    env = "HQ_SERVER_DIR",
    help_heading("GLOBAL OPTIONS"),
    hide_short_help(true)
    )]
    pub server_dir: Option<PathBuf>,

    /// Console color policy.
    #[arg(
        long,
        default_value_t = ColorPolicy::Auto,
        value_enum,
        global = true,
        help_heading("GLOBAL OPTIONS"),
        hide_short_help(true)
    )]
    pub colors: ColorPolicy,

    /// How should the output of the command be formatted.
    #[arg(
        long,
        env = "HQ_OUTPUT_MODE",
        default_value_t = Outputs::CLI,
        value_enum,
        global = true,
        help_heading("GLOBAL OPTIONS"),
        hide_short_help(true)
    )]
    pub output_mode: Outputs,

    /// Turn on a more detailed log output
    #[arg(
        long,
        env = "HQ_DEBUG",
        global = true,
        help_heading("GLOBAL OPTIONS"),
        hide_short_help(true)
    )]
    pub debug: bool,
}

// Root CLI options
#[derive(Parser)]
#[command(
    author,
    about,
    version(crate::HQ_VERSION),
    disable_help_subcommand(true),
    help_expected(true)
)]
pub struct RootOptions {
    #[clap(flatten)]
    pub common: CommonOpts,

    #[clap(subcommand)]
    pub subcmd: SubCommand,
}

#[allow(clippy::large_enum_variant)]
#[derive(Parser)]
pub enum SubCommand {
    /// Commands for controlling the HyperQueue server
    Server(ServerOpts),
    /// Commands for controlling HyperQueue jobs
    Job(JobOpts),
    /// Commands for displaying task(s)
    Task(TaskOpts),
    /// Submit a job to HyperQueue
    Submit(JobSubmitOpts),
    /// Commands for controlling HyperQueue workers
    Worker(WorkerOpts),
    /// Operations with log
    OutputLog(OutputLogOpts),
    /// Automatic allocation management
    #[command(name = "alloc")]
    AutoAlloc(AutoAllocOpts),
    /// Event and journal management
    Journal(JournalOpts),
    /// Data object management inside a task
    Data(DataOpts),
    /// Start the HyperQueue CLI dashboard
    #[cfg(feature = "dashboard")]
    Dashboard(DashboardOpts),
    /// Generate shell completion script
    GenerateCompletion(GenerateCompletionOpts),
}

/// HyperQueue Dashboard
#[derive(Parser)]
pub struct DashboardOpts {
    #[clap(subcommand)]
    pub subcmd: Option<DashboardCommand>,
}

#[derive(Parser, Default)]
pub enum DashboardCommand {
    /// Stream events from a server.
    /// Note that this will replay all events from the currently active
    /// journal file before new events will be streamed.
    #[default]
    Stream,
    /// Replay events from a recorded journal file.
    Replay {
        /// Path to a journal file created via `hq server start --journal=<path>`.
        journal: PathBuf,
    },
}

// Worker CLI options

#[derive(Parser)]
pub struct WorkerOpts {
    #[clap(subcommand)]
    pub subcmd: WorkerCommand,
}

#[allow(clippy::large_enum_variant)]
#[derive(Parser)]
pub enum WorkerCommand {
    /// Start worker
    Start(WorkerStartOpts),
    /// Stop worker
    Stop(WorkerStopOpts),
    /// Display information about workers.
    /// By default, only running workers will be displayed.
    List(WorkerListOpts),
    /// Hwdetect
    #[command(name = "hwdetect")]
    HwDetect(HwDetectOpts),
    /// Display information about a specific worker
    Info(WorkerInfoOpts),
    /// Display worker's hostname
    Address(WorkerAddressOpts),
    /// Waits on the connection of worker(s)
    Wait(WorkerWaitOpts),
}

#[derive(Parser)]
pub struct WorkerStopOpts {
    /// Select worker(s) to stop
    #[arg(value_parser = parse_last_all_range)]
    pub selector_arg: IdSelector,
}

#[derive(Parser)]
pub struct WorkerListOpts {
    /// Display all workers.
    #[arg(long, conflicts_with("filter"))]
    pub all: bool,

    /// Select only workers with the given state.
    #[arg(long, value_enum)]
    pub filter: Option<WorkerFilter>,
}

#[derive(Parser)]
pub struct WorkerAddressOpts {
    /// Worker ID
    pub worker_id: WorkerId,
}

#[derive(Parser)]
pub struct WorkerInfoOpts {
    /// Worker ID
    pub worker_id: WorkerId,
}

#[derive(Parser)]
pub struct WorkerWaitOpts {
    /// Number of worker(s) to wait on
    pub worker_count: u32,
}

#[derive(Parser)]
pub struct HwDetectOpts {
    /// Detect only physical cores
    #[arg(long)]
    pub no_hyper_threading: bool,
}

// Job CLI options

#[derive(Parser)]
pub struct JobOpts {
    #[clap(subcommand)]
    pub subcmd: JobCommand,
}

#[allow(clippy::large_enum_variant)]
#[derive(Parser)]
pub enum JobCommand {
    /// Display information about jobs.
    /// By default, only queued or running jobs will be displayed.
    List(JobListOpts),
    /// Display a summary with the amount of jobs per each job state.
    Summary,
    /// Display detailed information of the selected job
    Info(JobInfoOpts),
    /// Cancel a specific job.
    /// This will cancel all tasks, stopping them from being computation.
    Cancel(JobCancelOpts),
    /// Forget a specific job.
    /// This will remove the job from the server's memory, forgetting it completely and reducing
    /// the server's memory usage.
    ///
    /// You can only forget jobs that have been completed, i.e. that are not running or waiting to
    /// be executed. You have to cancel these jobs first.
    Forget(JobForgetOpts),
    /// Shows task(s) streams(stdout, stderr) of a specific job
    Cat(JobCatOpts),
    /// Submit a job to HyperQueue
    Submit(JobSubmitOpts),
    /// Submit a job through a job definition file
    SubmitFile(JobSubmitFileOpts),
    /// Waits until a job is finished
    Wait(JobWaitOpts),
    /// Interactively observe the execution of a job
    Progress(JobProgressOpts),
    /// Print task Ids for given job
    TaskIds(JobTaskIdsOpts),
    /// Open new job (without attaching any tasks yet)
    Open(SubmitJobConfOpts),
    /// Close an open job
    Close(JobCloseOpts),
}

#[derive(Parser)]
pub struct JobWaitOpts {
    /// Select job(s) to wait for
    #[arg(value_parser = parse_last_all_range)]
    pub selector: IdSelector,

    /// Wait until all tasks are completed, even if the job is still open
    #[clap(long, action)]
    pub without_close: bool,
}

#[derive(Parser)]
pub struct JobProgressOpts {
    /// Select job(s) to observe
    #[arg(value_parser = parse_last_all_range)]
    pub selector: IdSelector,
}

#[derive(Parser)]
pub struct GenerateCompletionOpts {
    /// Shell flavour for which the completion script should be generated
    #[arg(value_enum)]
    pub shell: Shell,
}

#[cfg(test)]
mod tests {
    use crate::common::cli::RootOptions;

    #[test]
    fn verify_root_cli() {
        use clap::CommandFactory;
        RootOptions::command().debug_assert()
    }
}
