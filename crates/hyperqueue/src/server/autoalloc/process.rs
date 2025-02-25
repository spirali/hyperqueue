use std::future::Future;
use std::path::PathBuf;
use std::time::SystemTime;

use futures::future::join_all;
use tako::WorkerId;
use tako::{Map, Set};
use tempfile::TempDir;

use crate::common::manager::info::{ManagerInfo, ManagerType};
use crate::common::rpc::RpcReceiver;
use crate::server::autoalloc::config::{
    get_refresh_timeout, get_status_check_interval, max_allocation_fails,
    MAX_QUEUED_STATUS_ERROR_COUNT, MAX_RUNNING_STATUS_ERROR_COUNT, MAX_SUBMISSION_FAILS,
    SUBMISSION_DELAYS,
};
use crate::server::autoalloc::estimator::{
    can_worker_execute_job, count_active_workers, get_server_task_state,
};
use crate::server::autoalloc::queue::pbs::PbsHandler;
use crate::server::autoalloc::queue::slurm::SlurmHandler;
use crate::server::autoalloc::queue::{AllocationExternalStatus, QueueHandler, SubmitMode};
use crate::server::autoalloc::service::{AutoAllocMessage, LostWorkerDetails};
use crate::server::autoalloc::state::{
    AllocationQueue, AllocationQueueState, AllocationState, AutoAllocState, RateLimiter,
    RateLimiterStatus,
};
use crate::server::autoalloc::{Allocation, AllocationId, AutoAllocResult, QueueId, QueueInfo};
use crate::server::event::streamer::EventStreamer;
use crate::server::state::StateRef;
use crate::transfer::messages::{AllocationQueueParams, QueueData, QueueState};
use crate::{get_or_return, JobId};

#[derive(Copy, Clone)]
enum RefreshReason {
    UpdateAllQueues,
    UpdateQueue(QueueId),
    NewJob(JobId),
}

/// This is the main autoalloc event loop, which periodically refreshes the autoalloc queue state
/// and also reacts to external autoalloc messages.
pub async fn autoalloc_process(
    state_ref: StateRef,
    events: EventStreamer,
    mut autoalloc: AutoAllocState,
    mut receiver: RpcReceiver<AutoAllocMessage>,
) {
    let timeout = get_refresh_timeout();
    loop {
        let refresh_reason = match tokio::time::timeout(timeout, receiver.recv()).await {
            Ok(None) | Ok(Some(AutoAllocMessage::QuitService)) => break,
            Ok(Some(message)) => handle_message(&mut autoalloc, &events, message).await,
            Err(_) => {
                log::debug!(
                    "No message received in {} ms, refreshing state",
                    timeout.as_millis()
                );
                Some(RefreshReason::UpdateAllQueues)
            }
        };
        if let Some(reason) = refresh_reason {
            refresh_state(&state_ref, &events, &mut autoalloc, reason).await;
        }
    }
    log::debug!("Ending autoalloc, stopping all allocations");
    stop_all_allocations(&autoalloc).await;
}

/// Reacts to auto allocation message and returns a queue that should be refreshed.
async fn handle_message(
    autoalloc: &mut AutoAllocState,
    events: &EventStreamer,
    message: AutoAllocMessage,
) -> Option<RefreshReason> {
    log::debug!("Handling message {message:?}");
    match message {
        AutoAllocMessage::WorkerConnected(id, manager_info) => {
            log::debug!(
                "Registering worker {id} for allocation {}",
                manager_info.allocation_id
            );
            if let Some((queue, queue_id, allocation_id)) =
                get_data_from_worker(autoalloc, &manager_info)
            {
                log::info!(
                    "Worker {id} connected from allocation {}",
                    manager_info.allocation_id
                );
                sync_allocation_status(
                    events,
                    queue_id,
                    queue,
                    &allocation_id,
                    AllocationSyncReason::WorkedConnected(id),
                );
                autoalloc
                    .get_queue_id_by_allocation(&manager_info.allocation_id)
                    .map(RefreshReason::UpdateQueue)
            } else {
                log::warn!(
                    "Worker {id} connected from an unknown allocation {}",
                    manager_info.allocation_id
                );
                None
            }
        }
        AutoAllocMessage::WorkerLost(id, manager_info, details) => {
            log::debug!(
                "Removing worker {id} from allocation {}",
                manager_info.allocation_id
            );
            if let Some((queue, queue_id, allocation_id)) =
                get_data_from_worker(autoalloc, &manager_info)
            {
                sync_allocation_status(
                    events,
                    queue_id,
                    queue,
                    &allocation_id,
                    AllocationSyncReason::WorkerLost(id, details),
                );
                autoalloc
                    .get_queue_id_by_allocation(&manager_info.allocation_id)
                    .map(RefreshReason::UpdateQueue)
            } else {
                log::warn!(
                    "Worker {id} disconnected to an unknown allocation {}",
                    manager_info.allocation_id
                );
                None
            }
        }
        AutoAllocMessage::JobCreated(id) => {
            log::debug!("Registering job {id}");
            Some(RefreshReason::NewJob(id))
        }
        AutoAllocMessage::GetQueues(response) => {
            let queues: Map<QueueId, QueueData> = autoalloc
                .queues()
                .map(|(id, queue)| {
                    (
                        id,
                        QueueData {
                            info: queue.info().clone(),
                            name: queue.name().map(|name| name.to_string()),
                            manager_type: queue.manager().clone(),
                            state: match queue.state() {
                                AllocationQueueState::Running => QueueState::Running,
                                AllocationQueueState::Paused => QueueState::Paused,
                            },
                        },
                    )
                })
                .collect();
            response.respond(queues);
            None
        }
        AutoAllocMessage::AddQueue {
            server_directory,
            params,
            queue_id,
            response,
        } => {
            log::debug!("Creating queue, params={params:?}");
            let result = create_queue(autoalloc, events, server_directory, params, queue_id);
            let queue_id = result.as_ref().ok().copied();
            response.respond(result);
            queue_id.map(RefreshReason::UpdateQueue)
        }
        AutoAllocMessage::RemoveQueue {
            id,
            force,
            response,
        } => {
            log::debug!("Removing queue {id}");
            let result = remove_queue(autoalloc, events, id, force).await;
            response.respond(result);
            None
        }
        AutoAllocMessage::PauseQueue { id, response } => {
            let result = match autoalloc.get_queue_mut(id) {
                Some(queue) => {
                    log::debug!("Pausing queue {id}");
                    queue.pause();
                    Ok(())
                }
                None => Err(anyhow::anyhow!(
                    "Queue {id} that should have been paused was not found"
                )),
            };
            response.respond(result);
            None
        }
        AutoAllocMessage::ResumeQueue { id, response } => {
            let result = match autoalloc.get_queue_mut(id) {
                Some(queue) => {
                    log::debug!("Resuming queue {id}");
                    queue.resume();
                    Ok(())
                }
                None => Err(anyhow::anyhow!(
                    "Queue {id} that should have been resumed was not found"
                )),
            };
            response.respond(result);
            Some(RefreshReason::UpdateQueue(id))
        }
        AutoAllocMessage::GetAllocations(queue_id, response) => {
            let result = match autoalloc.get_queue(queue_id) {
                Some(queue) => Ok(queue.all_allocations().cloned().collect()),
                None => Err(anyhow::anyhow!("Queue {queue_id} not found")),
            };
            response.respond(result);
            None
        }
        AutoAllocMessage::QuitService => unreachable!(),
    }
}

