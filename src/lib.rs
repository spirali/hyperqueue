pub mod client;
pub mod common;
pub mod script;
pub mod server;
pub mod transfer;
pub mod worker;

pub type Map<K, V> = hashbrown::HashMap<K, V>;
pub type Set<T> = hashbrown::HashSet<T>;

pub type WorkerId = tako::WorkerId;
pub type TakoTaskId = tako::TaskId;
pub type TaskTypeId = tako::TaskTypeId;

pub type JobId = u64;
pub type JobTaskId = u32;
pub type JobTaskCount = u32;

pub type Error = crate::common::error::HqError;
pub type Result<T> = std::result::Result<T, Error>;
