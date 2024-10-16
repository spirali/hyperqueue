#![deny(clippy::await_holding_refcell_ref)]

pub mod client;
pub mod common;
#[cfg(feature = "dashboard")]
pub mod dashboard;
pub mod server;
pub mod stream;
pub mod transfer;
pub mod worker;

#[cfg(test)]
pub(crate) mod tests;

use serde::{Deserialize, Serialize};
pub use tako::{Map, Set};

pub type Error = crate::common::error::HqError;
pub type Result<T> = std::result::Result<T, Error>;

// ID types
use tako::define_id_type;

pub use tako::WorkerId;
pub type TakoTaskId = tako::TaskId;
pub type Priority = tako::Priority;

define_id_type!(JobId, u32);
define_id_type!(JobTaskId, u32);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobDataObjectId {
    pub task_id: JobTaskId,
    pub data_id: DataId,
}

impl JobDataObjectId {
    pub fn to_dataobj_id(&self, job_id: JobId) -> DataObjectId {
        DataObjectId::new(make_tako_id(job_id, self.task_id), self.data_id)
    }
}

pub type JobTaskCount = u32;
pub type JobTaskStep = u32;

pub const DEFAULT_WORKER_GROUP_NAME: &str = "default";

// Reexports
pub use tako;
use tako::datasrv::{DataId, DataObjectId};
pub use tako::WrappedRcRefCell;

pub const HQ_VERSION: &str = {
    match option_env!("HQ_BUILD_VERSION") {
        Some(version) => version,
        None => const_format::concatcp!(env!("CARGO_PKG_VERSION"), "-dev"),
    }
};

pub fn make_tako_id(job_id: JobId, task_id: JobTaskId) -> TakoTaskId {
    TakoTaskId::new(((job_id.as_num() as u64) << 32) + task_id.as_num() as u64)
}

pub fn unwrap_tako_id(tako_task_id: TakoTaskId) -> (JobId, JobTaskId) {
    let num = tako_task_id.as_num();
    (
        JobId::new((num >> 32) as u32),
        JobTaskId::new((num & 0xffffffff) as u32),
    )
}

#[cfg(test)]
mod test {
    use crate::{make_tako_id, unwrap_tako_id, JobId, JobTaskId};

    #[test]
    fn test_make_tako_id() {
        assert_eq!(
            unwrap_tako_id(make_tako_id(JobId(123), JobTaskId(5))),
            (JobId(123), JobTaskId(5))
        );
    }
}
