#![cfg(test)]

use std::io::Cursor;
use std::ops::DerefMut;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::common::resources::descriptor::cpu_descriptor_from_socket_size;
use crate::common::resources::{
    CpuId, CpuRequest, GenericResourceAmount, GenericResourceDescriptor, GenericResourceId,
    GenericResourceRequest, NumOfCpus, ResourceDescriptor, ResourceRequest,
};
use crate::common::{Map, WrappedRcRefCell};
use crate::messages::common::{TaskConfiguration, TaskFailInfo, WorkerConfiguration};
use crate::messages::gateway::LostWorkerReason;
use crate::messages::worker::{StealResponse, StealResponseMsg, TaskFinishedMsg, ToWorkerMessage};
use crate::scheduler::state::SchedulerState;
use crate::server::comm::Comm;
use crate::server::core::Core;
use crate::server::reactor::{
    on_cancel_tasks, on_new_tasks, on_new_worker, on_steal_response, on_task_finished,
    on_task_running,
};
use crate::server::task::TaskRef;
use crate::server::worker::Worker;
use crate::server::worker_load::WorkerLoad;
use crate::transfer::auth::{deserialize, serialize};
use crate::{TaskId, WorkerId};

/// Memory stream for reading and writing at the same time.
pub struct MemoryStream {
    input: Cursor<Vec<u8>>,
    pub output: WrappedRcRefCell<Vec<u8>>,
}

impl AsyncRead for MemoryStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.input).poll_read(cx, buf)
    }
}
impl AsyncWrite for MemoryStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(self.output.get_mut().deref_mut()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(self.output.get_mut().deref_mut()).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(self.output.get_mut().deref_mut()).poll_shutdown(cx)
    }
}

