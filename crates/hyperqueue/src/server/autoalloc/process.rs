use std::time::SystemTime;

use crate::server::autoalloc::state::{Allocation, AllocationEvent, AllocationStatus};
use crate::server::autoalloc::{DescriptorId, QueueInfo};
use crate::server::job::Job;
use crate::server::state::{State, StateRef};

macro_rules! get_or_return {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            _ => return,
        }
    };
}

macro_rules! get_or_continue {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            _ => continue,
        }
    };
}

/// The main entrypoint of the autoalloc background process.
/// It invokes the autoalloc logic in fixed time intervals.
pub async fn autoalloc_process(state_ref: StateRef) {
    let duration = state_ref.get().get_autoalloc_state().refresh_interval();
    let mut interval = tokio::time::interval(duration);
    loop {
        interval.tick().await;
        autoalloc_tick(&state_ref).await;
    }
}

/// Removes all remaining active allocations
pub async fn autoalloc_shutdown(state_ref: StateRef) {
    let futures: Vec<_> = {
        state_ref
            .get()
            .get_autoalloc_state()
            .descriptors()
            .flat_map(|(_, descriptor)| {
                let handler = descriptor.descriptor.handler();
                descriptor
                    .all_allocations()
                    .filter(|alloc| alloc.is_active())
                    .map(move |alloc| {
                        let fut = handler.remove_allocation(alloc.id.clone());
                        let id = alloc.id.clone();
                        async move { (fut.await, id) }
                    })
            })
            .collect()
    };

    for (result, allocation_id) in futures::future::join_all(futures).await {
        match result {
            Ok(_) => {
                log::info!("Allocation {} was removed", allocation_id);
            }
            Err(e) => {
                log::error!("Failed to remove allocation {}: {:?}", allocation_id, e);
            }
        }
    }
}

async fn autoalloc_tick(state_ref: &StateRef) {
    log::debug!("Running autoalloc");

    let futures: Vec<_> = state_ref
        .get()
        .get_autoalloc_state()
        .descriptor_ids()
        .map(|id| process_descriptor(id, state_ref))
        .collect();
    futures::future::join_all(futures).await;
}

async fn process_descriptor(id: DescriptorId, state: &StateRef) {
    // TODO: check only once in a while
    refresh_allocations(id, state).await;
    schedule_new_allocations(id, state).await
}

/// Go through the allocations of descriptor with the given name and refresh their status.
/// Queue allocations might become running or finished, running allocations might become finished,
/// etc.
#[allow(clippy::needless_collect)]
async fn refresh_allocations(id: DescriptorId, state_ref: &StateRef) {
    let allocation_ids: Vec<_> =
        get_or_return!(state_ref.get().get_autoalloc_state().get_descriptor(id))
            .active_allocations()
            .map(|alloc| alloc.id.clone())
            .collect();
    for allocation_id in allocation_ids.into_iter() {
        let status_fut = get_or_return!(state_ref.get().get_autoalloc_state().get_descriptor(id))
            .descriptor
            .handler()
            .get_allocation_status(allocation_id.clone());

        let result = status_fut.await;

        let mut state = state_ref.get_mut();
        let state = state.get_autoalloc_state_mut();
        match result {
            Ok(status) => {
                match status {
                    Some(status) => {
                        let descriptor = get_or_continue!(state.get_descriptor_mut(id));
                        let id = allocation_id.clone();
                        log::debug!("Status of allocation {}: {:?}", allocation_id, status);
                        match status {
                            AllocationStatus::Running { .. } => {
                                let allocation =
                                    get_or_continue!(descriptor.get_allocation_mut(&allocation_id));
                                if let AllocationStatus::Queued = allocation.status {
                                    descriptor.add_event(AllocationEvent::AllocationStarted(
                                        allocation_id,
                                    ));
                                }
                            }
                            AllocationStatus::Finished { .. } => {
                                descriptor
                                    .add_event(AllocationEvent::AllocationFinished(allocation_id));
                            }
                            AllocationStatus::Failed { .. } => {
                                descriptor
                                    .add_event(AllocationEvent::AllocationFailed(allocation_id));
                            }
                            AllocationStatus::Queued => {}
                        };
                        get_or_continue!(descriptor.get_allocation_mut(&id)).status = status;
                    }
                    None => {
                        log::warn!("Allocation {} was not found", allocation_id);
                        let descriptor = get_or_continue!(state.get_descriptor_mut(id));
                        descriptor.remove_allocation(&allocation_id);
                        descriptor.add_event(AllocationEvent::AllocationDisappeared(allocation_id));
                    }
                };
            }
            Err(err) => {
                log::error!(
                    "Failed to get allocation {} status from {}: {:?}",
                    allocation_id,
                    id,
                    err
                );
                let descriptor = get_or_continue!(state.get_descriptor_mut(id));
                descriptor.add_event(AllocationEvent::StatusFail {
                    error: format!("{:?}", err),
                });
            }
        }
    }
}

