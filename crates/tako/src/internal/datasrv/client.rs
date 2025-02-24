use crate::internal::common::error::DsError;
use crate::internal::datasrv::dataobj::{DataId, DataInputId};
use crate::internal::datasrv::messages::{
    DataObject, FromDataNodeLocalMessage, ToDataNodeLocalMessage,
};
use crate::internal::worker::localcomm::IntroMessage;
use bstr::BStr;
use futures::{SinkExt, StreamExt};
use std::path::Path;
use std::rc::Rc;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub struct DataClient {
    stream: Framed<UnixStream, LengthDelimitedCodec>,
}

impl DataClient {
    pub async fn connect(path: &Path, token: &BStr) -> crate::Result<Self> {
        log::debug!("Creating local connection to: {}", path.display());
        let stream = UnixStream::connect(path).await?;
        let mut stream =
            crate::internal::worker::localcomm::make_protocol_builder().new_framed(stream);
        let data = bincode::serialize(&IntroMessage {
            token: token.into(),
        })?;
        stream.send(data.into()).await?;
        Ok(DataClient { stream })
    }

    async fn send_message(&mut self, message: ToDataNodeLocalMessage) -> crate::Result<()> {
        let data = bincode::serialize(&message)?;
        self.stream.send(data.into()).await?;
        Ok(())
    }

    async fn read_message(&mut self) -> crate::Result<FromDataNodeLocalMessage> {
        let data = self.stream.next().await;
        Ok(match data {
            Some(data) => {
                let data = data?;
                bincode::deserialize(&data)?
            }
            None => {
                return Err(DsError::GenericError(
                    "Unexpected end of stream".to_string(),
                ))
            }
        })
    }

    pub async fn put_data_object(
        &mut self,
        data_id: DataId,
        mime_type: String,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        let message = ToDataNodeLocalMessage::PutDataObject {
            data_id,
            data_object: DataObject { mime_type, data },
        };
        self.send_message(message).await?;
        let message = self.read_message().await?;
        match message {
            FromDataNodeLocalMessage::Uploaded(id) if id == data_id => Ok(()),
            FromDataNodeLocalMessage::Error(message) => Err(crate::Error::GenericError(message)),
            _ => Err(DsError::GenericError("Invalid response".to_string())),
        }
    }

    pub async fn get_input(&mut self, input_id: DataInputId) -> crate::Result<Rc<DataObject>> {
        let message = ToDataNodeLocalMessage::GetInput { input_id };
        self.send_message(message).await?;
        let message = self.read_message().await?;
        match message {
            FromDataNodeLocalMessage::DataObject(dataobj) => Ok(dataobj),
            FromDataNodeLocalMessage::Error(message) => Err(crate::Error::GenericError(message)),
            _ => Err(DsError::GenericError("Invalid response".to_string())),
        }
    }
}
