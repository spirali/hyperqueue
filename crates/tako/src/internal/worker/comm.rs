use crate::comm::serialize;
use crate::internal::messages::worker::FromWorkerMessage;
use bytes::Bytes;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

pub struct RealWorkerComm {
    sender: Option<UnboundedSender<Bytes>>,
    start_task_notify: Rc<Notify>,
    worker_is_empty_notify: Option<Rc<Notify>>,
}

pub enum WorkerComm {
    Real(RealWorkerComm),
    #[cfg(test)]
    Test(crate::internal::tests::utils::worker::TestWorkerComm),
}

impl WorkerComm {
    pub fn new(sender: UnboundedSender<Bytes>, start_task_notify: Rc<Notify>) -> Self {
        WorkerComm::Real(RealWorkerComm {
            sender: Some(sender),
            start_task_notify,
            worker_is_empty_notify: None,
        })
    }

    #[cfg(test)]
    pub fn new_test_comm() -> Self {
        WorkerComm::Test(crate::internal::tests::utils::worker::TestWorkerComm::new())
    }

    #[cfg(test)]
    pub fn test(&mut self) -> &mut crate::internal::tests::utils::worker::TestWorkerComm {
        match self {
            Self::Real(_) => panic!("Cannot get testing testing comm"),
            Self::Test(comm) => comm,
        }
    }

    pub fn send_message_to_server(&mut self, message: FromWorkerMessage) {
        match self {
            Self::Real(comm) => {
                if let Some(sender) = comm.sender.as_ref() {
                    if sender.send(serialize(&message).unwrap().into()).is_err() {
                        log::debug!("Message could not be sent to server");
                    }
                } else {
                    log::debug!(
            "Attempting to send a message to server, but server has already disconnected"
            );
                }
            }
            #[cfg(test)]
            WorkerComm::Test(comm) => comm.send_message_to_server(message),
        }
    }

    pub fn notify_worker_is_empty(&mut self) {
        match self {
            Self::Real(comm) => {
                if let Some(notify) = &comm.worker_is_empty_notify {
                    log::debug!("Notifying that worker is empty");
                    notify.notify_one()
                }
            }
            #[cfg(test)]
            WorkerComm::Test(comm) => comm.notify_worker_is_empty(),
        }
    }

    pub fn set_idle_worker_notify(&mut self, notify: Rc<Notify>) {
        match self {
            Self::Real(comm) => comm.worker_is_empty_notify = Some(notify),
            #[cfg(test)]
            WorkerComm::Test(_) => {}
        }
    }

    pub fn notify_start_task(&mut self) {
        match self {
            Self::Real(comm) => comm.start_task_notify.notify_one(),
            #[cfg(test)]
            WorkerComm::Test(comm) => {
                comm.notify_start_task();
            }
        }
    }

    pub fn drop_sender(&mut self) {
        match self {
            Self::Real(comm) => comm.sender = None,
            #[cfg(test)]
            WorkerComm::Test(_) => {}
        }
    }
}