pub struct TestEnv {
    core: Core,
    scheduler: SchedulerState,
    task_id_counter: u64,
    worker_id_counter: u32,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        TestEnv {
            core: Default::default(),
            scheduler: create_test_scheduler(),
            task_id_counter: 10,
            worker_id_counter: 100,
        }
    }

    pub fn core(&mut self) -> &mut Core {
        &mut self.core
    }

    pub fn task(&self, task_id: TaskId) -> TaskRef {
        self.core.get_task_by_id_or_panic(task_id).clone()
    }

    pub fn new_task(&mut self, builder: TaskBuilder) -> TaskRef {
        let tr = builder.build();
        submit_test_tasks(&mut self.core, &[&tr]);
        tr
    }

    pub fn new_generic_resource(&mut self, count: usize) {
        for i in 0..count {
            self.core
                .get_or_create_generic_resource_id(&format!("Res{}", i));
        }
    }

    pub fn new_task_assigned<W: Into<WorkerId>>(&mut self, builder: TaskBuilder, worker_id: W) {
        let tr = builder.build();
        submit_test_tasks(&mut self.core, &[&tr]);
        let task_id = tr.get().id();
        start_on_worker(&mut self.core, task_id, worker_id.into());
    }

    pub fn new_task_running<W: Into<WorkerId>>(&mut self, builder: TaskBuilder, worker_id: W) {
        let tr = builder.build();
        submit_test_tasks(&mut self.core, &[&tr]);
        let task_id = tr.get().id();
        start_on_worker_running(&mut self.core, task_id, worker_id.into());
    }

    pub fn worker<W: Into<WorkerId>>(&self, worker_id: W) -> &Worker {
        self.core.get_worker_by_id_or_panic(worker_id.into())
    }

    pub fn new_workers_ext(
        &mut self,
        defs: &[(u32, Option<Duration>, Vec<GenericResourceDescriptor>)],
    ) {
        for (i, (c, time_limit, grds)) in defs.iter().enumerate() {
            let worker_id = WorkerId::new(self.worker_id_counter);
            self.worker_id_counter += 1;

            let cpus = cpu_descriptor_from_socket_size(1, *c);
            let rd = ResourceDescriptor::new(cpus, grds.clone());

            let wcfg = WorkerConfiguration {
                resources: rd,
                listen_address: format!("1.1.1.{}:123", i),
                hostname: format!("test{}", i),
                work_dir: Default::default(),
                log_dir: Default::default(),
                heartbeat_interval: Duration::from_millis(1000),
                hw_state_poll_interval: Some(Duration::from_millis(1000)),
                idle_timeout: None,
                time_limit: time_limit.clone(),
                extra: Default::default(),
            };

            let worker = Worker::new(worker_id, wcfg, self.core.create_resource_map());
            on_new_worker(&mut self.core, &mut TestComm::default(), worker);
        }
    }

    pub fn new_workers(&mut self, cpus: &[u32]) {
        let defs: Vec<_> = cpus.iter().map(|c| (*c, None, Vec::new())).collect();
        self.new_workers_ext(&defs);
    }

    pub fn new_ready_tasks_cpus(&mut self, tasks: &[NumOfCpus]) -> Vec<TaskRef> {
        let trs: Vec<_> = tasks
            .iter()
            .map(|n_cpus| {
                let task_id = self.task_id_counter;
                self.task_id_counter += 1;
                TaskBuilder::new(task_id).cpus_compact(*n_cpus).build()
            })
            .collect();
        let trs_refs: Vec<_> = trs.iter().collect();
        submit_test_tasks(&mut self.core, &trs_refs);
        trs
    }

    pub fn _test_assign(&mut self, task_ref: &TaskRef, worker_id: WorkerId) {
        self.scheduler
            .test_assign(&mut self.core, &task_ref, worker_id);
        self.core.remove_from_ready_to_assign(task_ref);
    }

    pub fn test_assign<T: Into<TaskId>, W: Into<WorkerId>>(&mut self, task_id: T, worker_id: W) {
        self._test_assign(&self.task(task_id.into()), worker_id.into());
    }

    pub fn new_assigned_tasks_cpus(&mut self, tasks: &[&[NumOfCpus]]) {
        for (i, tdefs) in tasks.iter().enumerate() {
            let w_id = WorkerId::new(100 + i as u32);
            let trs = self.new_ready_tasks_cpus(tdefs);
            for tr in &trs {
                self._test_assign(tr, w_id);
            }
        }
    }

    pub fn check_worker_tasks<W: Into<WorkerId>, T: Into<TaskId> + Copy>(
        &self,
        worker_id: W,
        tasks: &[T],
    ) {
        let worker_id = worker_id.into();
        let ids = sorted_vec(
            self.core
                .get_worker_by_id_or_panic(worker_id)
                .tasks()
                .iter()
                .map(|t| t.get().id())
                .collect(),
        );
        assert_eq!(ids, sorted_vec(tasks.iter().map(|&id| id.into()).collect()));
    }

    pub fn worker_load<W: Into<WorkerId>>(&self, worker_id: W) -> &WorkerLoad {
        &self.core.get_worker_by_id_or_panic(worker_id.into()).load
    }

    pub fn check_worker_load_lower_bounds(&self, cpus: &[NumOfCpus]) {
        let found_cpus: Vec<NumOfCpus> = sorted_vec(
            self.core
                .get_workers()
                .map(|w| w.load.get_n_cpus())
                .collect(),
        );
        for (c, f) in cpus.iter().zip(found_cpus.iter()) {
            assert!(c <= f);
        }
    }

    pub fn finish_scheduling(&mut self) {
        let mut comm = create_test_comm();
        self.scheduler.finish_scheduling(&mut comm);
        self.core.sanity_check();
        println!("-------------");
        for worker in self.core.get_workers() {
            println!(
                "Worker {} ({}) {}",
                worker.id,
                worker.load.get_n_cpus(),
                worker
                    .tasks()
                    .iter()
                    .map(|t| format!("{}:{:?}", t.get().id(), &t.get().configuration.resources))
                    .collect::<Vec<String>>()
                    .join(", ")
            );
        }
    }

    pub fn schedule(&mut self) {
        let mut comm = create_test_comm();
        self.scheduler.run_scheduling(&mut self.core, &mut comm);
        self.core.sanity_check();
    }

    pub fn balance(&mut self) {
        self.scheduler.balance(&mut self.core);
        self.finish_scheduling();
    }
}

pub struct TaskBuilder {
    id: TaskId,
    inputs: Vec<TaskRef>,
    n_outputs: u32,
    resources: ResourceRequest,
}

impl TaskBuilder {
    pub fn new<T: Into<TaskId>>(id: T) -> TaskBuilder {
        TaskBuilder {
            id: id.into(),
            inputs: Default::default(),
            n_outputs: 0,
            resources: Default::default(),
        }
    }

    pub fn deps(mut self, deps: &[&TaskRef]) -> TaskBuilder {
        self.inputs = deps.iter().map(|&tr| tr.clone()).collect();
        self
    }

    pub fn outputs(mut self, value: u32) -> TaskBuilder {
        self.n_outputs = value;
        self
    }

