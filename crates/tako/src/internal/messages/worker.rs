use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::datasrv::{DataId, DataObjectId};
use crate::gateway::TaskDataFlags;
use crate::hwstats::WorkerHwStateMessage;
use crate::internal::common::resources::{ResourceAmount, ResourceIndex};
use crate::internal::messages::common::TaskFailInfo;
use crate::resources::ResourceFractions;
use crate::task::SerializedTaskContext;
use crate::{InstanceId, Priority};
use crate::{TaskId, WorkerId};

#[derive(Serialize, Deserialize, Debug)]
pub struct WorkerRegistrationResponse {
    pub worker_id: WorkerId,
    pub resource_names: Vec<String>,
    pub other_workers: Vec<NewWorkerMsg>,
    pub server_idle_timeout: Option<Duration>,
    pub server_uid: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ComputeTaskMsg {
    pub id: TaskId,

    pub instance_id: InstanceId,

    pub user_priority: Priority,
    pub scheduler_priority: Priority,

    pub resources: crate::internal::common::resources::ResourceRequestVariants,
    pub time_limit: Option<Duration>,
    pub node_list: Vec<WorkerId>,

    pub data_deps: Vec<DataObjectId>,
    pub data_flags: TaskDataFlags,

    #[serde(with = "serde_bytes")]
    pub body: Box<[u8]>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TaskIdsMsg {
    pub ids: Vec<TaskId>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkerResourceCounts {
    pub n_resources: Vec<ResourceAmount>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct NewWorkerMsg {
    pub worker_id: WorkerId,
    pub address: String,
    pub resources: WorkerResourceCounts,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TaskIdMsg {
    pub id: TaskId,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ToWorkerMessage {
    ComputeTask(ComputeTaskMsg),
    StealTasks(TaskIdsMsg),
    CancelTasks(TaskIdsMsg),
    NewWorker(NewWorkerMsg),
    LostWorker(WorkerId),
    SetReservation(bool),
    Stop,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TaskFinishedMsg {
    pub id: TaskId,
    pub output_ids: Vec<DataId>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TaskFailedMsg {
    pub id: TaskId,
    pub info: TaskFailInfo,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TaskRunningMsg {
    pub id: TaskId,
    pub context: SerializedTaskContext,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct DataDownloadedMsg {
    pub id: TaskId,
}

#[derive(Deserialize, Serialize, Debug)]
pub enum StealResponse {
    Ok,
    NotHere,
    Running,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct StealResponseMsg {
    pub responses: Vec<(TaskId, StealResponse)>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(test, derive(Eq, PartialEq))]
pub struct ResourceAllocation {
    pub resource: String,
    pub indices: Vec<(ResourceIndex, ResourceFractions)>,
    pub amount: ResourceAmount,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(test, derive(Eq, PartialEq))]
pub struct TaskResourceAllocation {
    pub resources: Vec<ResourceAllocation>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkerOverview {
    pub id: WorkerId,
    pub running_tasks: Vec<(TaskId, TaskResourceAllocation)>,
    pub hw_state: Option<WorkerHwStateMessage>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum WorkerStopReason {
    IdleTimeout,
    TimeLimitReached,
    Interrupted,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum FromWorkerMessage {
    TaskFinished(TaskFinishedMsg),
    TaskFailed(TaskFailedMsg),
    TaskRunning(TaskRunningMsg),
    StealResponse(StealResponseMsg),
    Overview(WorkerOverview),
    Heartbeat,
    Stop(WorkerStopReason),
    PlacementQuery(DataObjectId),
}
