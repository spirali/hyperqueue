use clap::{CommandFactory, FromArgMatches};
use clap_complete::generate;
use cli_table::ColorChoice;
use colored::Colorize;
use std::io;
use std::io::IsTerminal;
use std::panic::PanicHookInfo;

use hyperqueue::client::commands::autoalloc::command_autoalloc;
use hyperqueue::client::commands::job::{
    cancel_job, close_job, forget_job, output_job_cat, output_job_detail, output_job_list,
    output_job_summary, JobCancelOpts, JobCatOpts, JobCloseOpts, JobForgetOpts, JobInfoOpts,
    JobListOpts, JobTaskIdsOpts,
};
use hyperqueue::client::commands::journal::command_journal;
use hyperqueue::client::commands::outputlog::command_reader;
use hyperqueue::client::commands::server::command_server;
use hyperqueue::client::commands::submit::command::{open_job, SubmitJobConfOpts};
use hyperqueue::client::commands::submit::{
    submit_computation, submit_computation_from_job_file, JobSubmitFileOpts, JobSubmitOpts,
};
use hyperqueue::client::commands::wait::{wait_for_jobs, wait_for_jobs_with_progress};
use hyperqueue::client::commands::worker::{
    get_worker_info, get_worker_list, start_hq_worker, stop_worker, wait_for_workers, WorkerFilter,
    WorkerStartOpts,
};
use hyperqueue::client::default_server_directory_path;
use hyperqueue::client::globalsettings::GlobalSettings;
use hyperqueue::client::output::cli::CliOutput;
use hyperqueue::client::output::json::JsonOutput;
use hyperqueue::client::output::outputs::{Output, Outputs};
use hyperqueue::client::output::quiet::Quiet;
use hyperqueue::client::status::Status;
use hyperqueue::client::task::{
    output_job_task_ids, output_job_task_info, output_job_task_list, TaskCommand, TaskInfoOpts,
    TaskListOpts, TaskOpts,
};
use hyperqueue::common::cli::{
    get_task_id_selector, get_task_selector, ColorPolicy, CommonOpts, GenerateCompletionOpts,
    HwDetectOpts, JobCommand, JobOpts, JobProgressOpts, JobWaitOpts, OptsWithMatches, RootOptions,
    SubCommand, WorkerAddressOpts, WorkerCommand, WorkerInfoOpts, WorkerListOpts, WorkerOpts,
    WorkerStopOpts, WorkerWaitOpts,
};
use hyperqueue::common::setup::setup_logging;
use hyperqueue::common::utils::fs::absolute_path;
use hyperqueue::server::bootstrap::get_client_session;
use hyperqueue::transfer::messages::{
    FromClientMessage, IdSelector, JobInfoRequest, ToClientMessage,
};
use hyperqueue::worker::hwdetect::{
    detect_additional_resources, detect_cpus, prune_hyper_threading,
};
use hyperqueue::HQ_VERSION;
use tako::resources::{ResourceDescriptor, ResourceDescriptorItem, CPU_RESOURCE_NAME};

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

async fn command_job_submit(
    gsettings: &GlobalSettings,
    opts: OptsWithMatches<JobSubmitOpts>,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    submit_computation(gsettings, &mut session, opts).await
}

async fn command_job_open(
    gsettings: &GlobalSettings,
    opts: SubmitJobConfOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    open_job(gsettings, &mut session, opts).await
}

async fn command_submit_job_file(
    gsettings: &GlobalSettings,
    opts: JobSubmitFileOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    submit_computation_from_job_file(gsettings, &mut session, opts).await
}

async fn command_job_list(gsettings: &GlobalSettings, opts: JobListOpts) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;

    let (filter, show_open) = if opts.filter.is_empty() {
        if opts.all {
            (vec![], true)
        } else {
            (vec![Status::Waiting, Status::Running, Status::Opened], true)
        }
    } else {
        (opts.filter, false)
    };

    output_job_list(gsettings, &mut connection, filter, show_open).await
}

