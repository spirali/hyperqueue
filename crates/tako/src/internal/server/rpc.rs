use core::time::Duration;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use futures::stream::{SplitSink, SplitStream};
use futures::{Stream, StreamExt};
use orion::aead::streaming::{StreamOpener, StreamSealer};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::comm::{ConnectionRegistration, RegisterWorker};
use crate::gateway::LostWorkerReason;
use crate::internal::common::error::DsError;
use crate::internal::common::taskgroup::TaskGroup;
use crate::internal::messages::worker::{
    FromWorkerMessage, NewWorkerMsg, WorkerRegistrationResponse, WorkerStopReason,
};
use crate::internal::server::comm::{Comm, CommSenderRef};
use crate::internal::server::core::CoreRef;
use crate::internal::server::reactor::{
    on_new_worker, on_remove_worker, on_steal_response, on_task_error, on_task_finished,
    on_task_running,
};
use crate::internal::server::worker::Worker;
use crate::internal::transfer::auth::{
    do_authentication, forward_queue_to_sealed_sink, is_encryption_disabled, open_message,
    serialize,
};
use crate::internal::transfer::transport::make_protocol_builder;
use crate::internal::worker::configuration::sync_worker_configuration;
use crate::WorkerId;

pub struct ConnectionDescriptor {
    pub address: std::net::SocketAddr,
    pub receiver: SplitStream<Framed<tokio::net::TcpStream, LengthDelimitedCodec>>,
    pub sender: SplitSink<Framed<tokio::net::TcpStream, LengthDelimitedCodec>, bytes::Bytes>,
    pub sealer: Option<StreamSealer>,
    pub opener: Option<StreamOpener>,
}

pub(crate) async fn connection_initiator(
    listener: TcpListener,
    core_ref: CoreRef,
    comm_ref: CommSenderRef,
) -> crate::Result<()> {
    let group = TaskGroup::default();
    loop {
        let (socket, address) = group.run_until(listener.accept()).await?;
        socket.set_nodelay(true)?;
        let core_ref2 = core_ref.clone();
        let comm_ref = comm_ref.clone();
        group.add_task(async move {
            log::debug!("New connection: {}", address);
            let (connection, message) =
                match worker_authentication(&core_ref2, socket, address).await {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("Worker connection ended with: {:?}", e);
                        return;
                    }
                };
            match message {
                ConnectionRegistration::Worker(msg) => {
                    match worker_rpc_loop(&core_ref2, &comm_ref, connection, msg).await {
                        Ok(_) => log::debug!("Connection ended: {}", address),
                        Err(e) => log::warn!("Worker connection ended with: {:?}", e),
                    }
                }
                ConnectionRegistration::Custom => {
                    if let Some(handler) = core_ref2.get().custom_conn_handler() {
                        handler(connection);
                    }
                }
            }
        });
    }
}

pub(crate) async fn worker_authentication(
    core_ref: &CoreRef,
    stream: TcpStream,
    address: std::net::SocketAddr,
) -> crate::Result<(ConnectionDescriptor, ConnectionRegistration)> {
    let (mut writer, mut reader) = make_protocol_builder().new_framed(stream).split();

    let secret_key = core_ref.get().secret_key().clone();
    let has_key = secret_key.is_some();
    let (sealer, mut opener) = do_authentication(
        0,
        "server".to_string(),
        "worker".to_string(),
        secret_key.clone(),
        &mut writer,
        &mut reader,
    )
    .await?;
    if !is_encryption_disabled() {
        assert_eq!(sealer.is_some(), has_key);
    }

    let message_data = timeout(Duration::from_secs(15), reader.next())
        .await
        .map_err(|_| "Worker registration did not arrive")?
        .ok_or_else(|| {
            DsError::from("The remote side closed connection without worker registration")
        })??;

    let message: ConnectionRegistration = open_message(&mut opener, &message_data)?;

    log::debug!("Worker registration from {}", address);

    let connection = ConnectionDescriptor {
        address,
        receiver: reader,
        sender: writer,
        opener,
        sealer,
    };
    Ok((connection, message))
}

