use std::fs::File;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;

use crate::internal::common::error::DsError::GenericError;
use crate::internal::common::resources::{Allocation, ResourceRequest};
use bstr::{BString, ByteSlice};
use nix::libc;
use tokio::process::Command;

use crate::gateway::TaskDataFlags;
use crate::internal::common::resources::map::ResourceMap;
use crate::internal::server::worker::Worker;
use crate::internal::worker::configuration::WorkerConfiguration;
use crate::internal::worker::localcomm::Token;
use crate::internal::worker::resources::map::ResourceLabelMap;
use crate::internal::worker::state::WorkerState;
use crate::internal::worker::task::Task;
use crate::program::{ProgramDefinition, StdioDef};
use crate::task::SerializedTaskContext;
use crate::{InstanceId, TaskId, WorkerId};

pub enum TaskResult {
    Finished,
    Canceled,
    Timeouted,
}

impl From<StopReason> for TaskResult {
    fn from(r: StopReason) -> Self {
        match r {
            StopReason::Cancel => TaskResult::Canceled,
            StopReason::Timeout => TaskResult::Timeouted,
        }
    }
}

pub enum StopReason {
    Cancel,
    Timeout,
}

pub type TaskFuture = Pin<Box<dyn Future<Output = crate::Result<TaskResult>>>>;

pub struct TaskLaunchData {
    /// Future that represents the execution of the task
    pub task_future: TaskFuture,

    /// Opaque data that will be sent back to the server
    pub task_context: SerializedTaskContext,
}

impl TaskLaunchData {
    #[inline]
    pub fn new(task_future: TaskFuture, task_context: SerializedTaskContext) -> Self {
        Self {
            task_future,
            task_context,
        }
    }

    #[inline]
    pub fn from_future(task_future: TaskFuture) -> Self {
        Self {
            task_future,
            task_context: Default::default(),
        }
    }
}

/// Data necessary to build a task (create a Future representing that task).
pub struct TaskBuildContext<'a> {
    pub(crate) task: &'a Task,
    pub(crate) state: &'a WorkerState,
    pub(crate) resource_index: usize,
    pub(crate) token: Option<Token>,
}

impl<'a> TaskBuildContext<'a> {
    pub fn task_id(&self) -> TaskId {
        self.task.id
    }

    pub fn resources(&self) -> &'a ResourceRequest {
        &self.task.resources.requests()[self.resource_index]
    }

    pub fn data_flags(&self) -> TaskDataFlags {
        self.task.data_flags
    }

    pub fn n_resource_variants(&self) -> usize {
        self.task.resources.requests().len()
    }

    pub fn resource_variant(&self) -> usize {
        self.resource_index
    }

    pub fn allocation(&self) -> &'a Allocation {
        self.task.resource_allocation().unwrap()
    }

    pub fn body(&self) -> &'a [u8] {
        &self.task.body
    }

    pub fn worker_configuration(&self) -> &WorkerConfiguration {
        &self.state.configuration
    }

    pub fn node_list(&self) -> &'a [WorkerId] {
        &self.task.node_list
    }

    pub fn instance_id(&self) -> InstanceId {
        self.task.instance_id
    }

    pub fn worker_hostname(&self, worker_id: WorkerId) -> Option<&str> {
        self.state.worker_hostname(worker_id)
    }

    pub fn get_resource_map(&self) -> &ResourceMap {
        self.state.get_resource_map()
    }

    pub fn get_resource_label_map(&self) -> &ResourceLabelMap {
        self.state.get_resource_label_map()
    }

    pub fn data_access_key(&self) -> Option<BString> {
        self.token
            .as_ref()
            .map(|token| self.state.lc_state.borrow().data_access_key(token))
    }

    pub fn server_uid(&self) -> &str {
        self.state.server_uid()
    }
}

pub trait TaskLauncher {
    /// Creates data required to execute a task.
    /// `state_ref` must not be borrowed when this function is called.  
    fn build_task(
        &self,
        ctx: TaskBuildContext,
        stop_receiver: tokio::sync::oneshot::Receiver<StopReason>,
    ) -> crate::Result<TaskLaunchData>;
}

/// Create an output stream file on the given path.
/// If the path is relative, the file will be created relative to `cwd`.
fn create_output_stream(def: &StdioDef, cwd: &Path) -> crate::Result<Stdio> {
    let stdio = match def {
        StdioDef::File { path, .. } => {
            let stream_path = if path.is_relative() {
                cwd.join(path)
            } else {
                path.clone()
            };

            let file = File::create(stream_path)
                .map_err(|e| format!("Creating stream file failed: {e}"))?;
            Stdio::from(file)
        }
        StdioDef::Null => Stdio::null(),
        StdioDef::Pipe => Stdio::piped(),
    };
    Ok(stdio)
}

pub fn command_from_definitions(definition: &ProgramDefinition) -> crate::Result<Command> {
    if definition.args.is_empty() {
        return Result::Err(crate::Error::GenericError(
            "No command arguments".to_string(),
        ));
    }

    let mut command = Command::new(definition.args[0].to_os_str_lossy());

    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(|| {
            // We need to create a new process group for the task, so that we can
            // send signals to it without also sending a signal to "us" (the current worker).
            if let Err(error) = nix::unistd::setsid() {
                log::error!("Cannot set SID for task process: {error:?}");
            }
            // Send SIGTERM to this task when the parent (worker) dies.
            let ret = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            match ret {
                0 => {}
                error => log::error!("Cannot set PR_SET_PDEATHSIG for task process: {error:?}"),
            }
            Ok(())
        });
    }

    command.kill_on_drop(true);
    command.args(definition.args[1..].iter().map(|x| x.to_os_str_lossy()));

    if !definition.cwd.is_dir() {
        std::fs::create_dir_all(&definition.cwd).map_err(|error| {
            GenericError(format!("Could not create working directory: {error:?}"))
        })?;
    }
    command.current_dir(&definition.cwd);

    command.stdout(create_output_stream(&definition.stdout, &definition.cwd)?);
    command.stderr(create_output_stream(&definition.stderr, &definition.cwd)?);

    command.stdin(if definition.stdin.is_empty() {
        Stdio::null()
    } else {
        Stdio::piped()
    });

    for (k, v) in definition.env.iter() {
        command.env(k.to_os_str_lossy(), v.to_os_str_lossy());
    }

    Ok(command)
}