async fn command_job_summary(gsettings: &GlobalSettings) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;

    output_job_summary(gsettings, &mut connection).await
}

async fn command_job_detail(gsettings: &GlobalSettings, opts: JobInfoOpts) -> anyhow::Result<()> {
    if matches!(opts.selector, IdSelector::All) {
        log::warn!(
            "Job detail doesn't support the `all` selector, did you mean to use `hq job list --all`?"
        );
        return Ok(());
    }

    let mut session = get_client_session(gsettings.server_directory()).await?;
    output_job_detail(gsettings, &mut session, opts.selector).await
}

async fn command_job_cat(gsettings: &GlobalSettings, opts: JobCatOpts) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;
    output_job_cat(
        gsettings,
        &mut connection,
        opts.job_selector,
        get_task_selector(Some(opts.task_selector)),
        opts.stream,
        opts.print_task_header,
    )
    .await
}

async fn command_job_cancel(gsettings: &GlobalSettings, opts: JobCancelOpts) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;
    cancel_job(gsettings, &mut connection, opts.selector).await
}

async fn command_job_close(gsettings: &GlobalSettings, opts: JobCloseOpts) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;
    close_job(gsettings, &mut connection, opts.selector).await
}

async fn command_job_delete(gsettings: &GlobalSettings, opts: JobForgetOpts) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;
    forget_job(gsettings, &mut connection, opts).await
}

async fn command_job_task_ids(
    gsettings: &GlobalSettings,
    opts: JobTaskIdsOpts,
) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;
    output_job_task_ids(gsettings, &mut connection, opts).await
}

async fn command_job_wait(gsettings: &GlobalSettings, opts: JobWaitOpts) -> anyhow::Result<()> {
    let mut connection = get_client_session(gsettings.server_directory()).await?;
    wait_for_jobs(
        gsettings,
        &mut connection,
        opts.selector,
        !opts.without_close,
    )
    .await
}

async fn command_job_progress(
    gsettings: &GlobalSettings,
    opts: JobProgressOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    let response = hyperqueue::rpc_call!(
        session.connection(),
        FromClientMessage::JobInfo(JobInfoRequest {
            selector: opts.selector,
        }),
        ToClientMessage::JobInfoResponse(r) => r
    )
    .await?;
    wait_for_jobs_with_progress(&mut session, &response.jobs).await
}

async fn command_task_list(gsettings: &GlobalSettings, opts: TaskListOpts) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    output_job_task_list(
        gsettings,
        &mut session,
        opts.job_selector,
        get_task_selector(Some(opts.task_selector)),
        opts.verbosity.into(),
    )
    .await
}

async fn command_task_info(gsettings: &GlobalSettings, opts: TaskInfoOpts) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    output_job_task_info(
        gsettings,
        &mut session,
        opts.job_selector,
        get_task_id_selector(Some(opts.task_selector)),
        opts.verbosity.into(),
    )
    .await
}

async fn command_worker_start(
    gsettings: &GlobalSettings,
    opts: WorkerStartOpts,
) -> anyhow::Result<()> {
    start_hq_worker(gsettings, opts).await
}

async fn command_worker_stop(
    gsettings: &GlobalSettings,
    opts: WorkerStopOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    stop_worker(&mut session, opts.selector_arg).await?;
    Ok(())
}

async fn command_worker_list(
    gsettings: &GlobalSettings,
    opts: WorkerListOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;

    let filter = opts.filter.or({
        if opts.all {
            None
        } else {
            Some(WorkerFilter::Running)
        }
    });

    let workers = get_worker_list(&mut session, filter).await?;
    gsettings.printer().print_worker_list(workers);
    Ok(())
}

async fn command_worker_info(
    gsettings: &GlobalSettings,
    opts: WorkerInfoOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    let response = get_worker_info(&mut session, opts.worker_id).await?;

    if let Some(worker) = response {
        gsettings.printer().print_worker_info(worker);
    } else {
        log::error!("Worker {} not found", opts.worker_id);
    }
    Ok(())
}

