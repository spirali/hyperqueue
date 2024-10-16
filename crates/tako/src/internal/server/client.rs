use crate::internal::common::resources::{ResourceRequest, ResourceRequestVariants};
use tokio::sync::mpsc::UnboundedSender;

use crate::gateway::{
    CancelTasksResponse, FromGatewayMessage, NewTasksMessage, NewTasksResponse,
    SharedTaskConfiguration, TaskInfo, TaskState, TasksInfoResponse, ToGatewayMessage,
};
use crate::internal::common::resources::request::ResourceRequestEntry;
use crate::internal::messages::worker::ToWorkerMessage;
use crate::internal::scheduler::query::compute_new_worker_query;
use crate::internal::server::comm::{Comm, CommSender, CommSenderRef};
use crate::internal::server::core::{Core, CoreRef};
use crate::internal::server::reactor::{on_cancel_tasks, on_new_tasks};
use crate::internal::server::task::{Task, TaskConfiguration, TaskRuntimeState};
use std::rc::Rc;

fn create_task_configuration(
    core_ref: &mut Core,
    msg: SharedTaskConfiguration,
) -> TaskConfiguration {
    let resources = ResourceRequestVariants::new(
        msg.resources
            .variants
            .into_iter()
            .map(|rq| {
                ResourceRequest::new(
                    rq.n_nodes,
                    rq.min_time,
                    rq.resources
                        .into_iter()
                        .map(|r| {
                            let resource_id = core_ref.get_or_create_resource_id(&r.resource);
                            ResourceRequestEntry {
                                resource_id,
                                request: r.policy,
                            }
                        })
                        .collect(),
                )
            })
            .collect(),
    );

    TaskConfiguration {
        resources,
        time_limit: msg.time_limit,
        user_priority: msg.priority,
        crash_limit: msg.crash_limit,
        data_flags: msg.data_flags,
    }
}

pub(crate) async fn process_client_message(
    core_ref: &CoreRef,
    comm_ref: &CommSenderRef,
    client_sender: &UnboundedSender<ToGatewayMessage>,
    message: FromGatewayMessage,
) -> Option<String> {
    match message {
        FromGatewayMessage::NewTasks(msg) => handle_new_tasks(
            &mut core_ref.get_mut(),
            &mut comm_ref.get_mut(),
            client_sender,
            msg,
        ),
        FromGatewayMessage::ServerInfo => {
            let core = core_ref.get();
            assert!(client_sender
                .send(ToGatewayMessage::ServerInfo(core.get_server_info()))
                .is_ok());
            None
        }
        FromGatewayMessage::GetTaskInfo(request) => {
            log::debug!("Client asked for task info");
            if !request.tasks.is_empty() {
                todo!();
            }
            let core = core_ref.get();
            let task_map = core.task_map();
            let task_infos = task_map
                .task_ids()
                .map(|task_id| {
                    let task = task_map.get_task(task_id);
                    TaskInfo {
                        id: task.id,
                        state: match task.state {
                            TaskRuntimeState::Waiting(_) => TaskState::Waiting,
                            TaskRuntimeState::Assigned(_) => TaskState::Waiting,
                            TaskRuntimeState::Stealing(_, _) => TaskState::Waiting,
                            TaskRuntimeState::Running { .. } => TaskState::Waiting,
                            TaskRuntimeState::RunningMultiNode(_) => TaskState::Waiting,
                            TaskRuntimeState::Finished(_) => TaskState::Finished,
                        },
                    }
                })
                .collect();
            assert!(client_sender
                .send(ToGatewayMessage::TaskInfo(TasksInfoResponse {
                    tasks: task_infos
                }))
                .is_ok());
            None
        }
        FromGatewayMessage::CancelTasks(msg) => {
            log::debug!("Client asked for canceling tasks");
            let mut core = core_ref.get_mut();
            let mut comm = comm_ref.get_mut();
            let (cancelled_tasks, already_finished) =
                on_cancel_tasks(&mut core, &mut *comm, &msg.tasks);
            assert!(client_sender
                .send(ToGatewayMessage::CancelTasksResponse(CancelTasksResponse {
                    cancelled_tasks,
                    already_finished
                }))
                .is_ok());
            None
        }
        FromGatewayMessage::StopWorker(msg) => {
            let mut core = core_ref.get_mut();
            if let Some(ref mut worker) = core.get_worker_mut(msg.worker_id) {
                worker.set_stopping_flag(true);
                let mut comm = comm_ref.get_mut();
                comm.send_worker_message(msg.worker_id, &ToWorkerMessage::Stop);
                assert!(client_sender.send(ToGatewayMessage::WorkerStopped).is_ok());
                None
            } else {
                Some(format!("Worker with id {} not found", msg.worker_id))
            }
        }
        FromGatewayMessage::NewWorkerQuery(msg) => {
            for query in &msg.worker_queries {
                if let Err(e) = query.descriptor.validate() {
                    return Some(format!("Invalid descriptor: {e:?}"));
                }
            }
            let response = if comm_ref.get().get_scheduling_flag() {
                let (sx, rx) = tokio::sync::oneshot::channel();
                comm_ref
                    .get_mut()
                    .add_after_scheduling_callback(Box::new(move |core| {
                        let _ = sx.send(compute_new_worker_query(core, &msg.worker_queries));
                    }));
                rx.await.unwrap()
            } else {
                let core = core_ref.get();
                compute_new_worker_query(&core, &msg.worker_queries)
            };
            assert!(client_sender
                .send(ToGatewayMessage::NewWorkerAllocationQueryResponse(response))
                .is_ok());
            None
        }
    }
}

fn handle_new_tasks(
    core: &mut Core,
    comm: &mut CommSender,
    client_sender: &UnboundedSender<ToGatewayMessage>,
    msg: NewTasksMessage,
) -> Option<String> {
    log::debug!("Client sends {} tasks", msg.tasks.len());
    if msg.tasks.is_empty() {
        return Some("Task submission is empty".to_string());
    }

    let configurations: Vec<_> = msg
        .shared_data
        .into_iter()
        .map(|c| Rc::new(create_task_configuration(core, c)))
        .collect();

    for cfg in &configurations {
        if let Err(e) = cfg.resources.validate() {
            return Some(format!("Invalid task request {e:?}"));
        }
    }

    let mut tasks: Vec<Task> = Vec::with_capacity(msg.tasks.len());
    for task in msg.tasks {
        if core.is_used_task_id(task.id) {
            return Some(format!("Task id={} is already taken", task.id));
        }
        let idx = task.shared_data_index as usize;
        if idx >= configurations.len() {
            return Some(format!("Invalid configuration index {idx}"));
        }
        let conf = &configurations[idx];
        let task = Task::new(
            task.id,
            task.task_deps,
            task.dataobj_deps,
            conf.clone(),
            task.body,
        );
        tasks.push(task);
    }
    if !msg.adjust_instance_id.is_empty() {
        for task in &mut tasks {
            if let Some(instance_id) = msg.adjust_instance_id.get(&task.id) {
                task.instance_id = *instance_id;
            }
        }
    }
    on_new_tasks(core, comm, tasks);

    assert!(client_sender
        .send(ToGatewayMessage::NewTasksResponse(NewTasksResponse {
            n_waiting_for_workers: 0 // TODO
        }))
        .is_ok());
    None
}
