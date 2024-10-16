use crate::gateway::TaskDataFlags;
use crate::internal::common::resources::Allocation;
use crate::internal::messages::common::TaskFailInfo;
use crate::internal::messages::worker::{FromWorkerMessage, TaskRunningMsg};
use crate::internal::worker::localcomm::{Registration, Token};
use crate::internal::worker::state::{WorkerState, WorkerStateRef};
use crate::internal::worker::task_comm::RunningTaskComm;
use crate::launcher::{TaskBuildContext, TaskFuture, TaskLaunchData, TaskResult};
use crate::TaskId;
use bstr::{BString, ByteSlice};
use futures::future::Either;
use std::rc::Rc;
use tokio::sync::oneshot;

/// Primary entrypoint for starting a task on a worker.
/// The tako task will be started as a separate tokio task spawned onto the currently
/// active Runtime.
pub(crate) fn start_task(
    state: &mut WorkerState,
    state_ref: &WorkerStateRef,
    task_id: TaskId,
    allocation: Rc<Allocation>,
    resource_index: usize,
) {
    log::debug!("Task={} assigned", task_id);

    let (end_sender, end_receiver) = oneshot::channel();
    let task_comm = RunningTaskComm::new(end_sender);

    state.start_task(task_id, task_comm, allocation);

    let task = state.get_task(task_id);
    //let (tasks, lc_state, launcher) = state.borrow_tasks_and_lc_state_and_launcher();
    //let task = tasks.get(&task_id);
    let token = task.need_data_layer().then(|| {
        state
            .lc_state
            .borrow_mut()
            .register_task(Registration::DataConnection { task_id })
    });
    match state.task_launcher.build_task(
        TaskBuildContext {
            task,
            state,
            resource_index,
            token: token.clone(),
        },
        end_receiver,
    ) {
        Ok(task_launch_data) => {
            let TaskLaunchData {
                task_future,
                task_context,
            } = task_launch_data;

            state
                .comm()
                .send_message_to_server(FromWorkerMessage::TaskRunning(TaskRunningMsg {
                    id: task_id,
                    context: task_context,
                }));

            tokio::task::spawn_local(handle_task_future(
                task_future,
                state_ref.clone(),
                task_id,
                token.clone(),
            ));
        }
        Err(error) => {
            log::debug!(
                "Task initialization failed id={}, error={:?}",
                task_id,
                error
            );
            if let Some(token) = token {
                state.lc_state.borrow_mut().unregister_token(&token);
            }
            state.finish_task_failed(task_id, TaskFailInfo::from_string(error.to_string()));
        }
    };
}

/// Polls the task future and makes sure that various situations
/// (like cancellation, errors, timeout) are handled correctly.
async fn handle_task_future(
    task_future: TaskFuture,
    state_ref: WorkerStateRef,
    task_id: TaskId,
    token: Option<Token>,
) {
    let time_limit = {
        let state = state_ref.get();
        if let Some(task) = state.find_task(task_id) {
            task.time_limit
        } else {
            if let Some(token) = token {
                state.lc_state.borrow_mut().unregister_token(&token)
            }
            // Task was removed before spawn took place
            return;
        }
    };

    let result = if let Some(duration) = time_limit {
        let sleep = tokio::time::sleep(duration);
        tokio::pin!(sleep);
        match futures::future::select(task_future, sleep).await {
            Either::Left((r, _)) => r,
            Either::Right((_, task_future)) => {
                {
                    let mut state = state_ref.get_mut();
                    let task = state.get_task_mut(task_id);
                    log::debug!("Task {} timeouted", task.id);
                    // Send a message to the task future that it should terminate
                    // because it has been timeouted. Currently, we trust the
                    // implementation of the task and we don't further timeout
                    // it here.
                    task.task_comm_mut().unwrap().send_timeout_notification();
                }
                task_future.await
            }
        }
    } else {
        task_future.await
    };
    let mut state = state_ref.get_mut();
    if let Some(token) = token {
        state.lc_state.borrow_mut().unregister_token(&token)
    }
    match result {
        Ok(TaskResult::Finished) => {
            log::debug!("Inner task finished id={}", task_id);
            state.finish_task(task_id);
        }
        Ok(TaskResult::Canceled) => {
            log::debug!("Inner task canceled id={}", task_id);
            state.finish_task_cancel(task_id);
        }
        Ok(TaskResult::Timeouted) => {
            log::debug!("Inner task timeouted id={}", task_id);
            state.finish_task_failed(
                task_id,
                TaskFailInfo::from_string("Time limit reached".to_string()),
            );
        }
        Err(e) => {
            log::debug!("Inner task failed id={}, error={:?}", task_id, e);
            state.finish_task_failed(task_id, TaskFailInfo::from_string(e.to_string()));
        }
    }
}
