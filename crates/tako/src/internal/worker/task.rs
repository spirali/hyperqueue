use crate::gateway::TaskDataFlags;
use crate::internal::common::resources::Allocation;
use crate::internal::common::stablemap::ExtractKey;
use crate::internal::messages::worker::ComputeTaskMsg;
use crate::internal::worker::task_comm::RunningTaskComm;
use crate::{InstanceId, Priority, TaskId, WorkerId};
use std::rc::Rc;
use std::time::Duration;

pub enum TaskState {
    Waiting(u32),
    Running(RunningTaskComm, Rc<Allocation>),
}

pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub priority: (Priority, Priority),
    pub instance_id: InstanceId,

    pub resources: crate::internal::common::resources::ResourceRequestVariants,
    pub time_limit: Option<Duration>,
    pub body: Box<[u8]>,
    pub node_list: Vec<WorkerId>, // Filled in multi-node tasks; otherwise empty
    pub data_flags: TaskDataFlags,
}

impl Task {
    pub fn new(message: ComputeTaskMsg) -> Self {
        Self {
            id: message.id,
            priority: (message.user_priority, message.scheduler_priority),
            state: TaskState::Waiting(0),
            instance_id: message.instance_id,
            resources: message.resources,
            time_limit: message.time_limit,
            body: message.body,
            node_list: message.node_list,
            data_flags: message.data_flags,
        }
    }

    #[inline]
    pub fn is_waiting(&self) -> bool {
        matches!(self.state, TaskState::Waiting(_))
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        matches!(self.state, TaskState::Waiting(0))
    }

    #[inline]
    pub fn is_running(&self) -> bool {
        matches!(self.state, TaskState::Running(_, _))
    }

    pub fn resource_allocation(&self) -> Option<&Allocation> {
        match &self.state {
            TaskState::Running(_, a) => Some(a),
            TaskState::Waiting(_) => None,
        }
    }

    pub fn task_comm_mut(&mut self) -> Option<&mut RunningTaskComm> {
        match self.state {
            TaskState::Running(ref mut comm, _) => Some(comm),
            _ => None,
        }
    }

    pub fn get_waiting(&self) -> u32 {
        match self.state {
            TaskState::Waiting(x) => x,
            _ => 0,
        }
    }

    pub fn decrease_waiting_count(&mut self) -> bool {
        match &mut self.state {
            TaskState::Waiting(ref mut x) => {
                assert!(*x > 0);
                *x -= 1;
                *x == 0
            }
            _ => unreachable!(),
        }
    }

    pub fn increase_waiting_count(&mut self) {
        match &mut self.state {
            TaskState::Waiting(ref mut x) => {
                *x += 1;
            }
            _ => unreachable!(),
        }
    }

    pub fn need_data_layer(&self) -> bool {
        self.data_flags.contains(TaskDataFlags::KEEP_ALL_OUTPUTS)
    }
}

impl ExtractKey<TaskId> for Task {
    #[inline]
    fn extract_key(&self) -> TaskId {
        self.id
    }
}