pub async fn try_submit_allocation(params: AllocationQueueParams) -> anyhow::Result<()> {
    let tmpdir = TempDir::with_prefix("hq")?;
    let mut handler = create_allocation_handler(
        &params.manager,
        params.name.clone(),
        tmpdir.as_ref().to_path_buf(),
    )?;
    let worker_count = params.workers_per_alloc;
    let queue_info = create_queue_info(params);

    let allocation = handler
        .submit_allocation(0, &queue_info, worker_count as u64, SubmitMode::DryRun)
        .await
        .map_err(|e| anyhow::anyhow!("Could not submit allocation: {:?}", e))?;

    let working_dir = allocation.working_dir().to_path_buf();
    let id = allocation
        .into_id()
        .map_err(|e| anyhow::anyhow!("Could not submit allocation: {:?}", e))?;
    let allocation = Allocation::new(id.to_string(), worker_count as u64, working_dir);
    handler
        .remove_allocation(&allocation)
        .await
        .map_err(|e| anyhow::anyhow!("Could not cancel allocation {}: {:?}", allocation.id, e))?;

    Ok(())
}

// The code doesn't compile if the Box closures are removed
#[allow(clippy::redundant_closure)]
pub fn create_allocation_handler(
    manager: &ManagerType,
    name: Option<String>,
    directory: PathBuf,
) -> anyhow::Result<Box<dyn QueueHandler>> {
    match manager {
        ManagerType::Pbs => {
            let handler = PbsHandler::new(directory, name);
            handler.map::<Box<dyn QueueHandler>, _>(|handler| Box::new(handler))
        }
        ManagerType::Slurm => {
            let handler = SlurmHandler::new(directory, name);
            handler.map::<Box<dyn QueueHandler>, _>(|handler| Box::new(handler))
        }
    }
}

pub fn create_queue_info(params: AllocationQueueParams) -> QueueInfo {
    let AllocationQueueParams {
        manager,
        name: _name,
        workers_per_alloc,
        backlog,
        timelimit,
        additional_args,
        max_worker_count,
        worker_start_cmd,
        worker_stop_cmd,
        worker_args,
        idle_timeout,
    } = params;
    QueueInfo::new(
        manager,
        backlog,
        workers_per_alloc,
        timelimit,
        additional_args,
        max_worker_count,
        worker_args,
        idle_timeout,
        worker_start_cmd,
        worker_stop_cmd,
    )
}

fn create_rate_limiter() -> RateLimiter {
    RateLimiter::new(
        SUBMISSION_DELAYS.to_vec(),
        MAX_SUBMISSION_FAILS,
        max_allocation_fails(),
        get_status_check_interval(),
    )
}

fn create_queue(
    autoalloc: &mut AutoAllocState,
    events: &EventStreamer,
    server_directory: PathBuf,
    params: AllocationQueueParams,
    queue_id: Option<QueueId>,
) -> anyhow::Result<QueueId> {
    let name = params.name.clone();
    let handler = create_allocation_handler(&params.manager, name.clone(), server_directory);
    let queue_info = create_queue_info(params.clone());

    match handler {
        Ok(handler) => {
            let queue = AllocationQueue::new(queue_info, name, handler, create_rate_limiter());
            let id = {
                let id = autoalloc.add_queue(queue, queue_id);
                if queue_id.is_none() {
                    // When queue_id is provided, then we are restoring journal,
                    // so we do not want to double log the event
                    events.on_allocation_queue_created(id, params);
                }
                id
            };

            Ok(id)
        }
        Err(error) => Err(anyhow::anyhow!("Could not create autoalloc queue: {error}")),
    }
}

// TODO: use proper error type
async fn remove_queue(
    autoalloc: &mut AutoAllocState,
    events: &EventStreamer,
    id: QueueId,
    force: bool,
) -> anyhow::Result<()> {
    let remove_alloc_fut = {
        let queue_state = autoalloc.get_queue_mut(id);

        let fut = match queue_state {
            Some(state) => {
                let has_running_allocations =
                    state.all_allocations().any(|alloc| alloc.is_running());
                if has_running_allocations && !force {
                    return Err(anyhow::anyhow!(
                        "Allocation queue has running jobs, so it will \
not be removed. Use `--force` if you want to remove the queue anyway"
                    ));
                }

                prepare_queue_cleanup(state)
            }
            None => return Err(anyhow::anyhow!("Allocation queue not found")),
        };

        autoalloc.remove_queue(id);
        fut
    };

    for (result, allocation_id) in futures::future::join_all(remove_alloc_fut).await {
        match result {
            Ok(_) => log::info!("Allocation {} was removed", allocation_id),
            Err(e) => log::error!("Failed to remove allocation {}: {:?}", allocation_id, e),
        }
    }

    events.on_allocation_queue_removed(id);

    Ok(())
}

/// Removes all remaining active allocations
async fn stop_all_allocations(autoalloc: &AutoAllocState) {
    let futures = autoalloc
        .queues()
        .flat_map(|(_, queue)| prepare_queue_cleanup(queue));

    for (result, allocation_id) in futures::future::join_all(futures).await {
        match result {
            Ok(_) => {
                log::info!("Allocation {allocation_id} was removed");
            }
            Err(e) => {
                log::error!("Failed to remove allocation {allocation_id}: {e:?}");
            }
        }
    }
}

/// Processes updates either for a single queue or for all queues and removes stale directories
/// from disk.
async fn refresh_state(
    state_ref: &StateRef,
    events: &EventStreamer,
    autoalloc: &mut AutoAllocState,
    reason: RefreshReason,
) {
    let queue_ids: Vec<QueueId> = match reason {
        RefreshReason::UpdateAllQueues | RefreshReason::NewJob(_) => {
            autoalloc.queue_ids().collect()
        }
        RefreshReason::UpdateQueue(id) => {
            vec![id]
        }
    };
    let new_job_id = match reason {
        RefreshReason::NewJob(id) => Some(id),
        _ => None,
    };

    for id in queue_ids {
        refresh_queue_allocations(events, autoalloc, id).await;
        process_queue(state_ref, events, autoalloc, id, new_job_id).await;
    }

    remove_inactive_directories(autoalloc).await;
}

async fn process_queue(
    state_ref: &StateRef,
    events: &EventStreamer,
    autoalloc: &mut AutoAllocState,
    id: QueueId,
    new_job_id: Option<JobId>,
) {
    let try_to_submit = {
        let queue = get_or_return!(autoalloc.get_queue_mut(id));
        if !queue.state().is_running() {
            return;
        } else {
            let limiter = queue.limiter_mut();

            let status = limiter.submission_status();
            let allowed = matches!(status, RateLimiterStatus::Ok);
            if allowed {
                // Log a submission attempt, because we will try it below.
                // It is done here to avoid fetching the queue again.
                limiter.on_submission_attempt();
            } else {
                log::debug!("Submit attempt was rate limited: {status:?}");
            }
            allowed
        }
    };

    if try_to_submit {
        queue_try_submit(id, autoalloc, state_ref, events, new_job_id).await;
    }
    try_pause_queue(autoalloc, id);
}

fn get_data_from_worker<'a>(
    state: &'a mut AutoAllocState,
    manager_info: &ManagerInfo,
) -> Option<(&'a mut AllocationQueue, QueueId, AllocationId)> {
    let allocation_id = &manager_info.allocation_id;
    state
        .get_queue_id_by_allocation(allocation_id)
        .and_then(|queue_id| {
            state
                .get_queue_mut(queue_id)
                .map(|queue| (queue, queue_id, allocation_id.clone()))
        })
}

