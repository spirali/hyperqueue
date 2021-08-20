//! This module controls autoalloc (automatic allocation), a background process that queues jobs
//! into PBS/Slurm in response to user requirements and task workload to provide more workers for
//! the HQ runtime.
//!
//! The term `allocation` represents a PBS/Slurm job in this module, to distinguish itself from
//! HQ jobs.
pub use process::autoalloc_process;
pub use state::AutoAllocState;

mod descriptor;
mod process;
mod state;

pub type AutoAllocResult<T> = anyhow::Result<T>;

pub use descriptor::pbs::PbsHandler;
pub use descriptor::{QueueDescriptor, QueueInfo};
pub use state::{Allocation, AllocationEvent, AllocationEventHolder, AllocationStatus};
