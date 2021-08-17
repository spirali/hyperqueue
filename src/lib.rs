#![deny(clippy::await_holding_refcell_ref)]

pub mod client;
pub mod common;
pub mod dashboard;
pub mod server;
pub mod stream;
pub mod transfer;
pub mod worker;

pub type Map<K, V> = hashbrown::HashMap<K, V>;
pub type Set<T> = hashbrown::HashSet<T>;

pub type WorkerId = tako::WorkerId;
pub type TakoTaskId = tako::TaskId;
pub type TaskTypeId = tako::TaskTypeId;
pub type Priority = tako::Priority;

pub type JobId = u32;
pub type JobTaskId = u32;
pub type JobTaskCount = u32;
pub type JobTaskStep = u32;

pub type Error = crate::common::error::HqError;
pub type Result<T> = std::result::Result<T, Error>;
