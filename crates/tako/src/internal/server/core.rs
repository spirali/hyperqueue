use std::sync::Arc;
use std::time::Duration;

use orion::aead::SecretKey;

use crate::gateway::ServerInfo;
use crate::internal::common::resources::map::{ResourceIdAllocator, ResourceMap};
use crate::internal::common::resources::{ResourceId, ResourceRequestVariants};
use crate::internal::common::{Set, WrappedRcRefCell};
use crate::internal::scheduler::multinode::MultiNodeQueue;
use crate::internal::server::dataobjmap::DataObjectMap;
use crate::internal::server::rpc::ConnectionDescriptor;
use crate::internal::server::task::{Task, TaskRuntimeState};
use crate::internal::server::taskmap::TaskMap;
use crate::internal::server::worker::Worker;
use crate::internal::server::workergroup::WorkerGroup;
use crate::internal::server::workerload::WorkerResources;
use crate::internal::server::workermap::WorkerMap;
use crate::{Map, TaskId, WorkerId};

pub(crate) type CustomConnectionHandler = Box<dyn Fn(ConnectionDescriptor)>;

#[derive(Default)]
pub struct Core {
    tasks: TaskMap,
    workers: WorkerMap,
    worker_groups: Map<String, WorkerGroup>,
    data_objects: DataObjectMap,

    /* Scheduler items */
    parked_resources: Set<WorkerResources>, // Resources of workers that has flag NOTHING_TO_LOAD
    // TODO: benchmark and possibly replace with a set
    single_node_ready_to_assign: Vec<TaskId>,
    multi_node_ready_to_assign: Vec<TaskId>,
    multi_node_queue: MultiNodeQueue,
    has_new_tasks: bool,

    sleeping_sn_tasks: Vec<TaskId>, // Tasks that cannot be scheduled to any available worker

    maximal_task_id: TaskId,
    worker_id_counter: u32,
    resource_map: ResourceIdAllocator,
    worker_listen_port: u16,

    idle_timeout: Option<Duration>,

    secret_key: Option<Arc<SecretKey>>,
    server_uid: String,
    custom_conn_handler: Option<CustomConnectionHandler>,
}

pub(crate) type CoreRef = WrappedRcRefCell<Core>;

impl CoreRef {
    pub fn new(
        worker_listen_port: u16,
        secret_key: Option<Arc<SecretKey>>,
        idle_timeout: Option<Duration>,
        custom_conn_handler: Option<CustomConnectionHandler>,
        server_uid: String,
        worker_id_initial_value: WorkerId,
    ) -> Self {
        CoreRef::wrap(Core {
            worker_listen_port,
            secret_key,
            idle_timeout,
            custom_conn_handler,
            server_uid,
            worker_id_counter: worker_id_initial_value.as_num(),
            ..Default::default()
        })
    }
}

impl Core {
    // #[inline]
    // pub fn split_tasks_workers(&self) -> (&TaskMap, &WorkerMap) {
    //     (&self.tasks, &self.workers)
    // }

    #[inline]
    pub fn split_tasks_workers_mut(&mut self) -> (&mut TaskMap, &mut WorkerMap) {
        (&mut self.tasks, &mut self.workers)
    }

    pub fn new_worker_id(&mut self) -> WorkerId {
        self.worker_id_counter += 1;
        WorkerId::new(self.worker_id_counter)
    }

    pub fn worker_groups(&self) -> &Map<String, WorkerGroup> {
        &self.worker_groups
    }

    #[inline]
    pub fn is_used_task_id(&self, task_id: TaskId) -> bool {
        task_id <= self.maximal_task_id
    }

    pub fn idle_timeout(&self) -> &Option<Duration> {
        &self.idle_timeout
    }

    pub fn server_uid(&self) -> &str {
        &self.server_uid
    }

    pub(crate) fn multi_node_queue_split_mut(
        &mut self,
    ) -> (
        &mut MultiNodeQueue,
        &mut TaskMap,
        &mut WorkerMap,
        &Map<String, WorkerGroup>,
    ) {
        (
            &mut self.multi_node_queue,
            &mut self.tasks,
            &mut self.workers,
            &self.worker_groups,
        )
    }

    pub(crate) fn multi_node_queue_split(&self) -> (&MultiNodeQueue, &TaskMap, &WorkerMap) {
        (&self.multi_node_queue, &self.tasks, &self.workers)
    }

    pub fn park_workers(&mut self) {
        for worker in self.workers.values_mut() {
            if worker.is_underloaded()
                && worker
                    .sn_tasks()
                    .iter()
                    .all(|&task_id| self.tasks.get_task(task_id).is_sn_running())
            {
                log::debug!("Parking worker {}", worker.id);
                worker.set_parked_flag(true);
                self.parked_resources.insert(worker.resources.clone());
            }
        }
    }

