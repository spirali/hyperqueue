use crate::internal::datasrv::datanode_connection_handler;
use crate::internal::worker::state::WorkerStateRef;
use crate::{Map, TaskId};
use bstr::{BStr, BString, ByteSlice, ByteVec};
use futures::StreamExt;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::borrow::{Borrow, Cow};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::spawn_local;
use tokio_util::codec::length_delimited::Builder;
use tokio_util::codec::LengthDelimitedCodec;

pub(crate) enum Registration {
    DataConnection { task_id: TaskId },
    // TODO: SubworkerConnection
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Token {
    raw_token: Rc<BString>,
}

impl Token {
    pub fn new() -> Self {
        let raw_token = new_raw_token();
        Token {
            raw_token: Rc::new(raw_token),
        }
    }
    pub fn as_bstr(&self) -> &BStr {
        &self.raw_token.as_bstr()
    }
}

impl Borrow<BStr> for Token {
    fn borrow(&self) -> &BStr {
        self.raw_token.as_bstr()
    }
}

pub(crate) struct LocalCommState {
    unix_socket_path: PathBuf,
    registered_tokens: Map<Token, Registration>,
}

fn new_raw_token() -> BString {
    "hq0-"
        .bytes()
        .chain(rand::thread_rng().sample_iter(&Alphanumeric).take(12))
        .collect()
}

impl LocalCommState {
    pub fn new() -> Self {
        let path = {
            let rnd_part: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(8)
                .map(char::from)
                .collect();
            std::env::temp_dir().join(Path::new(&format!("hq-lc-{rnd_part}")))
        };
        LocalCommState {
            unix_socket_path: path,
            registered_tokens: Map::new(),
        }
    }
    pub fn register_task(&mut self, registration: Registration) -> Token {
        loop {
            let token = Token::new();
            if self.registered_tokens.contains_key(&token) {
                log::debug!("Token collision");
                continue;
            }
            self.registered_tokens.insert(token.clone(), registration);
            return token;
        }
    }

    pub fn unregister_token(&mut self, token: &Token) {
        self.registered_tokens.remove(token);
    }

    pub fn check_token(&self, token: &BStr) -> Option<&Registration> {
        self.registered_tokens.get(token)
    }

    pub fn data_access_key(&self, token: &Token) -> BString {
        let mut out = self
            .unix_socket_path
            .as_os_str()
            .as_bytes()
            .as_bstr()
            .to_owned();
        out.push_byte(b':');
        out.push_str(token.as_bstr());
        out
    }

    pub fn start(&self, state_ref: WorkerStateRef) -> crate::Result<()> {
        let listener = UnixListener::bind(&self.unix_socket_path)?;
        spawn_local(async move {
            loop {
                let socket = listener.accept().await;
                let state_ref = state_ref.clone();
                if let Ok((socket, _)) = socket {
                    spawn_local(async move {
                        if let Err(e) = handle_connection(state_ref, socket).await {
                            log::error!("lc connection error: {}", e);
                        }
                    });
                } else if let Err(e) = socket {
                    log::error!("failed to accept lc connection: {e}");
                };
            }
        });
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct IntroMessage {
    pub token: BString,
}

pub(crate) fn make_protocol_builder() -> Builder {
    *LengthDelimitedCodec::builder().little_endian()
}

async fn handle_connection(state_ref: WorkerStateRef, stream: UnixStream) -> crate::Result<()> {
    let (tx, mut rx) = make_protocol_builder().new_framed(stream).split();
    if let Some(data) = rx.next().await {
        let data = data?;
        let message: IntroMessage = bincode::deserialize(&data)?;
        let state = state_ref.get_mut();
        let lc_state = state.lc_state.borrow();
        let registration = lc_state
            .check_token(message.token.as_bstr())
            .ok_or_else(|| crate::Error::GenericError("Invalid token".to_string()))?;
        match registration {
            Registration::DataConnection { task_id } => {
                let task_id = *task_id;
                let data_node_ref = state.data_node_ref.clone();
                drop(lc_state);
                drop(state);
                datanode_connection_handler(data_node_ref, rx, tx, task_id).await?;
            }
        }
    } else {
        log::debug!("Local connection: closed without providing intro message")
    }
    Ok(())
}

async fn run_local_comm(state_ref: WorkerStateRef, listener: UnixListener) -> crate::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                log::debug!("New local connection via unix socket: {addr:?}");
                let state_ref = state_ref.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) = handle_connection(state_ref, stream).await {
                        log::error!("Local connection error: {err}");
                    }
                });
            }
            Err(e) => {
                log::debug!("Accepting a new data client via unix socket failed: {e}")
            }
        }
    }
}