    pub fn cpus_compact(mut self, cpu_request: NumOfCpus) -> TaskBuilder {
        self.resources.set_cpus(CpuRequest::Compact(cpu_request));
        self
    }

    pub fn time_request(mut self, time: u64) -> TaskBuilder {
        self.resources.set_time(Duration::new(time, 0));
        self
    }

    pub fn generic_res<Id: Into<GenericResourceId>>(
        mut self,
        idx: Id,
        amount: GenericResourceAmount,
    ) -> TaskBuilder {
        self.resources.add_generic_request(GenericResourceRequest {
            resource: idx.into(),
            amount,
        });
        self
    }

    pub fn build(self) -> TaskRef {
        self.resources.validate().unwrap();
        TaskRef::new(
            self.id,
            self.inputs,
            TaskConfiguration {
                resources: self.resources,
                n_outputs: self.n_outputs,
                time_limit: None,
                body: Default::default(),
            },
            Default::default(),
            false,
            false,
        )
    }
}

pub fn task<T: Into<TaskId>>(id: T) -> TaskRef {
    TaskBuilder::new(id.into()).outputs(1).build()
}

/* Deprecated: Use TaskBuilder directly */
pub fn task_with_deps<T: Into<TaskId>>(id: T, deps: &[&TaskRef], n_outputs: u32) -> TaskRef {
    TaskBuilder::new(id.into())
        .deps(deps)
        .outputs(n_outputs)
        .build()
}

/*
pub fn load_bin_test_data(path: &str) -> Vec<u8> {
    let path = get_test_path(path);
    std::fs::read(path).unwrap()
}*/

#[allow(unused)]
pub fn get_test_path(path: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(path)
        .to_str()
        .unwrap()
        .to_owned()
}

#[derive(Default, Debug)]
pub struct TestComm {
    pub worker_msgs: Map<WorkerId, Vec<ToWorkerMessage>>,
    pub broadcast_msgs: Vec<ToWorkerMessage>,

    pub client_task_finished: Vec<TaskId>,
    pub client_task_running: Vec<TaskId>,
    pub client_task_errors: Vec<(TaskId, Vec<TaskId>, TaskFailInfo)>,

    pub new_workers: Vec<(WorkerId, WorkerConfiguration)>,
    pub lost_workers: Vec<(WorkerId, Vec<TaskId>)>,

    pub need_scheduling: bool,
}

impl TestComm {
    pub fn take_worker_msgs<T: Into<WorkerId>>(
        &mut self,
        worker_id: T,
        len: usize,
    ) -> Vec<ToWorkerMessage> {
        let worker_id: WorkerId = worker_id.into();
        let msgs = match self.worker_msgs.remove(&worker_id) {
            None => {
                panic!("No messages for worker {}", worker_id)
            }
            Some(x) => x,
        };
        if len != 0 {
            assert_eq!(msgs.len(), len);
        }
        msgs
    }

    pub fn take_broadcasts(&mut self, len: usize) -> Vec<ToWorkerMessage> {
        assert_eq!(self.broadcast_msgs.len(), len);
        std::mem::take(&mut self.broadcast_msgs)
    }

    pub fn take_client_task_finished(&mut self, len: usize) -> Vec<TaskId> {
        assert_eq!(self.client_task_finished.len(), len);
        std::mem::take(&mut self.client_task_finished)
    }

    pub fn take_client_task_running(&mut self, len: usize) -> Vec<TaskId> {
        assert_eq!(self.client_task_running.len(), len);
        std::mem::take(&mut self.client_task_running)
    }

    pub fn take_client_task_errors(
        &mut self,
        len: usize,
    ) -> Vec<(TaskId, Vec<TaskId>, TaskFailInfo)> {
        assert_eq!(self.client_task_errors.len(), len);
        std::mem::take(&mut self.client_task_errors)
    }

    pub fn take_new_workers(&mut self) -> Vec<(WorkerId, WorkerConfiguration)> {
        std::mem::take(&mut self.new_workers)
    }

    pub fn take_lost_workers(&mut self) -> Vec<(WorkerId, Vec<TaskId>)> {
        std::mem::take(&mut self.lost_workers)
    }

    pub fn check_need_scheduling(&mut self) {
        assert!(self.need_scheduling);
        self.need_scheduling = false;
    }

    pub fn emptiness_check(&self) {
        if !self.worker_msgs.is_empty() {
            let ids: Vec<_> = self.worker_msgs.keys().collect();
            panic!("Unexpected worker messages for workers: {:?}", ids);
        }
        assert!(self.broadcast_msgs.is_empty());

        assert!(self.client_task_finished.is_empty());
        assert!(self.client_task_running.is_empty());
        assert!(self.client_task_errors.is_empty());

        assert!(self.new_workers.is_empty());
        assert!(self.lost_workers.is_empty());

        assert!(!self.need_scheduling);
    }
}

