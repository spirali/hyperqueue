use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use futures::{Sink, SinkExt, Stream, StreamExt};
use orion::kdf::SecretKey;
use tako::messages::common::{LauncherDefinition, ProgramDefinition};
use tako::messages::gateway::{
    CancelTasks, FromGatewayMessage, NewTasksMessage, StopWorkerRequest, TaskDef, ToGatewayMessage,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::common::env::create_hq_env;
use crate::server::job::{Job, JobTaskState};
use crate::server::rpc::TakoServer;
use crate::server::state::StateRef;
use crate::transfer::connection::ServerConnection;
use crate::transfer::messages::{
    CancelJobResponse, FromClientMessage, JobInfoResponse, JobType, SubmitRequest, SubmitResponse,
    ToClientMessage, WorkerListResponse,
};
use crate::{JobId, JobTaskCount, JobTaskId, WorkerId};
use bstr::BString;

pub async fn handle_client_connections(
    state_ref: StateRef,
    tako_ref: TakoServer,
    listener: TcpListener,
    end_flag: Rc<Notify>,
    key: Arc<SecretKey>,
) {
    while let Ok((connection, _)) = listener.accept().await {
        let state_ref = state_ref.clone();
        let tako_ref = tako_ref.clone();
        let end_flag = end_flag.clone();
        let key = key.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = handle_client(connection, state_ref, tako_ref, end_flag, key).await {
                log::error!("Client error: {}", e);
            }
        });
    }
}

async fn handle_client(
    socket: TcpStream,
    state_ref: StateRef,
    tako_ref: TakoServer,
    end_flag: Rc<Notify>,
    key: Arc<SecretKey>,
) -> crate::Result<()> {
    log::debug!("New client connection");
    let socket = ServerConnection::accept_client(socket, key).await?;
    let (tx, rx) = socket.split();

    client_rpc_loop(tx, rx, state_ref, tako_ref, end_flag).await;
    log::debug!("Client connection ended");
    Ok(())
}

pub async fn client_rpc_loop<
    Tx: Sink<ToClientMessage> + Unpin,
    Rx: Stream<Item = crate::Result<FromClientMessage>> + Unpin,
>(
    mut tx: Tx,
    mut rx: Rx,
    state_ref: StateRef,
    tako_ref: TakoServer,
    end_flag: Rc<Notify>,
) {
    while let Some(message_result) = rx.next().await {
        match message_result {
            Ok(message) => {
                let response = match message {
                    FromClientMessage::Submit(msg) => {
                        handle_submit(&state_ref, &tako_ref, msg).await
                    }
                    FromClientMessage::JobInfo(msg) => compute_job_info(&state_ref, msg.job_ids),
                    FromClientMessage::Stop => {
                        end_flag.notify_one();
                        break;
                    }
                    FromClientMessage::WorkerList => handle_worker_list(&state_ref).await,
                    FromClientMessage::StopWorker(msg) => {
                        handle_worker_stop(&tako_ref, msg.worker_id).await.unwrap()
                    }
                    FromClientMessage::Cancel(msg) => {
                        handle_job_cancel(&state_ref, &tako_ref, msg.job_id).await
                    }
                    FromClientMessage::JobDetail(msg) => {
                        compute_job_detail(&state_ref, msg.job_id, msg.include_tasks)
                    }
                };
                assert!(tx.send(response).await.is_ok());
            }
            Err(e) => {
                log::error!("Cannot parse client message: {}", e);
                assert!(tx
                    .send(ToClientMessage::Error(format!(
                        "Cannot parse message: {}",
                        e
                    )))
                    .await
                    .is_ok());
                return;
            }
        }
    }
}

async fn handle_worker_stop(
    tako_ref: &TakoServer,
    worker_id: WorkerId,
) -> crate::Result<ToClientMessage> {
    match tako_ref
        .send_message(FromGatewayMessage::StopWorker(StopWorkerRequest {
            worker_id,
        }))
        .await?
    {
        ToGatewayMessage::WorkerStopped => Ok(ToClientMessage::StopWorkerResponse),
        ToGatewayMessage::Error(error) => Ok(ToClientMessage::Error(error.message)),
        msg => panic!("Received invalid response to worker stop: {:?}", msg),
    }
}

fn compute_job_detail(state_ref: &StateRef, job_id: JobId, include_tasks: bool) -> ToClientMessage {
    let state = state_ref.get();
    ToClientMessage::JobDetailResponse(
        state
            .get_job(job_id)
            .map(|j| j.make_job_detail(include_tasks)),
    )
}

fn compute_job_info(state_ref: &StateRef, job_ids: Option<Vec<JobId>>) -> ToClientMessage {
    let state = state_ref.get();
    if let Some(job_ids) = job_ids {
        ToClientMessage::JobInfoResponse(JobInfoResponse {
            jobs: job_ids
                .into_iter()
                .filter_map(|job_id| state.get_job(job_id).map(|j| j.make_job_info()))
                .collect(),
        })
    } else {
        ToClientMessage::JobInfoResponse(JobInfoResponse {
            jobs: state.jobs().map(|j| j.make_job_info()).collect(),
        })
    }
}

