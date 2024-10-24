use std::cmp::min;
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use tako::gateway::{
    CancelTasks, FromGatewayMessage, LostWorkerMessage, NewWorkerMessage, TaskFailedMessage,
    TaskState, TaskUpdate, ToGatewayMessage,
};
use tako::ItemId;
use tako::{define_wrapped_type, TaskId};

use crate::server::autoalloc::LostWorkerDetails;
use crate::server::job::{Job, JobTaskInfo, JobTaskState};
use crate::server::restore::StateRestorer;
use crate::server::worker::Worker;
use crate::server::Senders;
use crate::transfer::messages::{JobSubmitDescription, JobTaskDescription, ServerInfo};
use crate::{make_tako_id, unwrap_tako_id, JobTaskId, WrappedRcRefCell};
use crate::{JobId, JobTaskCount, Map, TakoTaskId, WorkerId};

pub struct State {
    jobs: Map<JobId, Job>,
    workers: Map<WorkerId, Worker>,
    job_id_counter: <JobId as ItemId>::IdType,
    server_info: ServerInfo,
}

define_wrapped_type!(StateRef, State, pub);

fn cancel_tasks_from_callback(
    state_ref: &StateRef,
    senders: &Senders,
    job_id: JobId,
    tasks: Vec<TakoTaskId>,
) {
    if tasks.is_empty() {
        return;
    }
    log::debug!("Canceling {:?} tasks", tasks);
    let senders2 = senders.clone();
    let state_ref = state_ref.clone();
    tokio::task::spawn_local(async move {
        let message = FromGatewayMessage::CancelTasks(CancelTasks { tasks });
        let response = senders2.backend.send_tako_message(message).await.unwrap();

        match response {
            ToGatewayMessage::CancelTasksResponse(msg) => {
                let mut state = state_ref.get_mut();
                if let Some(job) = state.get_job_mut(job_id) {
                    log::debug!("Tasks {:?} canceled", msg.cancelled_tasks);
                    log::debug!("Tasks {:?} already finished", msg.already_finished);
                    for tako_id in msg.cancelled_tasks {
                        let (j_id, task_id) = unwrap_tako_id(tako_id);
                        assert_eq!(j_id, job.job_id);
                        job.set_cancel_state(task_id, &senders2);
                    }
                }
            }
            ToGatewayMessage::Error(msg) => {
                log::debug!("Canceling job {} failed: {}", job_id, msg.message);
            }
            _ => {
                panic!("Invalid message");
            }
        };
    });
}

impl State {
    pub fn get_job(&self, job_id: JobId) -> Option<&Job> {
        self.jobs.get(&job_id)
    }