/// Synchronize the state of allocations with the external job manager.
async fn refresh_queue_allocations(
    events: &EventStreamer,
    autoalloc: &mut AutoAllocState,
    queue_id: QueueId,
) {
    log::debug!("Attempt to refresh allocations of queue {queue_id}");
    let queue = get_or_return!(autoalloc.get_queue_mut(queue_id));
    if !queue.limiter().can_perform_status_check() {
        log::debug!("Refresh attempt was rate limited");
        return;
    }

    let (status_fut, allocation_ids) = {
        let allocations: Vec<_> = queue.active_allocations().collect();
        if allocations.is_empty() {
            return;
        }

        let fut = queue.handler().get_status_of_allocations(&allocations);
        let allocation_ids: Vec<AllocationId> = allocations
            .into_iter()
            .map(|alloc| alloc.id.clone())
            .collect();
        (fut, allocation_ids)
    };

    queue.limiter_mut().on_status_attempt();
    let result = status_fut.await;

    log::debug!("Allocations of queue {queue_id} have been refreshed: {result:?}");

    match result {
        Ok(mut status_map) => {
            let queue = get_or_return!(autoalloc.get_queue_mut(queue_id));
            log::debug!(
                "Active allocations of queue {queue_id} before update\n{:?}",
                queue.active_allocations().collect::<Vec<_>>()
            );
            for allocation_id in allocation_ids {
                let status = status_map.remove(&allocation_id).unwrap_or_else(|| {
                    Ok(AllocationExternalStatus::Failed {
                        started_at: None,
                        finished_at: SystemTime::now(),
                    })
                });
                match status {
                    Ok(status) => {
                        sync_allocation_status(
                            events,
                            queue_id,
                            queue,
                            &allocation_id,
                            AllocationSyncReason::AllocationExternalChange(status),
                        );
                    }
                    Err(error) => {
                        log::warn!("Could not get status of allocation {allocation_id}: {error:?}");

                        // We failed to get a status of an allocation.
                        // This can either be a transient error, or it can mean that the manager
                        // will never give us a valid status ever again.
                        // To avoid the latter situation, we count the number of status errors that
                        // we saw, and if it reaches a threshold, we finish the allocation to let
                        // autoalloc submit new allocations instead.
                        if let Some(allocation) = queue.get_allocation_mut(&allocation_id) {
                            increase_status_error_counter(events, allocation, queue_id);
                        }
                    }
                }
            }
            log::debug!(
                "Active allocations of queue {queue_id} after update\n{:?}",
                queue.active_allocations().collect::<Vec<_>>()
            );
        }
        Err(error) => {
            log::error!("Failed to get allocations status from queue {queue_id}: {error:?}");
            // We could not even communicate with the allocation manager.
            // Pessimistically bump the status error counter for all active allocations of the queue
            for allocation in queue.active_allocations_mut() {
                increase_status_error_counter(events, allocation, queue_id);
            }
        }
    }
}

fn increase_status_error_counter(
    events: &EventStreamer,
    allocation: &mut Allocation,
    queue_id: QueueId,
) {
    match &mut allocation.status {
        AllocationState::Queued { status_error_count } => {
            *status_error_count += 1;
            if *status_error_count > MAX_QUEUED_STATUS_ERROR_COUNT {
                log::debug!("Queued allocation {} received too many status errors. It is now considered to be finished",
                allocation.id);
                allocation.status = AllocationState::FinishedUnexpectedly {
                    connected_workers: Default::default(),
                    disconnected_workers: Default::default(),
                    started_at: None,
                    finished_at: SystemTime::now(),
                    // The allocation did not receive any workers, and its
                    // status check has repeatedly failed, so most likely
                    // it has failed.
                    failed: true,
                };
                events.on_allocation_finished(queue_id, allocation.id.clone());
            }
        }
        AllocationState::Running {
            status_error_count,
            started_at,
            connected_workers,
            disconnected_workers,
        } => {
            *status_error_count += 1;
            if *status_error_count > MAX_RUNNING_STATUS_ERROR_COUNT {
                log::debug!("Running allocation {} received too many status errors. It is now considered to be finished", allocation.id);
                allocation.status = AllocationState::FinishedUnexpectedly {
                    connected_workers: std::mem::take(connected_workers),
                    disconnected_workers: std::mem::take(disconnected_workers),
                    started_at: Some(*started_at),
                    finished_at: SystemTime::now(),
                    // Hard to say what has happened, but since the
                    // allocation has at least started, consider it to be a
                    // success.
                    failed: false,
                };
                events.on_allocation_finished(queue_id, allocation.id.clone());
            }
        }
        AllocationState::Finished { .. } | AllocationState::FinishedUnexpectedly { .. } => {}
    }
}

enum AllocationSyncReason {
    WorkedConnected(WorkerId),
    WorkerLost(WorkerId, LostWorkerDetails),
    AllocationExternalChange(AllocationExternalStatus),
}