async fn handle_job_cancel(
    state_ref: &StateRef,
    tako_ref: &TakoServer,
    job_id: JobId,
) -> ToClientMessage {
    let mut tako_task_ids = Vec::new();
    {
        let state = state_ref.get_mut();
        let n_tasks = match state.get_job(job_id) {
            None => return ToClientMessage::CancelJobResponse(CancelJobResponse::InvalidJob),
            Some(job) => {
                for (tako_id, _task_id, state) in job.iter_task_states() {
                    match state {
                        JobTaskState::Waiting | JobTaskState::Running => {
                            tako_task_ids.push(tako_id)
                        }
                        JobTaskState::Finished
                        | JobTaskState::Failed(_)
                        | JobTaskState::Canceled => { /* Do nothing */ }
                    }
                }
                job.n_tasks()
            }
        };
        if tako_task_ids.is_empty() {
            return ToClientMessage::CancelJobResponse(CancelJobResponse::Canceled(
                Vec::new(),
                n_tasks,
            ));
        }
    }

    let canceled_tasks = match tako_ref
        .send_message(FromGatewayMessage::CancelTasks(CancelTasks {
            tasks: tako_task_ids,
        }))
        .await
        .unwrap()
    {
        ToGatewayMessage::CancelTasksResponse(msg) => msg.cancelled_tasks,
        ToGatewayMessage::Error(msg) => {
            log::debug!("Canceling job failed: {}", msg.message);
            return ToClientMessage::Error(format!("Canceling job failed: {}", msg.message));
        }
        _ => {
            panic!("Invalid message");
        }
    };

    let (canceled_ids, already_finished) = {
        let mut state = state_ref.get_mut();
        let job = state.get_job_mut(job_id).unwrap();
        let canceled_ids: Vec<_> = canceled_tasks
            .iter()
            .map(|tako_id| job.set_cancel_state(*tako_id))
            .collect();
        let already_finished = job.n_tasks() - canceled_ids.len() as JobTaskCount;
        (canceled_ids, already_finished)
    };
    ToClientMessage::CancelJobResponse(CancelJobResponse::Canceled(canceled_ids, already_finished))
}

fn make_program_def_for_task(
    program_def: &ProgramDefinition,
    job_id: JobId,
    task_id: JobTaskId,
) -> ProgramDefinition {
    let job_id_str = job_id.to_string();
    let task_id_str = task_id.to_string();

    let make_replacement = |s: &str| {
        s.replace("%{JOB_ID}", &job_id_str)
            .replace("%{TASK_ID}", &task_id_str)
    };

    let mut def = program_def.clone();
    def.env
        .insert(create_hq_env("JOB_ID"), job_id.to_string().into());
    def.env
        .insert(create_hq_env("TASK_ID"), task_id.to_string().into());
    def.stdout = def
        .stdout
        .and_then(|p| p.to_str().map(make_replacement).map(PathBuf::from));
    def.stderr = def
        .stderr
        .and_then(|p| p.to_str().map(make_replacement).map(PathBuf::from));
    def
}

async fn handle_submit(
    state_ref: &StateRef,
    tako_ref: &TakoServer,
    message: SubmitRequest,
) -> ToClientMessage {
    if message.resources.validate().is_err() {
        return ToClientMessage::Error("Invalid resource request".to_string());
    }
    let resources = message.resources;
    let spec = message.spec;
    let pin = message.pin;

    let make_task = |job_id, task_id, tako_id, entry: Option<BString>| {
        let mut program = make_program_def_for_task(&spec, job_id, task_id);
        if let Some(e) = entry {
            program.env.insert(create_hq_env("ENTRY"), e);
        }
        let launcher_def = LauncherDefinition { program, pin };
        let body = rmp_serde::to_vec_named(&launcher_def).unwrap();
        TaskDef {
            id: tako_id,
            type_id: 0,
            body,
            keep: false,
            observe: true,
            n_outputs: 0,
            priority: 0,
            resources: resources.clone(),
        }
    };
    let (task_defs, job_detail) = {
        let mut state = state_ref.get_mut();
        let job_id = state.new_job_id();
        let task_count = match &message.job_type {
            JobType::Simple => 1,
            JobType::Array(a) => a.task_count(),
        };
        let tako_base_id = state.new_task_id(task_count);
        let task_defs = match (&message.job_type, message.entries) {
            (JobType::Simple, _) => vec![make_task(job_id, 0, tako_base_id, None)],
            (JobType::Array(a), None) => a
                .iter()
                .zip(tako_base_id..)
                .map(|(task_id, tako_id)| make_task(job_id, task_id, tako_id, None))
                .collect(),
            (JobType::Array(a), Some(entries)) => a
                .iter()
                .zip(tako_base_id..)
                .zip(entries.into_iter())
                .map(|((task_id, tako_id), entry)| make_task(job_id, task_id, tako_id, Some(entry)))
                .collect(),
        };
        let job = Job::new(
            message.job_type,
            job_id,
            tako_base_id,
            message.name.clone(),
            spec,
            resources,
            pin,
        );
        let job_detail = job.make_job_detail(false);
        state.add_job(job);

        (task_defs, job_detail)
    };
    match tako_ref
        .send_message(FromGatewayMessage::NewTasks(NewTasksMessage {
            tasks: task_defs,
        }))
        .await
        .unwrap()
    {
        ToGatewayMessage::NewTasksResponse(_) => { /* Ok */ }
        _ => {
            panic!("Invalid response");
        }
    };

    ToClientMessage::SubmitResponse(SubmitResponse { job: job_detail })
}

async fn handle_worker_list(state_ref: &StateRef) -> ToClientMessage {
    let state = state_ref.get();

    ToClientMessage::WorkerListResponse(WorkerListResponse {
        workers: state
            .get_workers()
            .values()
            .map(|w| w.make_info())
            .collect(),
    })
}
