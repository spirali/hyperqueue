use std::time::Duration;

use crate::common::manager::pbs::{format_pbs_duration, parse_pbs_datetime};
use crate::common::timeutils::local_to_system_time;
use crate::server::autoalloc::descriptor::{CreatedAllocation, QueueHandler};
use crate::server::autoalloc::state::{AllocationId, AllocationStatus, AllocationTimeInfo};
use crate::server::autoalloc::AutoAllocResult;
use anyhow::Context;
use bstr::ByteSlice;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Output;
use tokio::process::Command;

pub struct PbsHandler {
    // TODO: pass as queue info in trait
    queue: String,
    timelimit: Option<Duration>,
    server_directory: PathBuf,
    name: String,
    hq_path: PathBuf,
}

impl PbsHandler {
    pub fn new(
        queue: String,
        timelimit: Option<Duration>,
        name: String,
        server_directory: PathBuf,
    ) -> Self {
        let hq_path = std::env::current_exe().expect("Cannot get HyperQueue path");
        Self {
            queue,
            timelimit,
            server_directory,
            name,
            hq_path,
        }
    }
}

fn create_allocation_dir(server_directory: PathBuf, name: &str) -> Result<PathBuf, std::io::Error> {
    let mut dir = server_directory;
    dir.push("autoalloc");
    dir.push(name);

    std::fs::create_dir_all(&dir)?;

    Ok(tempdir::TempDir::new_in(dir, "allocation")?.into_path())
}

impl QueueHandler for PbsHandler {
    fn schedule_allocation(
        &self,
        worker_count: u64,
    ) -> Pin<Box<dyn Future<Output = AutoAllocResult<CreatedAllocation>>>> {
        let queue = self.queue.clone();
        let timelimit = self.timelimit;
        let hq_path = self.hq_path.display().to_string();
        let server_directory = self.server_directory.clone();
        let name = self.name.clone();

        Box::pin(async move {
            let directory = create_allocation_dir(server_directory.clone(), &name)?;

            let mut arguments = vec![
                "qsub".to_string(),
                "-q".to_string(),
                queue,
                "-wd".to_string(),
                directory.display().to_string(),
                "-o".to_string(),
                directory.join("stdout").display().to_string(),
                "-e".to_string(),
                directory.join("stderr").display().to_string(),
                format!("-lselect={}", worker_count),
            ];

            if let Some(ref timelimit) = timelimit {
                arguments.push(format!("-lwalltime={}", format_pbs_duration(timelimit)));
            }

            // `hq worker` arguments
            arguments.extend([
                "--".to_string(),
                hq_path,
                "worker".to_string(),
                "start".to_string(),
                "--idle-timeout".to_string(),
                "10m".to_string(),
                "--manager".to_string(),
                "pbs".to_string(),
                "--server-dir".to_string(),
                server_directory.display().to_string(),
            ]);

            log::debug!("Running PBS command `{}`", arguments.join(" "));
            let mut command = Command::new(arguments[0].clone());
            command.args(&arguments[1..]);

            let output = command.output().await.context("qsub start failed")?;
            let output = check_command_output(output).context("qsub execution failed")?;

            let job_id = output
                .stdout
                .to_str()
                .map_err(|e| anyhow::anyhow!("Invalid UTF-8 qsub output: {:?}", e))?
                .trim();
            let job_id = job_id.to_string();

            // Write the PBS job id to the folder as a debug information
            std::fs::write(directory.join("jobid"), &job_id)?;

            Ok(CreatedAllocation {
                id: job_id,
                working_dir: directory,
            })
        })
    }

    fn get_allocation_status(
        &self,
        allocation_id: AllocationId,
    ) -> Pin<Box<dyn Future<Output = AutoAllocResult<Option<AllocationStatus>>>>> {
        Box::pin(async move {
            // -x will also display finished jobs
            let arguments = vec!["qstat", "-f", &allocation_id, "-F", "json", "-x"];
            log::debug!("Running PBS command `{}`", arguments.join(" "));

            let mut command = Command::new(arguments[0]);
            command.args(&arguments[1..]);

            let output = command.output().await.context("qstat start failed")?;
            let output = check_command_output(output).context("qstat execution failed")?;
            let data: serde_json::Value =
                serde_json::from_slice(&output.stdout).context("Cannot parse qstat JSON output")?;
            let job = &data["Jobs"][&allocation_id];

            // Job was not found
            if job.is_null() {
                return Ok(None);
            }

            let state = get_json_str(&job["job_state"], "Job state")?;
            let queue_time = get_json_str(&job["qtime"], "Queue time")?;
            let start_time = &job["stime"];
            let finish_time = &job["mtime"];
            let times = AllocationTimeInfo {
                queued_at: local_to_system_time(parse_pbs_datetime(queue_time)?),
                started_at: start_time
                    .as_str()
                    .map(|v| AutoAllocResult::Ok(local_to_system_time(parse_pbs_datetime(v)?)))
                    .transpose()?,
                finished_at: finish_time
                    .as_str()
                    .map(|v| AutoAllocResult::Ok(local_to_system_time(parse_pbs_datetime(v)?)))
                    .transpose()?,
            };

            let status = match state {
                "Q" => AllocationStatus::Queued(times),
                "R" => AllocationStatus::Running(times),
                "F" => {
                    let exit_status = get_json_number(&job["Exit_status"], "Exit status")?;
                    if exit_status == 0 {
                        AllocationStatus::Finished(times)
                    } else {
                        AllocationStatus::Failed(times)
                    }
                }
                status => anyhow::bail!("Unknown PBS job status {}", status),
            };

            Ok(Some(status))
        })
    }
}

fn get_json_str<'a>(value: &'a serde_json::Value, context: &str) -> AutoAllocResult<&'a str> {
    value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("JSON key {} not found", context))
}
fn get_json_number(value: &serde_json::Value, context: &str) -> AutoAllocResult<u64> {
    value
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("JSON key {} not found", context))
}

fn check_command_output(output: Output) -> AutoAllocResult<Output> {
    let status = output.status;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "Exit code {}, stderr: {}, stdout: {}",
            status.code().unwrap(),
            output.stderr.to_str().unwrap(),
            output.stdout.to_str().unwrap()
        ));
    }
    Ok(output)
}
