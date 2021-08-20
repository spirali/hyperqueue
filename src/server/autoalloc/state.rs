use crate::server::autoalloc::descriptor::QueueDescriptor;
use crate::server::autoalloc::AutoAllocResult;
use crate::Map;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const MAX_EVENT_QUEUE_LENGTH: usize = 100;

pub type DescriptorName = String;

pub struct AutoAllocState {
    /// How often should the auto alloc process be invoked?
    refresh_interval: Duration,
    descriptors: Map<DescriptorName, DescriptorState>,
}

impl AutoAllocState {
    pub fn new(refresh_interval: Duration) -> AutoAllocState {
        Self {
            refresh_interval,
            descriptors: Default::default(),
        }
    }

    pub fn refresh_interval(&self) -> Duration {
        self.refresh_interval
    }

    pub fn add_descriptor(
        &mut self,
        name: DescriptorName,
        descriptor: QueueDescriptor,
    ) -> AutoAllocResult<()> {
        if self.descriptors.contains_key(&name) {
            return Err(anyhow::anyhow!(format!(
                "Descriptor {} already exists",
                name
            )));
        }
        self.descriptors.insert(name, descriptor.into());
        Ok(())
    }

    pub fn get_descriptor(&self, key: &str) -> Option<&DescriptorState> {
        self.descriptors.get(key)
    }

    pub fn get_descriptor_mut(&mut self, key: &str) -> Option<&mut DescriptorState> {
        self.descriptors.get_mut(key)
    }

    pub fn descriptor_names(&self) -> impl Iterator<Item = &str> {
        self.descriptors.keys().map(|s| s.as_str())
    }

    pub fn descriptors(&self) -> impl Iterator<Item = (&str, &DescriptorState)> {
        self.descriptors.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Represents the state of a single allocation queue.
pub struct DescriptorState {
    pub descriptor: QueueDescriptor,
    /// Active allocations
    allocations: Map<AllocationId, Allocation>,
    /// Records events that have occurred on this queue.
    events: VecDeque<AllocationEventHolder>,
}

impl From<QueueDescriptor> for DescriptorState {
    fn from(descriptor: QueueDescriptor) -> Self {
        Self {
            descriptor,
            allocations: Default::default(),
            events: Default::default(),
        }
    }
}

impl DescriptorState {
    pub fn add_event<T: Into<AllocationEventHolder>>(&mut self, event: T) {
        self.events.push_back(event.into());
        if self.events.len() > MAX_EVENT_QUEUE_LENGTH {
            self.events.pop_front();
        }
    }

    pub fn get_events(&self) -> &VecDeque<AllocationEventHolder> {
        &self.events
    }

    pub fn get_allocation(&self, id: &str) -> Option<&Allocation> {
        self.allocations.get(id)
    }

    pub fn get_allocation_mut(&mut self, id: &str) -> Option<&mut Allocation> {
        self.allocations.get_mut(id)
    }

    pub fn active_allocations(&self) -> impl Iterator<Item = &Allocation> {
        self.allocations.values().filter(|alloc| alloc.is_active())
    }

    pub fn all_allocations(&self) -> impl Iterator<Item = &Allocation> {
        self.allocations.values()
    }

    pub fn add_allocation(&mut self, key: AllocationId, allocation: Allocation) {
        if let Some(allocation) = self.allocations.insert(key, allocation) {
            log::warn!("Duplicate allocation detected: {}", allocation.id);
        }
    }

    pub fn remove_allocation(&mut self, key: &str) {
        if self.allocations.remove(key).is_none() {
            log::warn!("Trying to remove non-existent allocation {}", key);
        }
    }
}

pub type AllocationId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allocation {
    pub id: AllocationId,
    pub worker_count: u64,
    pub status: AllocationStatus,
    pub working_dir: PathBuf,
}

impl Allocation {
    /// Returns true if the allocation is currently in queue or running
    pub fn is_active(&self) -> bool {
        matches!(
            self.status,
            AllocationStatus::Queued { .. } | AllocationStatus::Running { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllocationTimeInfo {
    pub queued_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
}

impl AllocationTimeInfo {
    pub fn queued_now() -> Self {
        Self {
            queued_at: SystemTime::now(),
            started_at: None,
            finished_at: None,
        }
    }
}

// TODO: represent the state in a better way
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AllocationStatus {
    Queued(AllocationTimeInfo),
    Running(AllocationTimeInfo),
    Finished(AllocationTimeInfo),
    Failed(AllocationTimeInfo),
}

impl AllocationStatus {
    pub fn get_time_info(&self) -> AllocationTimeInfo {
        match self {
            AllocationStatus::Queued(times)
            | AllocationStatus::Running(times)
            | AllocationStatus::Finished(times)
            | AllocationStatus::Failed(times) => times.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllocationEventHolder {
    pub date: SystemTime,
    pub event: AllocationEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AllocationEvent {
    AllocationQueued(AllocationId),
    AllocationStarted(AllocationId),
    AllocationFinished(AllocationId),
    AllocationFailed(AllocationId),
    AllocationDisappeared(AllocationId),
    QueueFail { error: String },
    StatusFail { error: String },
}

impl From<AllocationEvent> for AllocationEventHolder {
    fn from(event: AllocationEvent) -> Self {
        Self {
            date: SystemTime::now(),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::common::manager::info::ManagerType;
    use crate::server::autoalloc::descriptor::{CreatedAllocation, QueueHandler};
    use crate::server::autoalloc::state::{AllocationId, AllocationStatus};
    use crate::server::autoalloc::{AutoAllocResult, AutoAllocState, QueueDescriptor, QueueInfo};
    use std::future::Future;
    use std::pin::Pin;
    use std::time::Duration;

    #[test]
    fn test_add_descriptor_with_same_name_twice() {
        let mut state = AutoAllocState::new(Duration::from_secs(1));

        impl QueueHandler for () {
            fn schedule_allocation(
                &self,
                _worker_count: u64,
            ) -> Pin<Box<dyn Future<Output = AutoAllocResult<CreatedAllocation>>>> {
                todo!()
            }

            fn get_allocation_status(
                &self,
                _allocation_id: AllocationId,
            ) -> Pin<Box<dyn Future<Output = AutoAllocResult<Option<AllocationStatus>>>>>
            {
                todo!()
            }
        }

        let info = QueueInfo::new("".to_string(), 1, 1, None);
        let name = "foo".to_string();
        assert!(state
            .add_descriptor(
                name.clone(),
                QueueDescriptor::new(ManagerType::Pbs, info.clone(), Box::new(()))
            )
            .is_ok());
        assert!(state
            .add_descriptor(
                name,
                QueueDescriptor::new(ManagerType::Pbs, info.clone(), Box::new(()))
            )
            .is_err());
    }
}