    pub fn add_sleeping_sn_task(&mut self, task_id: TaskId) {
        self.sleeping_sn_tasks.push(task_id);
    }

    pub fn sleeping_sn_tasks(&self) -> &[TaskId] {
        &self.sleeping_sn_tasks
    }

    pub fn take_sleeping_tasks(&mut self) -> Vec<TaskId> {
        std::mem::take(&mut self.sleeping_sn_tasks)
    }

    pub fn get_server_info(&self) -> ServerInfo {
        ServerInfo {
            worker_listen_port: self.worker_listen_port,
        }
    }

    pub fn sn_ready_to_assign(&self) -> &[TaskId] {
        &self.single_node_ready_to_assign
    }

    pub fn take_single_node_ready_to_assign(&mut self) -> Vec<TaskId> {
        std::mem::take(&mut self.single_node_ready_to_assign)
    }

    pub fn take_multi_node_ready_to_assign(&mut self) -> Vec<TaskId> {
        std::mem::take(&mut self.multi_node_ready_to_assign)
    }

    pub fn get_worker_listen_port(&self) -> u16 {
        self.worker_listen_port
    }

    pub fn check_has_new_tasks_and_reset(&mut self) -> bool {
        let result = self.has_new_tasks;
        self.has_new_tasks = false;
        result
    }

    pub fn new_worker(&mut self, worker: Worker) {
        /* Wake up sleeping tasks */
        let mut sleeping_sn_tasks = self.take_sleeping_tasks();
        self.single_node_ready_to_assign
            .append(&mut sleeping_sn_tasks);

        self.multi_node_queue.wakeup_sleeping_tasks();

        let worker_id = worker.id;
        if let Some(g) = self.worker_groups.get_mut(&worker.configuration.group) {
            g.new_worker(worker_id);
        } else {
            let mut worker_ids = Set::new();
            worker_ids.insert(worker_id);
            self.worker_groups.insert(
                worker.configuration.group.clone(),
                WorkerGroup::new(worker_ids),
            );
        }
        self.workers.insert(worker_id, worker);
    }

    pub fn remove_worker(&mut self, worker_id: WorkerId) -> Worker {
        let worker = self.workers.get_worker(worker_id);
        let group = self
            .worker_groups
            .get_mut(&worker.configuration.group)
            .unwrap();
        group.remove_worker(worker_id);
        if group.is_empty() {
            self.worker_groups.remove(&worker.configuration.group);
        }
        self.workers.remove(&worker_id).unwrap()
    }

    #[inline]
    pub fn get_worker_by_id(&self, id: WorkerId) -> Option<&Worker> {
        self.workers.get(&id)
    }

    #[inline]
    pub fn get_worker_by_id_or_panic(&self, id: WorkerId) -> &Worker {
        self.workers.get(&id).unwrap_or_else(|| {
            panic!("Asking for invalid worker id={id}");
        })
    }

    #[inline]
    pub fn get_worker_mut_by_id_or_panic(&mut self, id: WorkerId) -> &mut Worker {
        self.workers.get_mut(&id).unwrap_or_else(|| {
            panic!("Asking for invalid worker id={id}");
        })
    }

    #[inline]
    pub fn get_worker_mut(&mut self, id: WorkerId) -> Option<&mut Worker> {
        self.workers.get_mut(&id)
    }

    #[inline]
    pub fn get_workers(&self) -> impl Iterator<Item = &Worker> {
        self.workers.values()
    }

    #[inline]
    pub fn get_workers_mut(&mut self) -> impl Iterator<Item = &mut Worker> {
        self.workers.values_mut()
    }

    #[inline]
    pub fn get_worker_map(&self) -> &WorkerMap {
        &self.workers
    }

    #[inline]
    pub fn has_workers(&self) -> bool {
        !self.workers.is_empty()
    }

    pub fn add_task(&mut self, task: Task) {
        let is_ready = task.is_ready();
        let task_id = task.id;
        self.has_new_tasks = true;
        assert!(self.tasks.insert(task).is_none());
        if is_ready {
            self.add_ready_to_assign(task_id);
        }
    }

    #[inline(never)]
    pub fn wakeup_parked_resources(&mut self) {
        log::debug!("Waking up parked resources");
        for worker in self.workers.values_mut() {
            worker.set_parked_flag(false);
        }
        self.parked_resources.clear();
    }

    pub fn has_parked_resources(&mut self) -> bool {
        !self.parked_resources.is_empty()
    }

    #[inline]
    pub fn check_parked_resources(&self, request: &ResourceRequestVariants) -> bool {
        for res in &self.parked_resources {
            if res.is_capable_to_run(request) {
                return true;
            }
        }
        false
    }

