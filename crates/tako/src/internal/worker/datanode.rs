use crate::datasrv::DataInputId;
use crate::internal::common::error::DsError;
use crate::internal::datasrv::dataobj::{DataObjectId, InputMap};
use crate::internal::datasrv::messages::{
    DataObject, FromDataNodeLocalMessage, ToDataNodeLocalMessage,
};
use crate::internal::worker::localcomm::make_protocol_builder;
use crate::internal::worker::state::{WorkerState, WorkerStateRef};
use crate::{Map, Priority, PriorityTuple, Set, TaskId, WorkerId, WrappedRcRefCell};
use bytes::{Bytes, BytesMut};
use futures::{Sink, SinkExt, Stream, StreamExt};
use hashbrown::hash_map::Entry;
use std::path::Path;
use std::rc::Rc;
use tokio::net::UnixListener;
use tokio::sync::{oneshot, Notify};
use tokio::task::{spawn_local, AbortHandle, JoinSet};
use tokio_util::codec::length_delimited::Builder;
use tokio_util::codec::LengthDelimitedCodec;

pub(crate) struct DataConnectionSession {
    task_id: TaskId,
    inputs: Vec<DataObjectId>,
}

pub(crate) struct RunningDownloadHandle {
    placement_resolver: Option<oneshot::Sender<WorkerId>>,
    abort: AbortHandle,
}

pub(crate) struct DataNode {
    store: Map<DataObjectId, Rc<DataObject>>,

    download_queue: priority_queue::PriorityQueue<DataObjectId, PriorityTuple>,
    running_downloads: Map<DataObjectId, RunningDownloadHandle>,
    notify_downloader: Rc<Notify>,
}

impl DataNode {
    pub fn new() -> Self {
        DataNode {
            store: Map::new(),
            download_queue: Default::default(),
            running_downloads: Default::default(),
            notify_downloader: Rc::new(Notify::new()),
        }
    }

    pub fn get_object(&self, data_object_id: DataObjectId) -> Option<&Rc<DataObject>> {
        self.store.get(&data_object_id)
    }

    pub fn has_object(&self, data_object_id: DataObjectId) -> bool {
        self.store.contains_key(&data_object_id)
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

    fn is_downloading(&self, data_object_id: DataObjectId) -> bool {
        self.running_downloads.contains_key(&data_object_id)
            || self.download_queue.get(&data_object_id).is_some()
    }

    pub fn download_object(&mut self, data_object_id: DataObjectId, priority_tuple: PriorityTuple) {
        if self.is_downloading(data_object_id) {
            return;
        }
        log::debug!("Scheduling data object download {data_object_id}");
        self.download_queue.push(data_object_id, priority_tuple);
        self.notify_downloader.notify_one();
    }
}

async fn datanode_message_handler(
    state_ref: &WorkerStateRef,
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
            state_ref
                .get_mut()
                .data_node
                .put_object(DataObjectId::new(task_id, data_id), Rc::new(data_object))?;
            send_message(tx, FromDataNodeLocalMessage::Uploaded(data_id)).await?;
        }
        ToDataNodeLocalMessage::GetInput { input_id } => {
            if let Some(data_id) = input_map
                .as_ref()
                .and_then(|map| map.get(input_id.as_num() as usize))
            {
                if let Some(data_obj) = state_ref.get_mut().data_node.get_object(*data_id) {
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
    data_node_ref: WorkerStateRef,
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

const MAX_PARALLEL_DOWNLOADS: usize = 4;

pub(crate) fn start_download_process(state_ref: WorkerStateRef) {
    spawn_local(async move {
        let notify = {
            let state = state_ref.get();
            state.data_node.notify_downloader.clone()
        };
        notify.notified().await;
        let mut join_set = JoinSet::new();
        loop {
            {
                let mut state = state_ref.get_mut();
                while state.data_node.running_downloads.len() < MAX_PARALLEL_DOWNLOADS {
                    if let Some((data_id, _)) = state.data_node.download_queue.pop() {
                        let abort =
                            join_set.spawn_local(download_data_object(state_ref.clone(), data_id));
                        assert!(state
                            .data_node
                            .running_downloads
                            .insert(
                                data_id,
                                RunningDownloadHandle {
                                    placement_resolver: None,
                                    abort,
                                }
                            )
                            .is_none());
                    } else {
                        break;
                    }
                }
                state.data_node.running_downloads.is_empty()
            };

            tokio::select! {
                _ = notify.notified() => {}
                r = join_set.join_next() => {
                    if let Some(_) = r {
                        todo!()
                    } else {
                        notify.notified().await;
                    }
                }
            }
        }
    });
}

async fn download_data_object(
    state_ref: WorkerStateRef,
    data_id: DataObjectId,
) -> crate::Result<()> {
    let (sender, receiver) = oneshot::channel();
    {
        log::debug!("Trying to resolve placement for {data_id}");
        let mut state = state_ref.get_mut();
        if let Some(handle) = state.data_node.running_downloads.get_mut(&data_id) {
            handle.placement_resolver = Some(sender);
        } else {
            return Ok(());
        }
    }
    let worker_id = receiver.await.unwrap();
    todo!();
    Ok(())
}
