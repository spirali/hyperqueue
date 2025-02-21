use crate::client::commands::submit::command::{
    send_submit_request, DEFAULT_STDERR_PATH, DEFAULT_STDOUT_PATH,
};
use crate::client::commands::submit::defs::{
    ArrayDef, JobDef, StdioDefFull, StdioDefInput, TaskDef,
};
use crate::client::commands::submit::defs::{PinMode as PinModeDef, TaskConfigDef};
use crate::client::globalsettings::GlobalSettings;
use crate::common::arraydef::IntArray;
use crate::common::utils::fs::get_current_dir;
use crate::transfer::connection::ClientSession;
use crate::transfer::messages::{
    JobDescription, JobSubmitDescription, JobTaskDescription, PinMode, SubmitRequest,
    TaskDescription, TaskKind, TaskKindProgram, TaskWithDependencies,
};
use crate::{JobId, JobTaskCount, JobTaskId};
use clap::Parser;
use smallvec::smallvec;
use std::path::PathBuf;
use tako::gateway::{ResourceRequest, ResourceRequestVariants, TaskDataFlags};
use tako::program::{FileOnCloseBehavior, ProgramDefinition, StdioDef};

#[derive(Parser)]
pub struct JobSubmitFileOpts {
    /// Path to file with job definition
    path: PathBuf,

    /// Attach a submission to an open job
    #[clap(long)]
    job: Option<JobId>,
}

fn create_stdio(def: Option<StdioDefInput>, default: &str, is_log: bool) -> StdioDef {
    match def {
        None => {
            if is_log {
                StdioDef::Pipe
            } else {
                StdioDef::File {
                    path: PathBuf::from(default),
                    on_close: FileOnCloseBehavior::default(),
                }
            }
        }
        Some(StdioDefInput::None) => StdioDef::Null,
        Some(StdioDefInput::Path(path)) => StdioDef::File {
            path,
            on_close: FileOnCloseBehavior::default(),
        },
        Some(StdioDefInput::Full(StdioDefFull { path, on_close })) => {
            StdioDef::File { path, on_close }
        }
    }
}

fn build_task_description(cfg: TaskConfigDef) -> TaskDescription {
    TaskDescription {
        kind: TaskKind::ExternalProgram(TaskKindProgram {
            program: ProgramDefinition {
                args: cfg.command.into_iter().map(|x| x.into()).collect(),
                env: cfg.env,
                stdout: create_stdio(cfg.stdout, DEFAULT_STDOUT_PATH, false),
                stderr: create_stdio(cfg.stderr, DEFAULT_STDERR_PATH, false),
                stdin: cfg.stdin.map(|s| s.as_bytes().into()).unwrap_or_default(),
                cwd: cfg.cwd.map(|x| x.into()).unwrap_or_else(get_current_dir),
            },
            pin_mode: match cfg.pin {
                PinModeDef::None => PinMode::None,
                PinModeDef::TaskSet => PinMode::TaskSet,
                PinModeDef::OpenMP => PinMode::OpenMP,
            },
            task_dir: cfg.task_dir,
        }),
        resources: ResourceRequestVariants {
            variants: if cfg.request.is_empty() {
                smallvec![ResourceRequest::default()]
            } else {
                cfg.request.into_iter().map(|r| r.into_request()).collect()
            },
        },
        time_limit: cfg.time_limit,
        priority: cfg.priority,
        crash_limit: cfg.crash_limit,
    }
}

fn build_task(
    tdef: TaskDef,
    max_id: &mut JobTaskId,
    data_flags: TaskDataFlags,
) -> TaskWithDependencies {
    let id = tdef.id.unwrap_or_else(|| {
        *max_id = JobTaskId::new(max_id.as_num() + 1);
        *max_id
    });
    TaskWithDependencies {
        id,
        data_flags,
        task_desc: build_task_description(tdef.config),
        task_deps: tdef.deps,
        data_deps: tdef.data_deps,
    }
}

fn build_job_desc_array(array: ArrayDef) -> JobTaskDescription {
    let ids = array
        .ids
        .unwrap_or_else(|| IntArray::from_range(0, array.entries.len() as JobTaskCount));
    let entries = if array.entries.is_empty() {
        None
    } else {
        Some(array.entries.into_iter().map(|s| s.into()).collect())
    };
    JobTaskDescription::Array {
        ids,
        entries,
        task_desc: build_task_description(array.config),
    }
}

fn build_job_desc_individual_tasks(
    tasks: Vec<TaskDef>,
    data_flags: TaskDataFlags,
) -> JobTaskDescription {
    let mut max_id: JobTaskId = tasks
        .iter()
        .map(|t| t.id)
        .max()
        .flatten()
        .unwrap_or(JobTaskId(0));

    JobTaskDescription::Graph {
        tasks: tasks
            .into_iter()
            .map(|t| build_task(t, &mut max_id, data_flags))
            .collect(),
    }
}

fn build_job_submit(jdef: JobDef, job_id: Option<JobId>) -> SubmitRequest {
    let task_desc = if let Some(array) = jdef.array {
        build_job_desc_array(array)
    } else {
        let mut data_flags = TaskDataFlags::empty();
        if jdef.data_layer {
            data_flags.insert(TaskDataFlags::ENABLE_DATA_LAYER);
        }
        build_job_desc_individual_tasks(jdef.tasks, data_flags)
    };
    SubmitRequest {
        job_desc: JobDescription {
            name: jdef.name,
            max_fails: jdef.max_fails,
        },
        submit_desc: JobSubmitDescription {
            task_desc,
            submit_dir: get_current_dir(),
            stream_path: jdef.stream,
        },
        job_id,
    }
}

pub async fn submit_computation_from_job_file(
    gsettings: &GlobalSettings,
    session: &mut ClientSession,
    opts: JobSubmitFileOpts,
) -> anyhow::Result<()> {
    let jdef =
        {
            JobDef::parse(&std::fs::read_to_string(&opts.path).map_err(|e| {
                anyhow::anyhow!(format!("Cannot read {}: {}", opts.path.display(), e))
            })?)?
        };
    let request = build_job_submit(jdef, opts.job);
    send_submit_request(gsettings, session, request, false, false).await
}
