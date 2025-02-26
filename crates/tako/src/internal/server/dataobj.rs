use crate::datasrv::DataObjectId;
use crate::internal::common::stablemap::ExtractKey;
use crate::internal::server::task::Task;
use crate::{TaskId, WorkerId};
use std::collections::HashSet;

#[derive(Debug)]
pub(crate) struct DataObjectHandle {
    // TODO: Optimize for small number of placements (enum?)
    id: DataObjectId,
    placement: HashSet<WorkerId>,
    size: usize,
}

impl DataObjectHandle {
    pub fn new(id: DataObjectId, initial_placement: WorkerId, size: usize) -> Self {
        let mut placement = HashSet::new();
        placement.insert(initial_placement);
        DataObjectHandle {
            id,
            placement,
            size,
        }
    }
}

impl ExtractKey<DataObjectId> for DataObjectHandle {
    #[inline]
    fn extract_key(&self) -> DataObjectId {
        self.id
    }
}