    pub fn add_ready_to_assign(&mut self, task_id: TaskId) {
        let is_multi_node = self
            .get_task(task_id)
            .configuration
            .resources
            .is_multi_node();
        if is_multi_node {
            self.multi_node_ready_to_assign.push(task_id);
        } else {
            self.single_node_ready_to_assign.push(task_id);
        }
    }

    // TODO: move to TaskMap
    /// Removes a single task.
    /// It can still remain in [`ready_to_assign`], where it will remain until the scheduler picks
    /// it up.
    #[must_use]
    pub fn remove_task(&mut self, task_id: TaskId) -> TaskRuntimeState {
        let task = self
            .tasks
            .remove(task_id)
            .expect("Trying to remove non-existent task");
        assert!(!task.has_consumers());
        task.state
    }

    /// Removes multiple tasks at once, to reduce memory consumption
    pub fn remove_tasks_batched(&mut self, tasks: &Set<TaskId>) {
        for &task_id in tasks {
            let _ = self.remove_task(task_id);
        }
        self.single_node_ready_to_assign
            .retain(|t| !tasks.contains(t));
    }

    #[inline]
    pub fn task_map(&self) -> &TaskMap {
        &self.tasks
    }

    #[inline]
    pub fn task_map_mut(&mut self) -> &mut TaskMap {
        &mut self.tasks
    }

    #[inline]
    pub fn get_task(&self, task_id: TaskId) -> &Task {
        self.tasks.get_task(task_id)
    }

    #[inline]
    pub fn get_task_mut(&mut self, task_id: TaskId) -> &mut Task {
        self.tasks.get_task_mut(task_id)
    }

    #[inline]
    pub fn find_task(&self, task_id: TaskId) -> Option<&Task> {
        self.tasks.find_task(task_id)
    }

    #[inline]
    pub fn find_task_mut(&mut self, task_id: TaskId) -> Option<&mut Task> {
        self.tasks.find_task_mut(task_id)
    }

    pub fn custom_conn_handler(&self) -> &Option<CustomConnectionHandler> {
        &self.custom_conn_handler
    }

    #[cfg(test)]
    pub fn sanity_check(&self) {
        let fw_check = |task: &Task| {
            for task_dep in &task.task_deps {
                assert!(self.tasks.get_task(*task_dep).is_finished());
            }
            for &task_id in task.get_consumers() {
                assert!(self.tasks.get_task(task_id).is_waiting());
            }
        };

        let worker_check_sn = |core: &Core, task_id: TaskId, wid: WorkerId| {
            for (worker_id, worker) in core.workers.iter() {
                if let Some(mn) = worker.mn_task() {
                    assert_ne!(mn.task_id, task_id);
                }
                if wid == *worker_id {
                    assert!(worker.sn_tasks().contains(&task_id));
                } else {
                    assert!(!worker.sn_tasks().contains(&task_id));
                }
            }
        };

        for (worker_id, worker) in self.workers.iter() {
            assert_eq!(worker.id, *worker_id);
            if worker.is_parked() {
                assert!(self.parked_resources.contains(&worker.resources));
            }
            worker.sanity_check(&self.tasks);
        }

        for task_id in self.tasks.task_ids() {
            let task = self.get_task(task_id);
            assert_eq!(task.id, task_id);
            match &task.state {
                TaskRuntimeState::Waiting(winfo) => {
                    let mut count = 0;
                    for task_dep in &task.task_deps {
                        if !self.tasks.get_task(*task_dep).is_finished() {
                            count += 1;
                        }
                    }
                    for &task_id in task.get_consumers() {
                        assert!(self.tasks.get_task(task_id).is_waiting());
                    }
                    assert_eq!(winfo.unfinished_deps, count);
                    worker_check_sn(self, task.id, 0.into());
                    assert!(task.is_fresh());
                }

                TaskRuntimeState::Assigned(wid)
                | TaskRuntimeState::Running { worker_id: wid, .. } => {
                    assert!(!task.is_fresh());
                    fw_check(task);
                    worker_check_sn(self, task.id, *wid);
                }

                TaskRuntimeState::Stealing(_, target) => {
                    assert!(!task.is_fresh());
                    fw_check(task);
                    worker_check_sn(self, task.id, target.unwrap_or(WorkerId::new(0)));
                }

                TaskRuntimeState::Finished(_) => {
                    for task_dep in &task.task_deps {
                        assert!(self.tasks.get_task(*task_dep).is_finished());
                    }
                }
                TaskRuntimeState::RunningMultiNode(ws) => {
                    assert!(!ws.is_empty());
                    fw_check(task);
                    let mut set = Set::new();
                    for worker_id in ws {
                        assert!(self.workers.contains_key(worker_id));
                        assert!(set.insert(*worker_id));
                    }
                    for (worker_id, worker) in self.workers.iter() {
                        if set.contains(worker_id) {
                            assert!(worker.sn_tasks().is_empty());
                            let mn = worker.mn_task().unwrap();
                            assert_eq!(mn.task_id, task_id);
                            assert_eq!(ws[0] != *worker_id, mn.reservation_only);
                        } else {
                            assert!(!worker.sn_tasks().contains(&task_id));
                            if let Some(mn) = worker.mn_task() {
                                assert_ne!(mn.task_id, task_id);
                            }
                        }
                    }
                }
            }
        }
    }