/// Update the status of an allocation once an interesting event (`sync_reason`) has happened.
fn sync_allocation_status(
    events: &EventStreamer,
    queue_id: QueueId,
    queue: &mut AllocationQueue,
    allocation_id: &str,
    sync_reason: AllocationSyncReason,
) {
    let allocation = get_or_return!(queue.get_allocation_mut(allocation_id));

    enum AllocationFinished {
        Failure,
        Success,
    }

    let finished: Option<AllocationFinished> = {
        match sync_reason {
            AllocationSyncReason::WorkedConnected(worker_id) => match &mut allocation.status {
                AllocationState::Queued { .. } => {
                    allocation.status = AllocationState::Running {
                        connected_workers: Set::from_iter([worker_id]),
                        disconnected_workers: Default::default(),
                        started_at: SystemTime::now(),
                        status_error_count: 0,
                    };
                    events.on_allocation_started(queue_id, allocation_id.to_string());
                    None
                }
                AllocationState::Running {
                    connected_workers, ..
                } => {
                    if allocation.target_worker_count == connected_workers.len() as u64 {
                        log::warn!("Allocation {allocation_id} already has the expected number of workers, worker {worker_id} is not expected");
                    }
                    if !connected_workers.insert(worker_id) {
                        log::warn!(
                            "Allocation {allocation_id} already had worker {worker_id} connected"
                        );
                    }
                    None
                }
                AllocationState::Finished { .. } | AllocationState::FinishedUnexpectedly { .. } => {
                    log::warn!(
                            "Allocation {allocation_id} has status {:?} and does not expect new workers",
                            allocation.status
                        );
                    None
                }
            },
            AllocationSyncReason::WorkerLost(worker_id, details) => {
                match &mut allocation.status {
                    AllocationState::Queued {
                        status_error_count: _,
                    } => {
                        log::warn!(
                            "Worker {worker_id} has disconnected before its allocation was registered as running!"
                        );
                        None
                    }
                    AllocationState::Running {
                        disconnected_workers,
                        connected_workers,
                        started_at,
                        status_error_count: _,
                    } => {
                        if !connected_workers.remove(&worker_id) {
                            log::warn!("Worker {worker_id} has disconnected multiple times!");
                        }
                        disconnected_workers.add_lost_worker(worker_id, details);

                        // All expected workers have disconnected, the allocation ends in a normal way
                        if disconnected_workers.count() == allocation.target_worker_count {
                            let is_failed = disconnected_workers.all_crashed();

                            allocation.status = AllocationState::Finished {
                                started_at: *started_at,
                                finished_at: SystemTime::now(),
                                disconnected_workers: std::mem::take(disconnected_workers),
                            };

                            if is_failed {
                                Some(AllocationFinished::Failure)
                            } else {
                                Some(AllocationFinished::Success)
                            }
                        } else {
                            None
                        }
                    }
                    AllocationState::Finished { .. } => {
                        log::warn!(
                            "Worker {worker_id} has disconnected from an already finished allocation {}",
                            allocation.id
                        );
                        None
                    }
                    AllocationState::FinishedUnexpectedly { .. } => {
                        log::warn!(
                            "Worker {worker_id} has disconnected from an invalid allocation {}",
                            allocation.id
                        );
                        None
                    }
                }
            }
            AllocationSyncReason::AllocationExternalChange(status) => {
                match (&mut allocation.status, status) {
                    (AllocationState::Queued { .. }, AllocationExternalStatus::Queued)
                    | (AllocationState::Running { .. }, AllocationExternalStatus::Running) => {
                        // No change
                        None
                    }
                    (AllocationState::Queued { .. }, AllocationExternalStatus::Running) => {
                        // The allocation has started running. Usually, we will receive
                        // information about this by a worker being connected, but in theory this
                        // event can happen sooner (or the allocation can start, but the worker
                        // fails to start).
                        allocation.status = AllocationState::Running {
                            started_at: SystemTime::now(),
                            connected_workers: Default::default(),
                            disconnected_workers: Default::default(),
                            status_error_count: 0,
                        };
                        None
                    }
                    (
                        AllocationState::Queued { .. },
                        ref status @ (AllocationExternalStatus::Finished {
                            finished_at,
                            started_at,
                        }
                        | AllocationExternalStatus::Failed {
                            finished_at,
                            started_at,
                        }),
                    ) => {
                        // The allocation has finished without any worker connecting.
                        // This is an unexpected situation.
                        let failed = matches!(status, AllocationExternalStatus::Failed { .. });

                        allocation.status = AllocationState::FinishedUnexpectedly {
                            connected_workers: Default::default(),
                            disconnected_workers: Default::default(),
                            started_at,
                            finished_at,
                            failed,
                        };

                        Some(if failed {
                            AllocationFinished::Failure
                        } else {
                            AllocationFinished::Success
                        })
                    }
                    (
                        AllocationState::Running {
                            disconnected_workers,
                            started_at: _,
                            connected_workers,
                            status_error_count: _,
                        },
                        ref status @ (AllocationExternalStatus::Finished {
                            started_at,
                            finished_at,
                        }
                        | AllocationExternalStatus::Failed {
                            started_at,
                            finished_at,
                        }),
                    ) => {
                        // The allocation has finished without the last worker disconnecting.
                        // This usually shouldn't happen, but we still try to deal with it.
                        let failed = matches!(status, AllocationExternalStatus::Failed { .. });

                        allocation.status = AllocationState::FinishedUnexpectedly {
                            connected_workers: std::mem::take(connected_workers),
                            disconnected_workers: std::mem::take(disconnected_workers),
                            started_at,
                            finished_at,
                            failed,
                        };

                        Some(if failed {
                            AllocationFinished::Failure
                        } else {
                            AllocationFinished::Success
                        })
                    }
                    (
                        AllocationState::Finished { .. }
                        | AllocationState::FinishedUnexpectedly { .. },
                        status,
                    ) => {
                        // The allocation was already finished before, we do not expect any more
                        // status updates.
                        log::debug!("Unexpected external allocation {allocation_id} status {status:?}, the allocation was already finished before.");
                        None
                    }
                    (AllocationState::Running { .. }, AllocationExternalStatus::Queued) => {
                        // A nonsensical transform that should not ever happen. But the allocation
                        // manager can have weird caching, so we at least log something.
                        log::debug!("Unexpected external allocation {allocation_id} status queued, the allocation was already running.");
                        None
                    }
                }
            }
        }
    };

    match finished {
        Some(AllocationFinished::Success) => {
            log::debug!("Marking allocation success");
            queue.limiter_mut().on_allocation_success();
            events.on_allocation_finished(queue_id, allocation_id.to_string());
        }
        Some(AllocationFinished::Failure) => {
            log::debug!("Marking allocation failure");
            queue.limiter_mut().on_allocation_fail();
            events.on_allocation_finished(queue_id, allocation_id.to_string());
        }
        None => {}
    }
}

async fn queue_try_submit(
    queue_id: QueueId,
    autoalloc: &mut AutoAllocState,
    state_ref: &StateRef,
    events: &EventStreamer,
    new_job_id: Option<JobId>,
) {
    let (max_allocs_to_spawn, workers_per_alloc, mut task_state, mut max_workers_to_spawn) = {
        let queue = get_or_return!(autoalloc.get_queue(queue_id));

        let allocs_in_queue = queue.queued_allocations().count();

        let info = queue.info();
        let task_state = get_server_task_state(&state_ref.get(), info);
        let active_workers = count_active_workers(queue);
        let max_workers_to_spawn = match info.max_worker_count() {
            Some(max) => (max as u64).saturating_sub(active_workers),
            None => u64::MAX,
        };

        let max_allocs_to_spawn = info.backlog().saturating_sub(allocs_in_queue as u32);

        log::debug!(
            "Queue {queue_id} state: {allocs_in_queue} queued allocation(s), {active_workers} active worker(s), {max_workers_to_spawn} max. worker(s) to spawn, {max_allocs_to_spawn} allocation(s) to spawn"
        );

        (
            max_allocs_to_spawn,
            info.workers_per_alloc() as u64,
            task_state,
            max_workers_to_spawn,
        )
    };

    // A new job has arrived, which will necessarily have tasks in the waiting state.
    // To avoid creating needless allocations for it, don't do anything if we already have worker(s)
    // that can handle this job.
    if let Some(job_id) = new_job_id {
        if task_state.jobs.contains_key(&job_id) {
            let state = state_ref.get();
            if let Some(job) = state.get_job(job_id) {
                let has_worker = state
                    .get_workers()
                    .values()
                    .any(|worker| can_worker_execute_job(job, worker));
                if has_worker {
                    task_state.jobs.remove(&job_id);
                }
            }
        }
    }

    log::debug!("Task state: {task_state:?}");

    for _ in 0..max_allocs_to_spawn {
        // If there are no more waiting tasks, stop creating allocations
        // Assume that each worker will handle at least a single task
        if task_state.waiting_tasks() == 0 {
            log::debug!("No more waiting tasks found, no new allocations will be created");
            break;
        }
        // If the worker limit was reached, stop creating new allocations
        if max_workers_to_spawn == 0 {
            log::debug!("Worker limit reached, no new allocations will be created");
            break;
        }

        let workers_to_spawn = std::cmp::min(workers_per_alloc, max_workers_to_spawn);
        let schedule_fut = {
            let queue = get_or_return!(autoalloc.get_queue_mut(queue_id));
            let info = queue.info().clone();
            queue.handler_mut().submit_allocation(
                queue_id,
                &info,
                workers_to_spawn,
                SubmitMode::Submit,
            )
        };

        let result = schedule_fut.await;

        match result {
            Ok(submission_result) => {
                let working_dir = submission_result.working_dir().to_path_buf();
                match submission_result.into_id() {
                    Ok(allocation_id) => {
                        log::info!("Queued {workers_to_spawn} worker(s) into queue {queue_id}: allocation ID {allocation_id}");
                        events.on_allocation_queued(
                            queue_id,
                            allocation_id.clone(),
                            workers_to_spawn,
                        );
                        let allocation =
                            Allocation::new(allocation_id, workers_to_spawn, working_dir);
                        autoalloc.add_allocation(allocation, queue_id);
                        let queue = get_or_return!(autoalloc.get_queue_mut(queue_id));
                        queue.limiter_mut().on_submission_success();

                        task_state.remove_waiting_tasks(workers_to_spawn);
                        max_workers_to_spawn =
                            max_workers_to_spawn.saturating_sub(workers_to_spawn);
                    }
                    Err(err) => {
                        log::error!("Failed to submit allocation into queue {queue_id}: {err:?}");
                        autoalloc.add_inactive_directory(working_dir);
                        let queue = get_or_return!(autoalloc.get_queue_mut(queue_id));
                        queue.limiter_mut().on_submission_fail();
                        break;
                    }
                }
            }
            Err(err) => {
                log::error!("Failed to create allocation directory for queue {queue_id}: {err:?}");
                let queue = get_or_return!(autoalloc.get_queue_mut(queue_id));
                queue.limiter_mut().on_submission_fail();
                break;
            }
        }
    }
}