async fn command_worker_wait(
    gsettings: &GlobalSettings,
    opts: WorkerWaitOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    wait_for_workers(&mut session, opts.worker_count).await
}

fn command_worker_hwdetect(gsettings: &GlobalSettings, opts: HwDetectOpts) -> anyhow::Result<()> {
    let mut cpus = detect_cpus()?;
    if opts.no_hyper_threading {
        cpus = prune_hyper_threading(&cpus)?;
    }
    let mut resources = vec![ResourceDescriptorItem {
        name: CPU_RESOURCE_NAME.to_string(),
        kind: cpus,
    }];
    detect_additional_resources(&mut resources)?;
    gsettings
        .printer()
        .print_hw(&ResourceDescriptor::new(resources));
    Ok(())
}

async fn command_worker_address(
    gsettings: &GlobalSettings,
    opts: WorkerAddressOpts,
) -> anyhow::Result<()> {
    let mut session = get_client_session(gsettings.server_directory()).await?;
    let response = get_worker_info(&mut session, opts.worker_id).await?;

    match response {
        Some(info) => println!("{}", info.configuration.hostname),
        None => anyhow::bail!("Worker {} not found", opts.worker_id),
    }

    Ok(())
}

#[cfg(feature = "dashboard")]
/// Starts the hq dashboard
async fn command_dashboard_start(
    gsettings: &GlobalSettings,
    opts: hyperqueue::common::cli::DashboardOpts,
) -> anyhow::Result<()> {
    use hyperqueue::common::cli::DashboardCommand;
    use hyperqueue::dashboard::start_ui_loop;
    use hyperqueue::server::event::journal::JournalReader;
    use hyperqueue::server::event::Event;

    match opts.subcmd.unwrap_or_default() {
        DashboardCommand::Replay { journal } => {
            println!("Loading journal {}", journal.display());
            let mut journal = JournalReader::open(&journal)?;
            let events: Vec<Event> = journal.collect::<Result<_, _>>()?;
            println!("Loaded {} events", events.len());

            start_ui_loop(gsettings, Some(events)).await?;
        }
        DashboardCommand::Stream => {
            start_ui_loop(gsettings, None).await?;
        }
    }

    Ok(())
}

fn make_global_settings(opts: CommonOpts) -> GlobalSettings {
    let server_dir = absolute_path(
        opts.server_dir
            .unwrap_or_else(default_server_directory_path),
    );

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

    // Create Printer
    let printer: Box<dyn Output> = match opts.output_mode {
        Outputs::CLI => {
            // Set colored public for CLI
            match color_policy {
                ColorChoice::Always | ColorChoice::AlwaysAnsi => {
                    colored::control::set_override(true)
                }
                ColorChoice::Never => colored::control::set_override(false),
                _ => {}
            }

            Box::new(CliOutput::new(color_policy))
        }
        Outputs::JSON => Box::<JsonOutput>::default(),
        Outputs::Quiet => Box::<Quiet>::default(),
    };

    GlobalSettings::new(server_dir, printer)
}

fn generate_completion(opts: GenerateCompletionOpts) -> anyhow::Result<()> {
    let generator = opts.shell;

    let mut app = RootOptions::command();
    eprintln!("Generating completion file for {generator}...");
    generate(generator, &mut app, "hq".to_string(), &mut io::stdout());
    Ok(())
}