impl Comm for TestComm {
    fn send_worker_message(&mut self, worker_id: WorkerId, message: &ToWorkerMessage) {
        let data = serialize(&message).unwrap();
        let message = deserialize(&data).unwrap();
        self.worker_msgs.entry(worker_id).or_default().push(message);
    }

    fn broadcast_worker_message(&mut self, message: &ToWorkerMessage) {
        let data = serialize(&message).unwrap();
        let message = deserialize(&data).unwrap();
        self.broadcast_msgs.push(message);
    }

    fn ask_for_scheduling(&mut self) {
        self.need_scheduling = true;
    }

    fn send_client_task_finished(&mut self, task_id: TaskId) {
        self.client_task_finished.push(task_id);
    }

    fn send_client_task_started(&mut self, task_id: TaskId, _worker_id: WorkerId) {
        self.client_task_running.push(task_id);
    }

    fn send_client_task_error(
        &mut self,
        task_id: TaskId,
        consumers: Vec<TaskId>,
        error_info: TaskFailInfo,
    ) {
        self.client_task_errors
            .push((task_id, consumers, error_info));
    }

    fn send_client_worker_new(&mut self, worker_id: WorkerId, configuration: &WorkerConfiguration) {
        self.new_workers.push((worker_id, configuration.clone()));
    }

    fn send_client_worker_lost(
        &mut self,
        worker_id: WorkerId,
        running_tasks: Vec<TaskId>,
        _reason: LostWorkerReason,
    ) {
        self.lost_workers.push((worker_id, running_tasks));
    }
}

pub fn create_test_comm() -> TestComm {
    TestComm::default()
}

pub fn create_test_workers(core: &mut Core, cpus: &[u32]) {
    for (i, c) in cpus.iter().enumerate() {
        let worker_id = WorkerId::new((100 + i) as u32);

        let wcfg = WorkerConfiguration {
            resources: ResourceDescriptor::simple(*c),
            listen_address: format!("1.1.1.{}:123", i),
            hostname: format!("test{}", i),
            work_dir: Default::default(),
            log_dir: Default::default(),
            heartbeat_interval: Duration::from_millis(1000),
            hw_state_poll_interval: Some(Duration::from_millis(1000)),
            idle_timeout: None,
            time_limit: None,
            extra: Default::default(),
        };

        let worker = Worker::new(worker_id, wcfg, Default::default());
        on_new_worker(core, &mut TestComm::default(), worker);
    }
}

pub fn submit_test_tasks(core: &mut Core, tasks: &[&TaskRef]) {
    on_new_tasks(
        core,
        &mut TestComm::default(),
        tasks.iter().map(|&tr| tr.clone()).collect(),
    );
}

pub(crate) fn force_assign<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    scheduler: &mut SchedulerState,
    task_id: T,
    worker_id: W,
) {
    let task_ref = core.get_task_by_id_or_panic(task_id.into()).clone();
    core.remove_from_ready_to_assign(&task_ref);
    let mut task = task_ref.get_mut();
    scheduler.assign(core, &mut task, task_ref.clone(), worker_id.into());
}

pub(crate) fn force_reassign<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    scheduler: &mut SchedulerState,
    task_id: T,
    worker_id: W,
) {
    // The same as force_assign, but do not expect that task in ready_to_assign array
    let task_ref = core.get_task_by_id_or_panic(task_id.into()).clone();
    let mut task = task_ref.get_mut();
    scheduler.assign(core, &mut task, task_ref.clone(), worker_id.into());
}

pub fn fail_steal<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    task_id: T,
    worker_id: W,
    target_worker_id: W,
) {
    let task_id = task_id.into();
    start_stealing(core, task_id, target_worker_id.into());
    let mut comm = create_test_comm();
    on_steal_response(
        core,
        &mut comm,
        worker_id.into(),
        StealResponseMsg {
            responses: vec![(task_id, StealResponse::Running)],
        },
    )
}

pub fn start_stealing<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    task_id: T,
    new_worker_id: W,
) {
    let mut scheduler = create_test_scheduler();
    force_reassign(core, &mut scheduler, task_id.into(), new_worker_id.into());
    let mut comm = create_test_comm();
    scheduler.finish_scheduling(&mut comm);
}

