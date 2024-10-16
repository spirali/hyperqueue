use crate::internal::datasrv::dataobj::{DataId, DataInputId};
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display, Formatter};

#[derive(Serialize, Deserialize)]
pub(crate) struct DataObject {
    pub mime_type: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl Debug for DataObject {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "<DataObj mimetype='{}' len={}>",
            self.mime_type,
            self.data.len()
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ToDataNodeLocalMessage {
    PutDataObject {
        data_id: DataId,
        data_object: DataObject,
    },
    GetInput {
        input_id: DataInputId,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum FromDataNodeLocalMessage {
    Uploaded(DataId),
    DataObject(DataObject),
    Error(String),
}