async fn worker_rpc_loop(
    core_ref: &CoreRef,
    comm_ref: &CommSenderRef,
    connection: ConnectionDescriptor,
    msg: RegisterWorker,
) -> crate::Result<()> {
    let worker_id = core_ref.get_mut().new_worker_id();
    log::info!(
        "Worker {} registered from {}",
        worker_id,
        connection.address
    );

    let heartbeat_interval = msg.configuration.heartbeat_interval;
    log::debug!("Worker heartbeat: {:?}", heartbeat_interval);
    // Sanity that interval is not too small
    assert!(heartbeat_interval.as_millis() > 150);

    let mut configuration = msg.configuration;
    sync_worker_configuration(&mut configuration, *core_ref.get().idle_timeout());

    let idle_timeout = configuration.idle_timeout;
    let (queue_sender, queue_receiver) = tokio::sync::mpsc::unbounded_channel::<Bytes>();

    {
        let mut core = core_ref.get_mut();

        /* Make sure that all resources provided by Worker has an Id */
        for item in &configuration.resources.resources {
            core.get_or_create_resource_id(&item.name);
        }
        let worker = Worker::new(worker_id, configuration.clone(), core.create_resource_map());

        on_new_worker(&mut core, &mut *comm_ref.get_mut(), worker);
    }

    /* Send registration message, this has to be after on_new_worker
    because of registration of resources */
    let message = WorkerRegistrationResponse {
        worker_id,
        resource_names: core_ref.get().create_resource_map().into_vec(),
        other_workers: core_ref
            .get()
            .get_workers()
            .filter_map(|w| {
                if w.id != worker_id {
                    Some(NewWorkerMsg {
                        worker_id: w.id(),
                        address: w.configuration().listen_address.clone(),
                        resources: w.resources.to_transport(),
                    })
                } else {
                    None
                }
            })
            .collect(),
        server_idle_timeout: *core_ref.get().idle_timeout(),
        server_uid: core_ref.get().server_uid().to_string(),
    };
    queue_sender
        .send(serialize(&message).unwrap().into())
        .unwrap();

    comm_ref.get_mut().add_worker(worker_id, queue_sender);
    let snd_loop =
        forward_queue_to_sealed_sink(queue_receiver, connection.sender, connection.sealer);

    let periodic_check = async move {
        let mut interval = {
            tokio::time::interval(
                // Idle timeout might be smaller than 500ms in tests.
                #[allow(clippy::manual_clamp)]
                heartbeat_interval
                    .min(idle_timeout.unwrap_or(heartbeat_interval))
                    // Sanity check that interval is not too short
                    .max(Duration::from_millis(500)),
            )
        };
        loop {
            interval.tick().await;
            let now = Instant::now();
            let core = core_ref.get();
            let worker = core.get_worker_by_id_or_panic(worker_id);
            let elapsed = now - worker.last_heartbeat;

            if elapsed > heartbeat_interval * 2 {
                log::debug!("Heartbeat not arrived, worker={}", worker.id);
                break LostWorkerReason::HeartbeatLost;
            }
        }
    };

    let reason = tokio::select! {
        result = worker_receive_loop(core_ref.clone(), comm_ref.clone(), worker_id, connection.receiver, connection.opener) => {
            log::debug!("Receive loop terminated ({result:?}), worker={worker_id}");
            if let Ok(Some(reason)) = result {
                match reason {
                    WorkerStopReason::IdleTimeout => LostWorkerReason::IdleTimeout,
                    WorkerStopReason::TimeLimitReached => LostWorkerReason::TimeLimitReached,
                    WorkerStopReason::Interrupted => LostWorkerReason::ConnectionLost
                }
            } else {
                LostWorkerReason::ConnectionLost
            }
        }
        e = snd_loop => {
            log::debug!("Sending loop terminated: {:?}, worker={}", e, worker_id);
            LostWorkerReason::ConnectionLost
        }
        r = periodic_check => {
            log::debug!("Heartbeat loop terminated, worker={}", worker_id);
            r
        }
    };

    log::info!(
        "Worker {} connection closed (connection: {})",
        worker_id,
        connection.address
    );
    let mut core = core_ref.get_mut();
    let mut comm = comm_ref.get_mut();
    let stopping = { core.get_worker_by_id_or_panic(worker_id).is_stopping() };
    comm.remove_worker(worker_id);

    let reason = if stopping {
        LostWorkerReason::Stopped
    } else {
        reason
    };

    on_remove_worker(&mut core, &mut *comm, worker_id, reason);
    Ok(())
}

pub(crate) async fn worker_receive_loop<
    Reader: Stream<Item = Result<BytesMut, std::io::Error>> + Unpin,
>(
    core_ref: CoreRef,
    comm_ref: CommSenderRef,
    worker_id: WorkerId,
    mut receiver: Reader,
    mut opener: Option<StreamOpener>,
) -> crate::Result<Option<WorkerStopReason>> {
    while let Some(message) = receiver.next().await {
        let message: FromWorkerMessage = open_message(&mut opener, &message?)?;
        let mut core = core_ref.get_mut();
        let mut comm = comm_ref.get_mut();
        match message {
            FromWorkerMessage::TaskFinished(msg) => {
                on_task_finished(&mut core, &mut *comm, worker_id, msg);
            }
            FromWorkerMessage::TaskRunning(msg) => {
                on_task_running(&mut core, &mut *comm, worker_id, msg);
            }
            FromWorkerMessage::TaskFailed(msg) => {
                on_task_error(&mut core, &mut *comm, worker_id, msg.id, msg.info);
            }
            FromWorkerMessage::StealResponse(msg) => {
                on_steal_response(&mut core, &mut *comm, worker_id, msg)
            }
            FromWorkerMessage::Heartbeat => {
                if let Some(worker) = core.get_worker_mut(worker_id) {
                    log::debug!("Heartbeat received, worker={}", worker_id);
                    worker.last_heartbeat = Instant::now();
                };
            }
            FromWorkerMessage::Overview(overview) => {
                comm.send_client_worker_overview(overview);
            }
            FromWorkerMessage::Stop(reason) => {
                return Ok(Some(reason));
            }
            FromWorkerMessage::PlacementQuery(data_id) => {
                todo!()
            }
        }
    }
    Ok(None)
}
