use std::process::{Command, Stdio};

use anyhow::Context;

use crate::script::BackupOutputOpts;

pub(super) fn backup_output(opts: BackupOutputOpts) -> anyhow::Result<()> {
    if opts.args.is_empty() {
        anyhow::bail!("You have to provide at least one command");
    }
    let command = &opts.args[0];
    let arguments = &opts.args[1..];
    let status = Command::new(command)
        .args(arguments)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .status()
        .with_context(|| format!("Cannot run program {:?}", opts.args.join(" ")))?;

    let files = [opts.stdout, opts.stderr];
    if status.success() {
        // The program was successful, delete stdout/stderr files
        for file in files {
            std::fs::remove_file(file).ok();
        }
    } else {
        // Some error has happened, copy output to specified directory
        std::fs::create_dir_all(&opts.dir)?;
        for file in files {
            let basename = file
                .file_name()
                .with_context(|| format!("Cannot get file name of {:?}", file))
                .unwrap();
            let target = opts.dir.join(basename);
            std::fs::copy(file, target)?;
        }
        // Propagate the error
        std::process::exit(status.code().unwrap());
    }
    Ok(())
}