    #[inline]
    pub fn get_or_create_resource_id(&mut self, name: &str) -> ResourceId {
        self.resource_map.get_or_allocate_id(name)
    }

    #[inline]
    pub fn create_resource_map(&self) -> ResourceMap {
        self.resource_map.create_map()
    }

    #[inline]
    pub fn resource_count(&self) -> usize {
        self.resource_map.resource_count()
    }

    pub fn secret_key(&self) -> &Option<Arc<SecretKey>> {
        &self.secret_key
    }
}

#[cfg(test)]
mod tests {
    use crate::internal::server::core::Core;
    use crate::internal::server::task::Task;
    use crate::internal::server::task::TaskRuntimeState;
    use crate::internal::server::worker::Worker;
    use crate::internal::server::workergroup::WorkerGroup;
    use crate::internal::tests::utils::task;
    use crate::{TaskId, WorkerId};

    impl Core {
        pub fn worker_group(&self, group_name: &str) -> Option<&WorkerGroup> {
            self.worker_groups.get(group_name)
        }

        pub fn get_read_to_assign(&self) -> &[TaskId] {
            &self.single_node_ready_to_assign
        }

        pub fn remove_from_ready_to_assign(&mut self, task_id: TaskId) {
            self.single_node_ready_to_assign.retain(|&id| id != task_id);
            self.multi_node_ready_to_assign.retain(|&id| id != task_id);
        }

        pub fn assert_task_condition<T: Copy + Into<TaskId>, F: Fn(&Task) -> bool>(
            &self,
            task_ids: &[T],
            op: F,
        ) {
            for task_id in task_ids {
                let task_id: TaskId = (*task_id).into();
                if !op(self.get_task(task_id)) {
                    panic!("Task {} does not satisfy the condition", task_id);
                }
            }
        }

        pub fn assert_worker_condition<W: Copy + Into<WorkerId>, F: Fn(&Worker) -> bool>(
            &self,
            worker_ids: &[W],
            op: F,
        ) {
            for worker_id in worker_ids {
                let worker_id: WorkerId = (*worker_id).into();
                if !op(self.get_worker_by_id_or_panic(worker_id)) {
                    panic!("Worker {} does not satisfy the condition", worker_id);
                }
            }
        }

        pub fn assert_waiting<T: Copy + Into<TaskId>>(&self, task_ids: &[T]) {
            self.assert_task_condition(task_ids, |t| t.is_waiting());
        }

        pub fn assert_ready<T: Copy + Into<TaskId>>(&self, task_ids: &[T]) {
            self.assert_task_condition(task_ids, |t| t.is_ready());
        }

        pub fn assert_assigned<T: Copy + Into<TaskId>>(&self, task_ids: &[T]) {
            self.assert_task_condition(task_ids, |t| t.is_assigned());
        }

        pub fn assert_fresh<T: Copy + Into<TaskId>>(&self, task_ids: &[T]) {
            self.assert_task_condition(task_ids, |t| t.is_fresh());
        }

        pub fn assert_not_fresh<T: Copy + Into<TaskId>>(&self, task_ids: &[T]) {
            self.assert_task_condition(task_ids, |t| !t.is_fresh());
        }

        pub fn assert_running<T: Copy + Into<TaskId>>(&self, task_ids: &[T]) {
            self.assert_task_condition(task_ids, |t| t.is_sn_running());
        }

        pub fn assert_underloaded<W: Copy + Into<WorkerId>>(&self, worker_ids: &[W]) {
            self.assert_worker_condition(worker_ids, |w| w.is_underloaded());
        }

        pub fn assert_not_underloaded<W: Copy + Into<WorkerId>>(&self, worker_ids: &[W]) {
            self.assert_worker_condition(worker_ids, |w| !w.is_underloaded());
        }

        pub fn task<T: Into<TaskId>>(&self, id: T) -> &Task {
            self.tasks.get_task(id.into())
        }
    }

    #[test]
    fn add_remove() {
        let mut core = Core::default();
        let t = task::task(101);
        core.add_task(t);
        assert!(matches!(
            core.remove_task(101.into()),
            TaskRuntimeState::Waiting(_)
        ));
        assert_eq!(core.find_task(101.into()), None);
    }
}
