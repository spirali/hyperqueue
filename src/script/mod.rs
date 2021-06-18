use std::path::PathBuf;

use clap::Clap;

use crate::client::globalsettings::GlobalSettings;

mod backup;

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
pub struct ScriptOpts {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Clap)]
enum SubCommand {
    /// Wrap the execution of a program.
    /// If the executed program succeeds (has zero exit code), delete its stdout/stderr files.
    /// If the executed program fails (has non-zero exit code), copy its stdout/stderr files to the
    /// specified directory.
    BackupOutput(BackupOutputOpts),
}

#[derive(Clap)]
#[clap(setting = clap::AppSettings::ColoredHelp)]
struct BackupOutputOpts {
    /// Path to stdout of the executed program
    #[clap(long, env("HQ_STDOUT"))]
    stdout: PathBuf,

    /// Path to stderr of the executed program
    #[clap(long, env("HQ_STDERR"))]
    stderr: PathBuf,

    /// Where to store output files if the program fails
    #[clap()]
    dir: PathBuf,

    /// Commands that should be executed
    #[clap(last(true))]
    args: Vec<String>,
}

pub fn command_script(_gsettings: GlobalSettings, opts: ScriptOpts) -> anyhow::Result<()> {
    match opts.subcmd {
        SubCommand::BackupOutput(opts) => backup::backup_output(opts),
    }
}