fn hq_panic_hook(_info: &PanicHookInfo) {
    let message = format!(
        r#"Oops, HyperQueue has crashed. This is a bug, sorry for that.
If you would be so kind, please report this issue at the HQ issue tracker: https://github.com/It4innovations/hyperqueue/issues/new?title=HQ%20crashes
Please include the above error (starting from "thread ... panicked ...") and the stack backtrace in the issue contents, along with the following information:

HyperQueue version: {version}

You can also re-run HyperQueue server (and its workers) with the `RUST_LOG=hq=debug,tako=debug`
environment variable, and attach the logs to the issue, to provide us more information.
"#,
        version = HQ_VERSION
    );

    if io::stdout().is_terminal() {
        eprintln!("{}", message.red());
    } else {
        eprintln!("{message}");
    };
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> hyperqueue::Result<()> {
    // Augment panics - first print the error and backtrace like normally,
    // and then print our own custom error message.
    let std_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo| {
        std_panic(info);
        hq_panic_hook(info);
    }));

    // Also enable backtraces by default.
    // This enables backtraces when panicking, but also for normal anyhow errors.
    // SAFETY: we are at the beginning of the program, no other threads that could set
    // environment variables should be executing.
    unsafe {
        std::env::set_var("RUST_BACKTRACE", "full");
    }

    // This further disables backtraces for normal anyhow errors.
    // They should not be printed to users in release mode.
    #[cfg(not(debug_assertions))]
    {
        std::env::set_var("RUST_LIB_BACKTRACE", "0");
    }

    let matches = RootOptions::command().get_matches();
    let top_opts = match RootOptions::from_arg_matches(&matches) {
        Ok(opts) => opts,
        Err(error) => error.exit(),
    };

    setup_logging(top_opts.common.debug);

    let gsettings = make_global_settings(top_opts.common);

    let result = match top_opts.subcmd {
        SubCommand::Server(opts) => command_server(&gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Start(opts),
        }) => command_worker_start(&gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Stop(opts),
        }) => command_worker_stop(&gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::List(opts),
        }) => command_worker_list(&gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Info(opts),
        }) => command_worker_info(&gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::HwDetect(opts),
        }) => command_worker_hwdetect(&gsettings, opts),
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Address(opts),
        }) => command_worker_address(&gsettings, opts).await,
        SubCommand::Worker(WorkerOpts {
            subcmd: WorkerCommand::Wait(opts),
        }) => command_worker_wait(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::List(opts),
        }) => command_job_list(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Summary,
        }) => command_job_summary(&gsettings).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Info(opts),
        }) => command_job_detail(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Cat(opts),
        }) => command_job_cat(&gsettings, opts).await,
        SubCommand::Submit(opts)
        | SubCommand::Job(JobOpts {
            subcmd: JobCommand::Submit(opts),
        }) => command_job_submit(&gsettings, OptsWithMatches::new(opts, matches)).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::SubmitFile(opts),
        }) => command_submit_job_file(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Cancel(opts),
        }) => command_job_cancel(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Forget(opts),
        }) => command_job_delete(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Wait(opts),
        }) => command_job_wait(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Progress(opts),
        }) => command_job_progress(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::TaskIds(opts),
        }) => command_job_task_ids(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Open(opts),
        }) => command_job_open(&gsettings, opts).await,
        SubCommand::Job(JobOpts {
            subcmd: JobCommand::Close(opts),
        }) => command_job_close(&gsettings, opts).await,
        SubCommand::Task(TaskOpts {
            subcmd: TaskCommand::List(opts),
        }) => command_task_list(&gsettings, opts).await,
        SubCommand::Task(TaskOpts {
            subcmd: TaskCommand::Info(opts),
        }) => command_task_info(&gsettings, opts).await,
        #[cfg(feature = "dashboard")]
        SubCommand::Dashboard(opts) => command_dashboard_start(&gsettings, opts).await,
        SubCommand::OutputLog(opts) => command_reader(&gsettings, opts),
        SubCommand::AutoAlloc(opts) => command_autoalloc(&gsettings, opts).await,
        SubCommand::Journal(opts) => command_journal(&gsettings, opts).await,
        SubCommand::GenerateCompletion(opts) => generate_completion(opts),
    };

    if let Err(e) = result {
        gsettings.printer().print_error(e);
        std::process::exit(1);
    }

    Ok(())
}