/// Find out if workers spawned in this queue can possibly provide computational resources
/// for tasks of this job.
///
/// TODO: once HQ jobs are heterogeneous, the implementation will need to be modified
fn can_provide_worker(job: &Job, queue_info: &QueueInfo) -> bool {
    job.resources.min_time < queue_info.timelimit()
}

fn count_available_tasks(state: &State, queue_info: &QueueInfo) -> u64 {
    let waiting_tasks: u64 = state
        .jobs()
        .map(|job| {
            let result = match can_provide_worker(job, queue_info) {
                true => job.counters.n_waiting_tasks(job.n_tasks()),
                false => 0,
            };
            result as u64
        })
        .sum();
    waiting_tasks
}

/// Schedule new allocations for the descriptor with the given name.
async fn schedule_new_allocations(id: DescriptorId, state_ref: &StateRef) {
    let (allocations_to_create, workers_per_alloc, mut waiting_tasks) = {
        let state = state_ref.get();
        let descriptor = get_or_return!(state.get_autoalloc_state().get_descriptor(id));
        let allocs_in_queue = descriptor.queued_allocations().count();

        let waiting_tasks = count_available_tasks(&state, descriptor.descriptor.info());

        let info = descriptor.descriptor.info();
        (
            info.backlog().saturating_sub(allocs_in_queue as u32),
            info.workers_per_alloc() as u64,
            waiting_tasks,
        )
    };

    if waiting_tasks == 0 {
        log::debug!("No waiting tasks found, no new allocations will be created");
        return;
    }

    for _ in 0..allocations_to_create {
        let schedule_fut = {
            let mut state = state_ref.get_mut();
            let descriptor = get_or_return!(state.get_autoalloc_state_mut().get_descriptor_mut(id));
            let info = descriptor.descriptor.info().clone();
            descriptor
                .descriptor
                .handler_mut()
                .schedule_allocation(id, &info, workers_per_alloc)
        };

        let result = schedule_fut.await;

        let mut state = state_ref.get_mut();
        let state = state.get_autoalloc_state_mut();
        let descriptor = get_or_return!(state.get_descriptor_mut(id));
        match result {
            Ok(created) => {
                log::info!("Queued {} workers into queue {}", workers_per_alloc, id);
                descriptor.add_event(AllocationEvent::AllocationQueued(created.id().to_string()));
                descriptor.add_allocation(Allocation {
                    id: created.id().to_string(),
                    worker_count: workers_per_alloc,
                    queued_at: SystemTime::now(),
                    status: AllocationStatus::Queued,
                    working_dir: created.working_dir().to_path_buf(),
                });

                // If there are no more waiting tasks, stop creating allocations
                // Assume that each worker will handle at least a single task
                waiting_tasks = waiting_tasks.saturating_sub(workers_per_alloc);
                if waiting_tasks == 0 {
                    break;
                }
            }
            Err(err) => {
                log::error!("Failed to queue allocation into queue {}: {:?}", id, err);
                descriptor.add_event(AllocationEvent::QueueFail {
                    error: format!("{:?}", err),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::time::{Duration, SystemTime};

    use crate::common::arraydef::IntArray;
    use hashbrown::HashMap;
    use tako::common::resources::TimeRequest;
    use tako::messages::common::ProgramDefinition;
    use tako::messages::gateway::ResourceRequest;

    use crate::common::manager::info::ManagerType;
    use crate::server::autoalloc::descriptor::{
        CreatedAllocation, QueueDescriptor, QueueHandler, QueueInfo,
    };
    use crate::server::autoalloc::process::autoalloc_tick;
    use crate::server::autoalloc::state::{AllocationEvent, AllocationId, AllocationStatus};
    use crate::server::autoalloc::{Allocation, AutoAllocResult, DescriptorId};
    use crate::server::job::Job;
    use crate::server::state::StateRef;
    use crate::transfer::messages::JobType;
    use crate::WrappedRcRefCell;

    #[tokio::test]
    async fn test_log_failed_allocation_attempt() {
        let state = create_state(1000);

        let handler = Handler::new(
            WrappedRcRefCell::wrap(()),
            move |_, _| async move { anyhow::bail!("foo") },
            move |_, _| async move { Ok(Some(AllocationStatus::Queued)) },
            |_, _| async move { Ok(()) },
        );
        add_descriptor(&state, handler, 1, 1, Duration::from_secs(60));

        autoalloc_tick(&state).await;

        let state = state.get();
        let state = state.get_autoalloc_state();
        let descriptor = state.get_descriptor(0).unwrap();
        matches!(
            descriptor.get_events()[0].event,
            AllocationEvent::QueueFail { .. }
        );
    }

    #[tokio::test]
    async fn test_fill_backlog() {
        let state = create_state(1000);

        let handler = always_queued_handler();
        add_descriptor(&state, handler, 4, 2, Duration::from_secs(60));

        autoalloc_tick(&state).await;

        let allocations = get_allocations(&state, 0);
        assert_eq!(allocations.len(), 4);
        assert!(allocations.iter().all(|alloc| alloc.worker_count == 2));
    }

    #[tokio::test]
    async fn test_do_nothing_on_full_backlog() {
        let state = create_state(1000);

        let handler = always_queued_handler();
        add_descriptor(&state, handler, 4, 1, Duration::from_secs(60));

        autoalloc_tick(&state).await;
        autoalloc_tick(&state).await;
        autoalloc_tick(&state).await;
        autoalloc_tick(&state).await;

        assert_eq!(get_allocations(&state, 0).len(), 4);
    }

    #[tokio::test]
    async fn test_keep_backlog_filled() {
        let state = create_state(1000);

        let mut queue = HashMap::<AllocationId, isize>::new();
        queue.insert("0".to_string(), 0); // run immediately
        queue.insert("1".to_string(), 2); // run after two checks
        queue.insert("2".to_string(), 3); // run after three checks

        let handler = Handler::new(
            WrappedRcRefCell::wrap((0, queue)),
            move |state, _| async move {
                let id_state = &mut state.get_mut().0;
                let id = *id_state;
                *id_state += 1;
                Ok(CreatedAllocation::new(id.to_string(), Default::default()))
            },
            move |state, id| async move {
                let queue_state = &mut state.get_mut().1;
                let queue_time = *queue_state.get(&id).unwrap_or(&1000);
                queue_state.insert(id, queue_time - 1);

                let status = if queue_time <= 0 {
                    AllocationStatus::Running {
                        started_at: SystemTime::now(),
                    }
                } else {
                    AllocationStatus::Queued
                };
                Ok(Some(status))
            },
            |_, _| async move { Ok(()) },
        );
        add_descriptor(&state, handler, 3, 1, Duration::from_secs(60));

        // schedule allocations
        autoalloc_tick(&state).await;
        check_allocation_count(get_allocations(&state, 0), 3, 0);

        // add new job to queue
        autoalloc_tick(&state).await;
        check_allocation_count(get_allocations(&state, 0), 3, 1);

        autoalloc_tick(&state).await;
        check_allocation_count(get_allocations(&state, 0), 3, 1);

        autoalloc_tick(&state).await;
        check_allocation_count(get_allocations(&state, 0), 3, 2);

        autoalloc_tick(&state).await;
        check_allocation_count(get_allocations(&state, 0), 3, 3);
    }

    #[tokio::test]
    async fn test_do_not_create_allocations_without_tasks() {
        let state = create_state(0);

        let handler = always_queued_handler();
        add_descriptor(&state, handler, 3, 1, Duration::from_secs(60));

        autoalloc_tick(&state).await;
        assert_eq!(get_allocations(&state, 0).len(), 0);
    }

    #[tokio::test]
    async fn test_do_not_fill_backlog_when_tasks_run_out() {
        let state = create_state(5);

        let handler = always_queued_handler();
        add_descriptor(&state, handler, 5, 2, Duration::from_secs(60));

        // 5 tasks, 3 * 2 workers -> last two allocations should be ignored
        autoalloc_tick(&state).await;
        assert_eq!(get_allocations(&state, 0).len(), 3);
    }

    #[tokio::test]
    async fn test_ignore_task_with_high_time_request() {
        let state = create_state(0);
        state
            .get_mut()
            .add_job(create_job(1, Duration::from_secs(60 * 60)));

        let handler = always_queued_handler();
        add_descriptor(&state, handler, 1, 1, Duration::from_secs(60 * 30));

        // Allocations last for 30 minutes, but job requires 60 minutes
        // Nothing should be scheduled
        autoalloc_tick(&state).await;
        assert_eq!(get_allocations(&state, 0).len(), 0);
    }

    struct Handler<ScheduleFn, StatusFn, RemoveFn, State> {
        schedule_fn: WrappedRcRefCell<ScheduleFn>,
        status_fn: WrappedRcRefCell<StatusFn>,
        remove_fn: WrappedRcRefCell<RemoveFn>,
        custom_state: WrappedRcRefCell<State>,
    }

    impl<
            State: 'static,
            ScheduleFn: 'static + Fn(WrappedRcRefCell<State>, u64) -> ScheduleFnFut,
            ScheduleFnFut: Future<Output = AutoAllocResult<CreatedAllocation>>,
            StatusFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> StatusFnFut,
            StatusFnFut: Future<Output = AutoAllocResult<Option<AllocationStatus>>>,
            RemoveFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> RemoveFnFut,
            RemoveFnFut: Future<Output = AutoAllocResult<()>>,
        > Handler<ScheduleFn, StatusFn, RemoveFn, State>
    {
        fn new(
            custom_state: WrappedRcRefCell<State>,
            schedule_fn: ScheduleFn,
            status_fn: StatusFn,
            remove_fn: RemoveFn,
        ) -> Box<dyn QueueHandler> {
            Box::new(Self {
                schedule_fn: WrappedRcRefCell::wrap(schedule_fn),
                status_fn: WrappedRcRefCell::wrap(status_fn),
                remove_fn: WrappedRcRefCell::wrap(remove_fn),
                custom_state,
            })
        }
    }

    impl<
            State: 'static,
            ScheduleFn: 'static + Fn(WrappedRcRefCell<State>, u64) -> ScheduleFnFut,
            ScheduleFnFut: Future<Output = AutoAllocResult<CreatedAllocation>>,
            StatusFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> StatusFnFut,
            StatusFnFut: Future<Output = AutoAllocResult<Option<AllocationStatus>>>,
            RemoveFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> RemoveFnFut,
            RemoveFnFut: Future<Output = AutoAllocResult<()>>,
        > QueueHandler for Handler<ScheduleFn, StatusFn, RemoveFn, State>
    {
        fn schedule_allocation(
            &mut self,
            _descriptor_id: DescriptorId,
            _queue_info: &QueueInfo,
            worker_count: u64,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<CreatedAllocation>>>> {
            let schedule_fn = self.schedule_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (schedule_fn.get())(custom_state.clone(), worker_count).await })
        }

        fn get_allocation_status(
            &self,
            allocation_id: AllocationId,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<Option<AllocationStatus>>>>> {
            let status_fn = self.status_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (status_fn.get())(custom_state.clone(), allocation_id).await })
        }

        fn remove_allocation(
            &self,
            allocation_id: AllocationId,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<()>>>> {
            let remove_fn = self.remove_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (remove_fn.get())(custom_state.clone(), allocation_id).await })
        }
    }

    fn add_descriptor(
        state_ref: &StateRef,
        handler: Box<dyn QueueHandler>,
        backlog: u32,
        workers_per_alloc: u32,
        timelimit: Duration,
    ) {
        let descriptor = QueueDescriptor::new(
            ManagerType::Pbs,
            QueueInfo::new(backlog, workers_per_alloc, timelimit, vec![], None, vec![]),
            None,
            handler,
        );

        let mut state = state_ref.get_mut();
        let state = state.get_autoalloc_state_mut();
        state.add_descriptor(state.descriptors().count() as DescriptorId, descriptor)
    }

    fn create_state(waiting_tasks: u32) -> StateRef {
        let state = StateRef::new(Duration::from_millis(100));
        if waiting_tasks > 0 {
            state
                .get_mut()
                .add_job(create_job(waiting_tasks, Duration::from_secs(0)));
        }
        state
    }

    fn always_queued_handler() -> Box<dyn QueueHandler> {
        Handler::new(
            WrappedRcRefCell::wrap(0),
            move |state, _| async move {
                let mut s = state.get_mut();
                let id = *s;
                *s += 1;
                Ok(CreatedAllocation::new(id.to_string(), Default::default()))
            },
            move |_, _| async move { Ok(Some(AllocationStatus::Queued)) },
            |_, _| async move { Ok(()) },
        )
    }

    fn get_allocations(state: &StateRef, descriptor: DescriptorId) -> Vec<Allocation> {
        let state = state.get();
        let state = state.get_autoalloc_state();
        state
            .get_descriptor(descriptor)
            .unwrap()
            .all_allocations()
            .cloned()
            .collect()
    }

    fn check_allocation_count(allocations: Vec<Allocation>, queued: usize, running: usize) {
        assert_eq!(
            queued,
            allocations
                .iter()
                .filter(|a| matches!(a.status, AllocationStatus::Queued))
                .count()
        );
        assert_eq!(
            running,
            allocations
                .iter()
                .filter(|a| matches!(a.status, AllocationStatus::Running { .. }))
                .count()
        );
    }

    fn create_job(tasks: u32, min_time: TimeRequest) -> Job {
        Job::new(
            JobType::Array(IntArray::from_range(0, tasks)),
            0.into(),
            0.into(),
            "job".to_string(),
            ProgramDefinition {
                args: vec![],
                env: Default::default(),
                stdout: Default::default(),
                stderr: Default::default(),
                cwd: None,
            },
            ResourceRequest {
                cpus: Default::default(),
                generic: vec![],
                min_time,
            },
            false,
            None,
            None,
            0,
            None,
            None,
        )
    }
}
