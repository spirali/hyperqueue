use super::dataobj::{DataObjectId, InputMap};
use crate::datasrv::DataInputId;
use crate::internal::common::error::DsError;
use crate::internal::datasrv::messages::{
    DataObject, FromDataNodeLocalMessage, ToDataNodeLocalMessage,
};
use crate::internal::worker::localcomm::make_protocol_builder;
use crate::internal::worker::state::WorkerStateRef;
use crate::{Map, TaskId, WrappedRcRefCell};
use bytes::{Bytes, BytesMut};
use futures::{Sink, SinkExt, Stream, StreamExt};
use hashbrown::hash_map::Entry;
use std::path::Path;
use std::rc::Rc;
use tokio::net::UnixListener;
use tokio_util::codec::length_delimited::Builder;
use tokio_util::codec::LengthDelimitedCodec;

pub(crate) struct DataConnectionSession {
    task_id: TaskId,
    inputs: Vec<DataObjectId>,
}

pub(crate) struct DataNode {
    store: Map<DataObjectId, Rc<DataObject>>,
}

pub(crate) type DataNodeRef = WrappedRcRefCell<DataNode>;

impl DataNode {
    pub fn new() -> Self {
        DataNode { store: Map::new() }
    }

    pub fn get_object(&self, data_object_id: DataObjectId) -> Option<&Rc<DataObject>> {
        self.store.get(&data_object_id)
    }

    pub fn put_object(
        &mut self,
        data_object_id: DataObjectId,
        data_object: Rc<DataObject>,
    ) -> crate::Result<()> {
        match self.store.entry(data_object_id) {
            Entry::Occupied(_) => Err(DsError::GenericError(format!(
                "DataObject {data_object_id} already exists"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(data_object);
                Ok(())
            }
        }
    }
}

impl DataNodeRef {
    pub fn new() -> Self {
        DataNodeRef::wrap(DataNode::new())
    }
}

async fn datanode_message_handler(
    data_node_ref: &DataNodeRef,
    task_id: TaskId,
    input_map: &Option<Rc<Vec<DataObjectId>>>,
    message: ToDataNodeLocalMessage,
    tx: &mut (impl Sink<Bytes> + Unpin),
) -> crate::Result<()> {
    match message {
        ToDataNodeLocalMessage::PutDataObject {
            data_id,
            data_object,
        } => {
            data_node_ref
                .get_mut()
                .put_object(DataObjectId::new(task_id, data_id), Rc::new(data_object))?;
            send_message(tx, FromDataNodeLocalMessage::Uploaded(data_id)).await?;
        }
        ToDataNodeLocalMessage::GetInput { input_id } => {
            if let Some(data_id) = input_map
                .as_ref()
                .and_then(|map| map.get(input_id.as_num() as usize))
            {
                if let Some(data_obj) = data_node_ref.get_mut().get_object(*data_id) {
                    send_message(tx, FromDataNodeLocalMessage::DataObject(data_obj.clone()))
                        .await?;
                } else {
                    return Err(DsError::GenericError(format!(
                        "DataObject {data_id} (input {input_id}) not found"
                    )));
                }
            } else {
                return Err(DsError::GenericError(format!("Input {input_id} not found")));
            }
        }
    }
    Ok(())
}

async fn send_message(
    tx: &mut (impl Sink<Bytes> + Unpin),
    message: FromDataNodeLocalMessage,
) -> crate::Result<()> {
    let data = bincode::serialize(&message)?;
    tx.send(data.into())
        .await
        .map_err(|_| DsError::GenericError("Data connection send message error".to_string()))?;
    Ok(())
}

pub(crate) async fn datanode_connection_handler(
    data_node_ref: DataNodeRef,
    mut rx: impl Stream<Item = Result<BytesMut, std::io::Error>> + Unpin,
    mut tx: (impl Sink<Bytes> + Unpin),
    task_id: TaskId,
    input_map: Option<Rc<Vec<DataObjectId>>>,
) -> crate::Result<()> {
    while let Some(data) = rx.next().await {
        let data = data?;
        let message: ToDataNodeLocalMessage = bincode::deserialize(&data)?;
        if let Err(e) =
            datanode_message_handler(&data_node_ref, task_id, &input_map, message, &mut tx).await
        {
            log::debug!("Data handler failed: {}", e);
            send_message(&mut tx, FromDataNodeLocalMessage::Error(e.to_string())).await?;
        }
    }
    Ok(())
}
