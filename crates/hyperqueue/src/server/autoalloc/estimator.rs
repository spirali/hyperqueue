use crate::server::autoalloc::state::AllocationQueue;
use crate::server::autoalloc::QueueInfo;
use crate::server::job::Job;
use crate::server::state::State;
use crate::server::worker::Worker;
use crate::transfer::messages::{JobTaskDescription, TaskDescription};
use crate::JobId;
use std::time::Duration;
use tako::Map;

pub type WaitingTaskCount = u64;

#[derive(Debug)]
pub struct ServerTaskState {
    // Job to number of waiting tasks
    pub jobs: Map<JobId, WaitingTaskCount>,
}

impl ServerTaskState {
    pub fn waiting_tasks(&self) -> WaitingTaskCount {
        self.jobs.values().sum()
    }

    pub fn remove_waiting_tasks(&mut self, mut task_count: u64) {
        for waiting in self.jobs.values_mut() {
            let to_remove = std::cmp::min(*waiting, task_count);
            *waiting -= to_remove;
            task_count -= to_remove;

            if task_count == 0 {
                break;
            }
        }
        self.jobs = self
            .jobs
            .iter()
            .filter(|&(_, waiting)| *waiting > 0)
            .map(|(id, waiting)| (*id, *waiting))
            .collect();
    }
}

pub fn get_server_task_state(state: &State, queue_info: &QueueInfo) -> ServerTaskState {
    let jobs: Map<JobId, WaitingTaskCount> = state
        .jobs()
        .filter(|job| !job.is_terminated() && can_queue_execute_job(job, queue_info))
        .map(|job| {
            (
                job.job_id,
                job.counters.n_waiting_tasks(job.n_tasks()) as u64,
            )
        })
        .collect();
    ServerTaskState { jobs }
}

/// Guesses if workers from the given queue can compute tasks from the given job.
fn can_queue_execute_job(job: &Job, queue_info: &QueueInfo) -> bool {
    job.submit_descs
        .iter()
        .all(|submit_desc| match &submit_desc.task_desc {
            JobTaskDescription::Array {
                task_desc: TaskDescription { resources, .. },
                ..
            } => resources.min_time() <= queue_info.timelimit(),
            JobTaskDescription::Graph { tasks } => {
                // TODO: optimize
                tasks
                    .iter()
                    .map(|t| t.task_desc.resources.min_time())
                    .min()
                    .unwrap_or(Duration::ZERO)
                    <= queue_info.timelimit()
            }
        })
}

pub fn can_worker_execute_job(_job: &Job, worker: &Worker) -> bool {
    // TODO
    worker.is_running()
}

pub fn count_active_workers(queue: &AllocationQueue) -> u64 {
    queue
        .active_allocations()
        .map(|allocation| allocation.target_worker_count)
        .sum()
}
