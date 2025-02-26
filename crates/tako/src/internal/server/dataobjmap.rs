use crate::datasrv::DataObjectId;
use crate::internal::common::stablemap::StableMap;
use crate::internal::server::dataobj::DataObjectHandle;

#[derive(Default, Debug)]
pub(crate) struct DataObjectMap {
    data_objs: StableMap<DataObjectId, DataObjectHandle>,
}

impl DataObjectMap {
    #[inline(always)]
    pub fn insert(&mut self, data_obj: DataObjectHandle) {
        self.data_objs.insert(data_obj);
    }
}