    pub fn get_job_mut(&mut self, job_id: JobId) -> Option<&mut Job> {
        self.jobs.get_mut(&job_id)
    }

    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    pub fn add_worker(&mut self, worker: Worker) {
        let worker_id = worker.worker_id();
        assert!(self.workers.insert(worker_id, worker).is_none())
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    pub fn set_worker_port(&mut self, port: u16) {
        self.server_info.worker_port = port;
    }

    pub fn add_job(&mut self, job: Job) {
        assert!(job.submit_descs.is_empty());
        let job_id = job.job_id;
        assert!(self.jobs.insert(job_id, job).is_none());
    }

    /// Completely forgets this job, in order to reduce memory usage.
    pub(crate) fn forget_job(&mut self, job_id: JobId) -> Option<Job> {
        let job = match self.jobs.remove(&job_id) {
            Some(job) => {
                assert!(job.is_terminated());
                job
            }
            None => {
                log::error!("Trying to forget unknown job {job_id}");
                return None;
            }
        };
        Some(job)
    }

    pub fn new_job_id(&mut self) -> JobId {
        let id = self.job_id_counter;
        self.job_id_counter += 1;
        id.into()
    }

    pub fn revert_to_job_id(&mut self, id: JobId) {
        self.job_id_counter = id.as_num();
    }

    pub fn last_n_ids(&self, n: u32) -> impl Iterator<Item = JobId> {
        let n = min(n, self.job_id_counter - 1);
        ((self.job_id_counter - n)..self.job_id_counter).map(|id| id.into())
    }

    pub fn get_workers(&self) -> &Map<WorkerId, Worker> {
        &self.workers
    }

    pub fn get_worker(&self, worker_id: WorkerId) -> Option<&Worker> {
        self.workers.get(&worker_id)
    }

    pub fn get_worker_mut(&mut self, worker_id: WorkerId) -> Option<&mut Worker> {
        self.workers.get_mut(&worker_id)
    }

    pub fn process_task_failed(
        &mut self,
        state_ref: &StateRef,
        senders: &Senders,
        msg: TaskFailedMessage,
    ) {
        log::debug!("Task id={} failed: {:?}", msg.id, msg.info);

        let (job_id, task_id) = unwrap_tako_id(msg.id);
        let job = self.get_job_mut(job_id).unwrap();
        for tako_id in msg.cancelled_tasks {
            log::debug!(
                "Task id={} canceled because of task dependency fails",
                tako_id
            );
            let (j_id, task_id) = unwrap_tako_id(tako_id);
            assert_eq!(job_id, j_id);
            job.set_cancel_state(task_id, senders);
        }
        job.set_failed_state(task_id, msg.info.message, senders);

        if let Some(max_fails) = &job.job_desc.max_fails {
            if job.counters.n_failed_tasks > *max_fails {
                let task_ids = job.non_finished_task_ids();
                cancel_tasks_from_callback(state_ref, senders, job.job_id, task_ids);
            }
        }
    }

    pub fn process_task_update(&mut self, msg: TaskUpdate, senders: &Senders) {
        log::debug!("Task id={} updated {:?}", msg.id, msg.state);
        match msg.state {
            TaskState::Running {
                instance_id,
                worker_ids,
                context,
            } => {
                let (job_id, task_id) = unwrap_tako_id(msg.id);
                let job = self.get_job_mut(job_id).unwrap();
                job.set_running_state(task_id, worker_ids.clone(), context);
                senders
                    .events
                    .on_task_started(job_id, task_id, instance_id, worker_ids.clone());
            }
            TaskState::Finished => {
                let now = Utc::now();
                let (job_id, task_id) = unwrap_tako_id(msg.id);
                let job = self.get_job_mut(job_id).unwrap();
                job.set_finished_state(task_id, now, senders);
            }
            TaskState::Waiting => {
                let (job_id, task_id) = unwrap_tako_id(msg.id);
                let job = self.get_job_mut(job_id).unwrap();
                job.set_waiting_state(task_id);
            }
            TaskState::Invalid => {
                unreachable!()
            }
        };
    }

    pub fn process_worker_new(&mut self, msg: NewWorkerMessage, senders: &Senders) {
        log::debug!("New worker id={}", msg.worker_id);
        self.add_worker(Worker::new(msg.worker_id, msg.configuration.clone()));
        // TODO: use observer in event storage instead of sending these messages directly
        senders
            .autoalloc
            .on_worker_connected(msg.worker_id, &msg.configuration);
        senders
            .events
            .on_worker_added(msg.worker_id, msg.configuration);
    }

    pub fn process_worker_lost(
        &mut self,
        _state_ref: &StateRef,
        senders: &Senders,
        msg: LostWorkerMessage,
    ) {
        log::debug!("Worker lost id={}", msg.worker_id);
        for tako_id in msg.running_tasks {
            let (job_id, task_id) = unwrap_tako_id(tako_id);
            let job = self.get_job_mut(job_id).unwrap();
            job.set_waiting_state(task_id);
        }

        let worker = self.workers.get_mut(&msg.worker_id).unwrap();
        worker.set_offline_state(msg.reason.clone());

        senders.autoalloc.on_worker_lost(
            msg.worker_id,
            &worker.configuration,
            LostWorkerDetails {
                reason: msg.reason.clone(),
                lifetime: (Utc::now() - worker.started_at()).to_std().unwrap(),
            },
        );
        senders.events.on_worker_lost(msg.worker_id, msg.reason);
    }

    pub(crate) fn restore_state(&mut self, restorer: &StateRestorer) {
        self.job_id_counter = restorer.job_id_counter()
    }
}

impl StateRef {
    pub fn new(server_info: ServerInfo) -> StateRef {
        Self(WrappedRcRefCell::wrap(State {
            jobs: Default::default(),
            workers: Default::default(),
            job_id_counter: 1,
            server_info,
        }))
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;
    use tako::program::{ProgramDefinition, StdioDef};

    use crate::common::arraydef::IntArray;
    use crate::server::job::Job;
    use crate::server::state::State;
    use crate::tests::utils::create_hq_state;
    use crate::transfer::messages::{
        JobDescription, JobSubmitDescription, JobTaskDescription, PinMode, TaskDescription,
        TaskKind, TaskKindProgram,
    };
    use crate::{unwrap_tako_id, JobId, TakoTaskId};

    fn dummy_program_definition() -> ProgramDefinition {
        ProgramDefinition {
            args: vec![],
            env: Default::default(),
            stdout: StdioDef::Null,
            stderr: StdioDef::Null,
            stdin: vec![],
            cwd: Default::default(),
        }
    }

    fn add_test_job<J: Into<JobId>, T: Into<TakoTaskId>>(
        state: &mut State,
        ids: IntArray,
        job_id: J,
        base_task_id: T,
    ) {
        let job_id: JobId = job_id.into();
        let task_desc = JobTaskDescription::Array {
            ids,
            entries: None,
            task_desc: TaskDescription {
                kind: TaskKind::ExternalProgram(TaskKindProgram {
                    program: dummy_program_definition(),
                    pin_mode: PinMode::None,
                    task_dir: false,
                }),
                resources: Default::default(),
                time_limit: None,
                priority: 0,
                crash_limit: 5,
            },
        };
        let mut job = Job::new(
            job_id,
            JobDescription {
                name: "".to_string(),
                max_fails: None,
            },
            false,
        );
        job.attach_submit(Arc::new(JobSubmitDescription {
            task_desc,
            submit_dir: Default::default(),
            stream_path: None,
        }));
        state.add_job(job);
    }
}