async fn remove_inactive_directories(autoalloc: &mut AutoAllocState) {
    let to_remove = autoalloc.get_directories_for_removal();
    let futures = to_remove.into_iter().map(|dir| async move {
        let result = tokio::fs::remove_dir_all(&dir).await;
        (result, dir)
    });
    for (result, directory) in join_all(futures).await {
        if let Err(err) = result {
            log::error!("Failed to remove stale allocation directory {directory:?}: {err:?}");
        }
    }
}

fn try_pause_queue(autoalloc: &mut AutoAllocState, id: QueueId) {
    let queue = get_or_return!(autoalloc.get_queue_mut(id));
    if !queue.state().is_running() {
        return;
    }
    let limiter = queue.limiter();

    let status = limiter.submission_status();
    match status {
        RateLimiterStatus::TooManyFailedSubmissions => {
            log::error!("The queue {id} had too many failed submissions, it will be paused.");
            queue.pause();
        }
        RateLimiterStatus::TooManyFailedAllocations => {
            log::error!("The queue {id} had too many failed allocations, it will be paused.");
            queue.pause();
        }
        RateLimiterStatus::Ok | RateLimiterStatus::Wait => (),
    }
}

pub fn prepare_queue_cleanup(
    state: &AllocationQueue,
) -> Vec<impl Future<Output = (AutoAllocResult<()>, AllocationId)> + use<>> {
    let handler = state.handler();
    state
        .active_allocations()
        .map(move |alloc| {
            let allocation_id = alloc.id.clone();
            let future = handler.remove_allocation(alloc);
            async move { (future.await, allocation_id) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::anyhow;
    use derive_builder::Builder;
    use smallvec::smallvec;
    use tako::gateway::{
        LostWorkerReason, ResourceRequest, ResourceRequestEntry, ResourceRequestVariants,
    };
    use tako::program::ProgramDefinition;
    use tako::resources::{AllocationRequest, TimeRequest, CPU_RESOURCE_NAME};
    use tako::WorkerId;
    use tako::{Map, Set, WrappedRcRefCell};
    use tempfile::TempDir;

    use crate::common::arraydef::IntArray;
    use crate::common::manager::info::ManagerType;
    use crate::common::utils::time::mock_time::MockTime;
    use crate::server::autoalloc::process::{
        queue_try_submit, refresh_state, sync_allocation_status, AllocationSyncReason,
        RefreshReason,
    };
    use crate::server::autoalloc::queue::{
        AllocationExternalStatus, AllocationStatusMap, AllocationSubmissionResult, QueueHandler,
        SubmitMode,
    };
    use crate::server::autoalloc::state::{
        AllocationQueue, AllocationQueueState, AllocationState, AutoAllocState, RateLimiter,
    };
    use crate::server::autoalloc::{
        Allocation, AllocationId, AutoAllocResult, LostWorkerDetails, QueueId, QueueInfo,
    };
    use crate::server::event::streamer::EventStreamer;
    use crate::server::job::Job;
    use crate::server::state::{State, StateRef};
    use crate::tests::utils::create_hq_state;
    use crate::transfer::messages::{
        JobDescription, JobSubmitDescription, JobTaskDescription, PinMode, TaskDescription,
        TaskKind, TaskKindProgram,
    };
    use tako::resources::ResourceAmount;

    #[tokio::test]
    async fn fill_backlog() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(4).workers_per_alloc(2),
        );

        queue_try_submit(
            queue_id,
            &mut state,
            &hq_state,
            &EventStreamer::new(None),
            None,
        )
        .await;

        let allocations = get_allocations(&state, queue_id);
        assert_eq!(allocations.len(), 4);
        assert!(allocations
            .iter()
            .all(|alloc| alloc.target_worker_count == 2));
    }

    #[tokio::test]
    async fn do_nothing_on_full_backlog() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(&mut state, handler, QueueBuilder::default().backlog(4));

        for _ in 0..5 {
            queue_try_submit(
                queue_id,
                &mut state,
                &hq_state,
                &EventStreamer::new(None),
                None,
            )
            .await;
        }

        assert_eq!(get_allocations(&state, queue_id).len(), 4);
    }

    #[tokio::test]
    async fn worker_connects_from_unknown_allocation() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(1),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        on_worker_added(&s, queue_id, &mut state, "foo", 0);

        assert!(get_allocations(&state, queue_id)
            .iter()
            .all(|alloc| !alloc.is_running()));
    }

    #[tokio::test]
    async fn start_allocation_when_worker_connects() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(1),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocs = get_allocations(&state, queue_id);

        on_worker_added(&s, queue_id, &mut state, &allocs[0].id, 10);
        check_running_workers(get_allocations(&state, queue_id).first().unwrap(), vec![10]);
    }

    #[tokio::test]
    async fn add_another_worker_to_allocation() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(2),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocs = get_allocations(&state, queue_id);

        for id in [0u32, 1] {
            on_worker_added(&s, queue_id, &mut state, &allocs[0].id, id.into());
        }
        check_running_workers(
            get_allocations(&state, queue_id).first().unwrap(),
            vec![0, 1],
        );
    }

    #[tokio::test]
    async fn finish_allocation_when_worker_disconnects() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(1),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocs = get_allocations(&state, queue_id);

        on_worker_added(&s, queue_id, &mut state, &allocs[0].id, 0);
        on_worker_lost(
            &s,
            queue_id,
            &mut state,
            &allocs[0].id,
            0,
            lost_worker_normal(LostWorkerReason::ConnectionLost),
        );
        check_finished_workers(
            get_allocations(&state, queue_id).first().unwrap(),
            vec![(0, LostWorkerReason::ConnectionLost)],
        );
    }

    #[tokio::test]
    async fn finish_allocation_when_last_worker_disconnects() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(2),
        );

        let s = EventStreamer::new(None);

        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocs = get_allocations(&state, queue_id);

        for id in [0u32, 1] {
            on_worker_added(&s, queue_id, &mut state, &allocs[0].id, id);
        }
        on_worker_lost(
            &s,
            queue_id,
            &mut state,
            &allocs[0].id,
            0,
            lost_worker_normal(LostWorkerReason::ConnectionLost),
        );
        let allocation = get_allocations(&state, queue_id)[0].clone();
        check_running_workers(&allocation, vec![1]);
        check_disconnected_running_workers(
            &allocation,
            vec![(0, LostWorkerReason::ConnectionLost)],
        );
        on_worker_lost(
            &s,
            queue_id,
            &mut state,
            &allocs[0].id,
            1,
            lost_worker_normal(LostWorkerReason::HeartbeatLost),
        );
        let allocation = get_allocations(&state, queue_id)[0].clone();
        check_finished_workers(
            &allocation,
            vec![
                (0, LostWorkerReason::ConnectionLost),
                (1, LostWorkerReason::HeartbeatLost),
            ],
        );
    }

    #[tokio::test]
    async fn do_not_create_allocations_without_tasks() {
        let hq_state = new_hq_state(0);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(&mut state, handler, QueueBuilder::default().backlog(3));

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(get_allocations(&state, queue_id).len(), 0);
    }

    #[tokio::test]
    async fn do_not_fill_backlog_when_tasks_run_out() {
        let hq_state = new_hq_state(5);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(5).workers_per_alloc(2),
        );

        // 5 tasks, 3 * 2 workers -> last two allocations should be ignored
        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(get_allocations(&state, queue_id).len(), 3);
    }

    #[tokio::test]
    async fn stop_allocating_on_error() {
        let hq_state = new_hq_state(5);
        let mut state = AutoAllocState::new(1);

        let handler_state = WrappedRcRefCell::wrap(HandlerState::default());
        let handler = stateful_handler(handler_state.clone());
        let queue_id = add_queue(&mut state, handler, QueueBuilder::default().backlog(5));

        handler_state.get_mut().allocation_will_fail = true;

        // Only try the first allocation in the backlog
        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(handler_state.get().allocation_attempts, 1);

        handler_state.get_mut().allocation_will_fail = false;

        // Finish the rest
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(handler_state.get().allocation_attempts, 6);
    }

    #[tokio::test]
    async fn ignore_task_with_high_time_request() {
        let hq_state = new_hq_state(0);
        attach_job(&mut hq_state.get_mut(), 0, 1, Duration::from_secs(60 * 60));
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().timelimit(Duration::from_secs(60 * 30)),
        );

        // Allocations last for 30 minutes, but job requires 60 minutes
        // Nothing should be scheduled
        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(get_allocations(&state, queue_id).len(), 0);
    }

    #[tokio::test]
    async fn respect_max_worker_count() {
        let hq_state = new_hq_state(100);
        let mut state = AutoAllocState::new(1);

        let handler_state = WrappedRcRefCell::wrap(HandlerState::default());
        let handler = stateful_handler(handler_state.clone());

        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(4).max_worker_count(Some(5)),
        );

        let s = EventStreamer::new(None);
        // Put 4 allocations into the queue.
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocations = get_allocations(&state, queue_id);
        assert_eq!(allocations.len(), 4);

        // Start 2 allocations
        on_worker_added(&s, queue_id, &mut state, &allocations[0].id, 0);
        on_worker_added(&s, queue_id, &mut state, &allocations[1].id, 1);

        // Create only one additional allocation
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(get_allocations(&state, queue_id).len(), 5);

        // Finish one allocation
        on_worker_lost(
            &s,
            queue_id,
            &mut state,
            &allocations[1].id,
            1,
            lost_worker_normal(LostWorkerReason::ConnectionLost),
        );

        // One worker was freed, create an additional allocation
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        assert_eq!(get_allocations(&state, queue_id).len(), 6);
    }

    #[tokio::test]
    async fn max_worker_count_shorten_last_allocation() {
        let hq_state = new_hq_state(100);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();

        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default()
                .backlog(2)
                .workers_per_alloc(4)
                .max_worker_count(Some(6)),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocations = get_allocations(&state, queue_id);
        assert_eq!(allocations.len(), 2);
        assert_eq!(allocations[0].target_worker_count, 4);
        assert_eq!(allocations[1].target_worker_count, 2);
    }

    #[tokio::test]
    async fn delete_stale_directories_of_unsubmitted_allocations() {
        let hq_state = new_hq_state(100);
        let mut state = AutoAllocState::new(1);

        let make_dir = || {
            let tempdir = TempDir::with_prefix("hq").unwrap();
            tempdir.into_path()
        };

        let handler = fails_submit_handler();

        let max_kept = 2;
        state.set_max_kept_directories(max_kept);
        add_queue(&mut state, handler, QueueBuilder::default());

        let dirs = [make_dir(), make_dir()];
        state.set_inactive_allocation_directories(dirs.iter().cloned().collect());

        let s = EventStreamer::new(None);
        // Delete oldest directory
        refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
        assert!(!dirs[0].exists());
        assert!(dirs[1].exists());

        // Delete second oldest directory
        refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
        assert!(!dirs[1].exists());
    }

    #[tokio::test]
    async fn pause_queue_when_submission_fails_too_many_times() {
        let hq_state = new_hq_state(100);
        let mut state = AutoAllocState::new(1);

        let shared = WrappedRcRefCell::wrap(HandlerState::default());
        let handler = stateful_handler(shared.clone());

        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default()
                .backlog(5)
                .limiter_max_submit_fails(2),
        );

        shared.get_mut().allocation_will_fail = true;

        let s = EventStreamer::new(None);
        refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
        check_queue_exists(&state, queue_id);
        refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
        check_queue_paused(&state, queue_id);
    }

    #[tokio::test]
    async fn pause_queue_when_allocation_fails_too_many_times() {
        let hq_state = new_hq_state(100);
        let mut state = AutoAllocState::new(1);

        let shared = WrappedRcRefCell::wrap(HandlerState::default());
        let handler = stateful_handler(shared.clone());

        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default()
                .backlog(5)
                .limiter_max_alloc_fails(2),
        );
        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;

        let allocations = get_allocations(&state, queue_id);
        fail_allocation(queue_id, &mut state, &allocations[0].id);
        refresh_state(
            &hq_state,
            &s,
            &mut state,
            RefreshReason::UpdateQueue(queue_id),
        )
        .await;
        check_queue_exists(&state, queue_id);

        fail_allocation(queue_id, &mut state, &allocations[1].id);
        refresh_state(
            &hq_state,
            &s,
            &mut state,
            RefreshReason::UpdateQueue(queue_id),
        )
        .await;
        check_queue_paused(&state, queue_id);
    }

    #[tokio::test]
    async fn respect_rate_limiter() {
        let hq_state = new_hq_state(100);
        let mut state = AutoAllocState::new(1);

        let shared = WrappedRcRefCell::wrap(HandlerState::default());
        let handler = stateful_handler(shared.clone());

        add_queue(
            &mut state,
            handler,
            QueueBuilder::default()
                .backlog(5)
                .limiter_max_alloc_fails(100)
                .limiter_delays(vec![
                    Duration::ZERO,
                    Duration::from_secs(1),
                    Duration::from_secs(10),
                ]),
        );

        shared.get_mut().allocation_will_fail = true;

        let check_alloc_count = |count: usize| {
            assert_eq!(shared.get().allocation_attempts, count);
        };
        let s = EventStreamer::new(None);
        let mut now = Instant::now();
        {
            let _mock = MockTime::mock(now);
            refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
            check_alloc_count(1);
        }

        // Now we should sleep until +1 sec
        {
            now += Duration::from_millis(500);
            let _mock = MockTime::mock(now);
            refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
            check_alloc_count(1);
        }
        {
            now += Duration::from_millis(1500);
            let _mock = MockTime::mock(now);
            refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
            check_alloc_count(2);
        }

        // Now we should sleep until +10 sec
        {
            now += Duration::from_millis(5000);
            let _mock = MockTime::mock(now);
            refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
            check_alloc_count(2);
        }
        {
            now += Duration::from_millis(6000);
            let _mock = MockTime::mock(now);
            refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
            check_alloc_count(3);
        }
        // The delay shouldn't increase any more when we have reached the maximum delay
        {
            now += Duration::from_millis(11000);
            let _mock = MockTime::mock(now);
            refresh_state(&hq_state, &s, &mut state, RefreshReason::UpdateAllQueues).await;
            check_alloc_count(4);
        }
    }

    #[tokio::test]
    async fn bump_fail_counter_if_worker_fails_quick() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(1),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocs = get_allocations(&state, queue_id);

        fail_allocation(queue_id, &mut state, &allocs[0].id);
        assert_eq!(
            state
                .get_queue(queue_id)
                .unwrap()
                .limiter()
                .allocation_fail_count(),
            1
        );
    }

    #[tokio::test]
    async fn do_not_bump_fail_counter_if_worker_fails_normally() {
        let hq_state = new_hq_state(1000);
        let mut state = AutoAllocState::new(1);

        let handler = always_queued_handler();
        let queue_id = add_queue(
            &mut state,
            handler,
            QueueBuilder::default().backlog(1).workers_per_alloc(1),
        );

        let s = EventStreamer::new(None);
        queue_try_submit(queue_id, &mut state, &hq_state, &s, None).await;
        let allocs = get_allocations(&state, queue_id);

        on_worker_added(&s, queue_id, &mut state, &allocs[0].id, 0);
        on_worker_lost(
            &s,
            queue_id,
            &mut state,
            &allocs[0].id,
            0,
            lost_worker_normal(LostWorkerReason::ConnectionLost),
        );
        assert_eq!(
            state
                .get_queue(queue_id)
                .unwrap()
                .limiter()
                .allocation_fail_count(),
            0
        );
    }

    // Utilities
    struct Handler<ScheduleFn, StatusFn, RemoveFn, State> {
        schedule_fn: WrappedRcRefCell<ScheduleFn>,
        status_fn: WrappedRcRefCell<StatusFn>,
        remove_fn: WrappedRcRefCell<RemoveFn>,
        custom_state: WrappedRcRefCell<State>,
    }

    impl<
            State: 'static,
            ScheduleFn: 'static + Fn(WrappedRcRefCell<State>, u64) -> ScheduleFnFut,
            ScheduleFnFut: Future<Output = AutoAllocResult<AllocationSubmissionResult>>,
            StatusFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> StatusFnFut,
            StatusFnFut: Future<Output = AutoAllocResult<Option<AllocationExternalStatus>>>,
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
            ScheduleFnFut: Future<Output = AutoAllocResult<AllocationSubmissionResult>>,
            StatusFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> StatusFnFut,
            StatusFnFut: Future<Output = AutoAllocResult<Option<AllocationExternalStatus>>>,
            RemoveFn: 'static + Fn(WrappedRcRefCell<State>, AllocationId) -> RemoveFnFut,
            RemoveFnFut: Future<Output = AutoAllocResult<()>>,
        > QueueHandler for Handler<ScheduleFn, StatusFn, RemoveFn, State>
    {
        fn submit_allocation(
            &mut self,
            _queue_id: QueueId,
            _queue_info: &QueueInfo,
            worker_count: u64,
            _mode: SubmitMode,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<AllocationSubmissionResult>>>> {
            let schedule_fn = self.schedule_fn.clone();
            let custom_state = self.custom_state.clone();

            Box::pin(async move { (schedule_fn.get())(custom_state.clone(), worker_count).await })
        }

        fn get_status_of_allocations(
            &self,
            allocations: &[&Allocation],
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<AllocationStatusMap>>>> {
            let status_fn = self.status_fn.clone();
            let custom_state = self.custom_state.clone();
            let allocation_ids: Vec<AllocationId> =
                allocations.iter().map(|alloc| alloc.id.clone()).collect();

            Box::pin(async move {
                let mut result = Map::default();
                for allocation_id in allocation_ids {
                    let status =
                        (status_fn.get())(custom_state.clone(), allocation_id.clone()).await;
                    if let Some(status) = status.transpose() {
                        result.insert(allocation_id, status);
                    }
                }
                Ok(result)
            })
        }

        fn remove_allocation(
            &self,
            allocation: &Allocation,
        ) -> Pin<Box<dyn Future<Output = AutoAllocResult<()>>>> {
            let remove_fn = self.remove_fn.clone();
            let custom_state = self.custom_state.clone();
            let allocation_id = allocation.id.clone();

            Box::pin(async move { (remove_fn.get())(custom_state.clone(), allocation_id).await })
        }
    }

    #[derive(Default)]
    struct HandlerState {
        state: Map<AllocationId, AllocationExternalStatus>,
        spawned_allocations: Set<AllocationId>,
        allocation_attempts: usize,
        allocation_will_fail: bool,
        deleted_allocations: Set<AllocationId>,
    }

    // Handlers
    fn always_queued_handler() -> Box<dyn QueueHandler> {
        Handler::new(
            WrappedRcRefCell::wrap(0),
            move |state, _| async move {
                let mut s = state.get_mut();
                let id = *s;
                *s += 1;
                Ok(AllocationSubmissionResult::new(
                    Ok(id.to_string()),
                    Default::default(),
                ))
            },
            move |_, _| async move { Ok(Some(AllocationExternalStatus::Queued)) },
            |_, _| async move { Ok(()) },
        )
    }

    fn fails_submit_handler() -> Box<dyn QueueHandler> {
        Handler::new(
            WrappedRcRefCell::wrap(()),
            move |_, _| async move {
                let tempdir = TempDir::with_prefix("hq").unwrap();
                let dir = tempdir.into_path();

                Ok(AllocationSubmissionResult::new(
                    Err(anyhow::anyhow!("failure")),
                    dir,
                ))
            },
            move |_, _| async move { Ok(Some(AllocationExternalStatus::Queued)) },
            |_, _| async move { Ok(()) },
        )
    }

    fn fails_status_handler() -> Box<dyn QueueHandler> {
        Handler::new(
            WrappedRcRefCell::wrap(0),
            move |state, _| async move {
                let mut s = state.get_mut();
                let id = *s;
                *s += 1;
                Ok(AllocationSubmissionResult::new(
                    Ok(id.to_string()),
                    Default::default(),
                ))
            },
            move |_, alloc_id| async move {
                Err(anyhow::anyhow!(
                    "Cannot get status of allocation {alloc_id}"
                ))
            },
            |_, _| async move { Ok(()) },
        )
    }

    /// Handler that spawns allocations with sequentially increasing IDs (starting at *0*) and
    /// returns allocation statuses from [`HandlerState`].
    fn stateful_handler(state: WrappedRcRefCell<HandlerState>) -> Box<dyn QueueHandler> {
        Handler::new(
            state,
            move |state, _worker_count| async move {
                let mut state = state.get_mut();
                let tempdir = TempDir::with_prefix("hq").unwrap();
                let dir = tempdir.into_path();

                state.allocation_attempts += 1;

                if state.allocation_will_fail {
                    Ok(AllocationSubmissionResult::new(
                        AutoAllocResult::Err(anyhow!("Fail")),
                        dir,
                    ))
                } else {
                    let spawned = &mut state.spawned_allocations;
                    let id = spawned.len();
                    spawned.insert(id.to_string());

                    Ok(AllocationSubmissionResult::new(Ok(id.to_string()), dir))
                }
            },
            move |state, id| async move {
                let state = &mut state.get_mut().state;
                let status = state
                    .get(&id)
                    .cloned()
                    .unwrap_or(AllocationExternalStatus::Queued);
                Ok(Some(status))
            },
            |state, id| async move {
                state.get_mut().deleted_allocations.insert(id);
                Ok(())
            },
        )
    }

    // Queue definitions
    #[derive(Builder)]
    #[builder(pattern = "owned", build_fn(name = "finish"))]
    struct Queue {
        #[builder(default = "ManagerType::Slurm")]
        manager: ManagerType,
        #[builder(default = "1")]
        backlog: u32,
        #[builder(default = "1")]
        workers_per_alloc: u32,
        #[builder(default = "Duration::from_secs(60 * 60)")]
        timelimit: Duration,
        #[builder(default)]
        max_worker_count: Option<u32>,
        #[builder(default = "100")]
        limiter_max_alloc_fails: u64,
        #[builder(default = "100")]
        limiter_max_submit_fails: u64,
        #[builder(default = "vec![Duration::ZERO]")]
        limiter_delays: Vec<Duration>,
    }

    impl QueueBuilder {
        fn build(self) -> (QueueInfo, RateLimiter) {
            let Queue {
                manager,
                backlog,
                workers_per_alloc,
                timelimit,
                max_worker_count,
                limiter_max_alloc_fails,
                limiter_max_submit_fails,
                limiter_delays,
            } = self.finish().unwrap();
            (
                QueueInfo::new(
                    manager,
                    backlog,
                    workers_per_alloc,
                    timelimit,
                    vec![],
                    max_worker_count,
                    vec![],
                    None,
                    None,
                    None,
                ),
                RateLimiter::new(
                    limiter_delays,
                    limiter_max_submit_fails,
                    limiter_max_alloc_fails,
                    Duration::from_secs(1),
                ),
            )
        }
    }

    fn add_queue(
        autoalloc: &mut AutoAllocState,
        handler: Box<dyn QueueHandler>,
        queue_builder: QueueBuilder,
    ) -> QueueId {
        let (queue_info, limiter) = queue_builder.build();
        let queue = AllocationQueue::new(queue_info, None, handler, limiter);
        autoalloc.add_queue(queue, None)
    }

    fn new_hq_state(waiting_tasks: u32) -> StateRef {
        let state = create_hq_state();
        if waiting_tasks > 0 {
            attach_job(
                &mut state.get_mut(),
                0,
                waiting_tasks,
                Duration::from_secs(0),
            );
        }
        state
    }

    fn get_allocations(autoalloc: &AutoAllocState, queue_id: QueueId) -> Vec<Allocation> {
        let mut allocations: Vec<_> = autoalloc
            .get_queue(queue_id)
            .unwrap()
            .all_allocations()
            .cloned()
            .collect();
        allocations.sort_by(|a, b| a.id.cmp(&b.id));
        allocations
    }

    fn create_job(job_id: u32, tasks: u32, min_time: TimeRequest) -> Job {
        let def = ProgramDefinition {
            args: vec![],
            env: Default::default(),
            stdout: Default::default(),
            stderr: Default::default(),
            stdin: vec![],
            cwd: Default::default(),
        };
        let resources = ResourceRequestVariants::new_simple(ResourceRequest {
            n_nodes: 0,
            min_time,
            resources: smallvec![ResourceRequestEntry {
                resource: CPU_RESOURCE_NAME.to_string(),
                policy: AllocationRequest::Compact(ResourceAmount::new_units(1)),
            }],
        });

        let mut job = Job::new(
            job_id.into(),
            JobDescription {
                name: "job".to_string(),
                max_fails: None,
            },
            false,
        );
        job.attach_submit(Arc::new(JobSubmitDescription {
            task_desc: JobTaskDescription::Array {
                ids: IntArray::from_range(0, tasks),
                entries: None,
                task_desc: TaskDescription {
                    kind: TaskKind::ExternalProgram(TaskKindProgram {
                        program: def,
                        pin_mode: PinMode::None,
                        task_dir: false,
                    }),
                    resources,
                    time_limit: None,
                    priority: 0,
                    crash_limit: 5,
                },
            },
            submit_dir: Default::default(),
            stream_path: None,
        }));
        job
    }

    fn attach_job(state: &mut State, job_id: u32, tasks: u32, min_time: TimeRequest) {
        let job = create_job(job_id, tasks, min_time);
        state.add_job(job);
    }

    fn on_worker_added(
        streamer: &EventStreamer,
        queue_id: QueueId,
        state: &mut AutoAllocState,
        allocation_id: &str,
        worker_id: u32,
    ) {
        sync_allocation_status(
            streamer,
            queue_id,
            state.get_queue_mut(queue_id).unwrap(),
            allocation_id,
            AllocationSyncReason::WorkedConnected(worker_id.into()),
        );
    }

    fn on_worker_lost(
        streamer: &EventStreamer,
        queue_id: QueueId,
        state: &mut AutoAllocState,
        allocation_id: &str,
        worker_id: u32,
        details: LostWorkerDetails,
    ) {
        sync_allocation_status(
            streamer,
            queue_id,
            state.get_queue_mut(queue_id).unwrap(),
            allocation_id,
            AllocationSyncReason::WorkerLost(worker_id.into(), details),
        );
    }

    fn check_running_workers(allocation: &Allocation, workers: Vec<u32>) {
        let workers: Vec<WorkerId> = workers.into_iter().map(WorkerId::from).collect();
        match &allocation.status {
            AllocationState::Running {
                connected_workers, ..
            } => {
                let mut connected: Vec<WorkerId> = connected_workers.iter().cloned().collect();
                connected.sort_unstable();
                assert_eq!(connected, workers);
            }
            _ => panic!("Allocation should be in running status"),
        }
    }

    fn check_disconnected_running_workers(
        allocation: &Allocation,
        workers: Vec<(u32, LostWorkerReason)>,
    ) {
        let workers: Vec<(WorkerId, LostWorkerReason)> = workers
            .into_iter()
            .map(|(id, reason)| (WorkerId::from(id), reason))
            .collect();
        match &allocation.status {
            AllocationState::Running {
                disconnected_workers,
                ..
            } => {
                let mut disconnected: Vec<_> = disconnected_workers
                    .clone()
                    .into_iter()
                    .map(|(id, details)| (id, details.reason))
                    .collect();
                disconnected.sort_unstable_by_key(|(id, _)| *id);
                assert_eq!(disconnected, workers);
            }
            _ => panic!("Allocation should be in running status"),
        }
    }

    fn check_finished_workers(allocation: &Allocation, workers: Vec<(u32, LostWorkerReason)>) {
        let workers: Vec<(WorkerId, LostWorkerReason)> = workers
            .into_iter()
            .map(|(id, reason)| (WorkerId::from(id), reason))
            .collect();
        match &allocation.status {
            AllocationState::Finished {
                disconnected_workers,
                ..
            } => {
                let mut disconnected: Vec<_> = disconnected_workers
                    .clone()
                    .into_iter()
                    .map(|(id, details)| (id, details.reason))
                    .collect();
                disconnected.sort_unstable_by_key(|(id, _)| *id);
                assert_eq!(disconnected, workers);
            }
            _ => panic!("Allocation should be in finished status"),
        }
    }

    fn check_queue_exists(autoalloc: &AutoAllocState, id: QueueId) {
        assert!(autoalloc.get_queue(id).is_some());
    }
    fn check_queue_paused(autoalloc: &AutoAllocState, id: QueueId) {
        assert!(matches!(
            autoalloc.get_queue(id).unwrap().state(),
            AllocationQueueState::Paused
        ));
    }

    fn fail_allocation(queue_id: QueueId, autoalloc: &mut AutoAllocState, allocation_id: &str) {
        let s = EventStreamer::new(None);
        on_worker_added(&s, queue_id, autoalloc, &allocation_id, 0);
        on_worker_lost(
            &s,
            queue_id,
            autoalloc,
            &allocation_id,
            0,
            lost_worker_quick(LostWorkerReason::ConnectionLost),
        );
    }

    fn lost_worker_normal(reason: LostWorkerReason) -> LostWorkerDetails {
        LostWorkerDetails {
            reason,
            lifetime: Duration::from_secs(3600),
        }
    }
    fn lost_worker_quick(reason: LostWorkerReason) -> LostWorkerDetails {
        LostWorkerDetails {
            reason,
            lifetime: Duration::from_millis(1),
        }
    }
}
