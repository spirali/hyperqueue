#![deny(clippy::await_holding_refcell_ref)]

#[macro_use]
pub mod internal;

pub mod comm;
pub mod gateway;
pub mod hwstats;
pub mod launcher;
pub mod program;

pub use crate::internal::common::index::{AsIdVec, ItemId};
pub use crate::internal::common::taskgroup::TaskGroup;
pub use crate::internal::common::utils::format_comma_delimited;
pub use crate::internal::common::WrappedRcRefCell;
pub use crate::internal::common::{Map, Set};

define_id_type!(WorkerId, u32);
define_id_type!(TaskId, u64);
define_id_type!(InstanceId, u32);

// Priority: Bigger number -> Higher priority
pub type Priority = i32;
pub type PriorityTuple = (Priority, Priority); // user priority, scheduler priority

pub type Error = internal::common::error::DsError;
pub type Result<T> = std::result::Result<T, Error>;

pub mod resources {
    pub use crate::internal::common::resources::{
        Allocation, AllocationRequest, NumOfNodes, ResourceAllocation, ResourceAmount,
        ResourceDescriptor, ResourceDescriptorItem, ResourceDescriptorKind, ResourceFractions,
        ResourceIndex, ResourceLabel, ResourceRequest, ResourceRequestEntries,
        ResourceRequestEntry, ResourceRequestVariants, ResourceUnits, TimeRequest,
        AMD_GPU_RESOURCE_NAME, CPU_RESOURCE_ID, CPU_RESOURCE_NAME, MEM_RESOURCE_NAME,
        NVIDIA_GPU_RESOURCE_NAME,
    };

    pub use crate::internal::common::resources::map::ResourceMap;

    pub use crate::internal::common::resources::descriptor::DescriptorError;

    pub use crate::internal::common::resources::amount::{
        FRACTIONS_MAX_DIGITS, FRACTIONS_PER_UNIT,
    };
}

pub mod server {
    pub use crate::internal::server::rpc::ConnectionDescriptor;
    pub use crate::internal::server::start::server_start;
}

pub mod worker {
    pub use crate::internal::messages::worker::WorkerOverview;
    pub use crate::internal::worker::configuration::ServerLostPolicy;
    pub use crate::internal::worker::configuration::WorkerConfiguration;

    pub use crate::internal::worker::rpc::run_worker;
}

pub mod task {
    pub type SerializedTaskContext = Vec<u8>;
}

pub mod datasrv {
    pub use crate::internal::datasrv::client::DataClient;
    pub use crate::internal::datasrv::dataobj::{DataId, DataInputId, DataObjectId};
}