pub fn start_on_worker<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    task_id: T,
    worker_id: W,
) {
    let mut scheduler = create_test_scheduler();
    let mut comm = TestComm::default();
    force_assign(core, &mut scheduler, task_id.into(), worker_id.into());
    scheduler.finish_scheduling(&mut comm);
}

pub fn start_on_worker_running<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    task_id: T,
    worker_id: W,
) {
    let task_id = task_id.into();
    let worker_id = worker_id.into();

    let mut scheduler = create_test_scheduler();
    let mut comm = TestComm::default();
    force_assign(core, &mut scheduler, task_id, worker_id);
    scheduler.finish_scheduling(&mut comm);
    on_task_running(core, &mut comm, worker_id, task_id);
}

pub fn cancel_tasks<T: Into<TaskId> + Copy>(core: &mut Core, task_ids: &[T]) {
    let mut comm = create_test_comm();
    on_cancel_tasks(
        core,
        &mut comm,
        &task_ids.iter().map(|&v| v.into()).collect::<Vec<_>>(),
    );
}

pub fn finish_on_worker<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    task_id: T,
    worker_id: W,
    size: u64,
) {
    let mut comm = TestComm::default();
    on_task_finished(
        core,
        &mut comm,
        worker_id.into(),
        TaskFinishedMsg {
            id: task_id.into(),
            size,
        },
    );
}

pub fn start_and_finish_on_worker<W: Into<WorkerId>, T: Into<TaskId>>(
    core: &mut Core,
    task_id: T,
    worker_id: W,
    size: u64,
) {
    let task_id = task_id.into();
    let worker_id = worker_id.into();

    start_on_worker(core, task_id, worker_id);
    finish_on_worker(core, task_id, worker_id, size);
}

pub fn submit_example_1(core: &mut Core) {
    /*
       11  12 <- keep
        \  / \
         13  14
         /\  /
        16 15 <- keep
        |
        17
    */

    let t1 = task(11);
    let t2 = task(12);
    t2.get_mut().set_keep_flag(true);
    let t3 = task_with_deps(13, &[&t1, &t2], 1);
    let t4 = task_with_deps(14, &[&t2], 1);
    let t5 = task_with_deps(15, &[&t3, &t4], 1);
    t5.get_mut().set_keep_flag(true);
    let t6 = task_with_deps(16, &[&t3], 1);
    let t7 = task_with_deps(17, &[&t6], 1);
    submit_test_tasks(core, &[&t1, &t2, &t3, &t4, &t5, &t6, &t7]);
}

pub fn submit_example_2(core: &mut Core) {
    /* Graph simple
         T1
        /  \
       T2   T3
       |  / |\
       T4   | T6
        \      \
         \ /   T7
          T5
    */

    let t1 = task_with_deps(1, &[], 1);
    let t2 = task_with_deps(2, &[&t1], 1);
    let t3 = task_with_deps(3, &[&t1], 1);
    let t4 = task_with_deps(4, &[&t2, &t3], 1);
    let t5 = task_with_deps(5, &[&t4], 1);
    let t6 = task_with_deps(6, &[&t3], 1);
    let t7 = task_with_deps(7, &[&t6], 1);

    submit_test_tasks(core, &[&t1, &t2, &t3, &t4, &t5, &t6, &t7]);
}

pub fn sorted_vec<T: Ord>(mut vec: Vec<T>) -> Vec<T> {
    vec.sort();
    vec
}

pub fn as_cpus(ids: Vec<Vec<u32>>) -> Vec<Vec<CpuId>> {
    ids.into_iter()
        .map(|v| v.into_iter().map(|id| id.into()).collect())
        .collect()
}

#[allow(unused)]
pub fn enable_test_logging() {
    env_logger::builder().is_test(false).init()
}

pub fn expect_error_message<T>(result: anyhow::Result<T>, msg: &str) {
    match result {
        Ok(_) => panic!("Expected error, got Ok"),
        Err(error) => {
            let formatted = format!("{:?}", error);
            if !formatted.contains(msg) {
                panic!("Did not find `{}` in `{}`", msg, formatted);
            }
        }
    }
}

pub(crate) fn create_test_scheduler() -> SchedulerState {
    SchedulerState::new()
}

impl SchedulerState {
    pub(crate) fn test_assign(&mut self, core: &mut Core, task_ref: &TaskRef, worker_id: WorkerId) {
        let mut task = task_ref.get_mut();
        self.assign(core, &mut task, task_ref.clone(), worker_id);
    }
}
