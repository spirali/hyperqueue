use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bstr::BString;
use tako::datasrv::DataObjectId;
use tako::gateway::{
    FromGatewayMessage, NewTasksMessage, ResourceRequestVariants, SharedTaskConfiguration,
    TaskConfiguration, TaskDataFlags, ToGatewayMessage,
};
use tako::Map;
use tako::Set;
use thin_vec::ThinVec;

use crate::common::arraydef::IntArray;
use crate::common::placeholders::{
    fill_placeholders_after_submit, fill_placeholders_log, normalize_path,
};
use crate::server::job::Job;
use crate::server::state::{State, StateRef};
use crate::server::Senders;
use crate::transfer::messages::{
    JobDescription, JobSubmitDescription, JobTaskDescription, OpenJobResponse, SubmitRequest,
    SubmitResponse, TaskBuildDescription, TaskDescription, TaskIdSelector, TaskKind,
    TaskKindProgram, TaskSelector, TaskStatusSelector, TaskWithDependencies, ToClientMessage,
};
use crate::{make_tako_id, JobId, JobTaskCount, JobTaskId, Priority, TakoTaskId};

fn create_new_task_message(
    job_id: JobId,
    submit_desc: &mut JobSubmitDescription,
) -> NewTasksMessage {
    match &mut submit_desc.task_desc {
        JobTaskDescription::Array {
            ids,
            entries,
            task_desc,
        } => build_tasks_array(
            job_id,
            ids,
            std::mem::take(entries),
            task_desc,
            &submit_desc.submit_dir,
            submit_desc.stream_path.as_ref(),
        ),
        JobTaskDescription::Graph { tasks } => build_tasks_graph(
            job_id,
            tasks,
            &submit_desc.submit_dir,
            submit_desc.stream_path.as_ref(),
        ),
    }
}

pub(crate) fn submit_job_desc(
    state: &mut State,
    job_id: JobId,
    mut submit_desc: JobSubmitDescription,
) -> NewTasksMessage {
    prepare_job(job_id, &mut submit_desc, state);
    let new_tasks = create_new_task_message(job_id, &mut submit_desc);
    submit_desc.strip_large_data();
    state
        .get_job_mut(job_id)
        .unwrap()
        .attach_submit(Arc::new(submit_desc));
    new_tasks
}

pub(crate) fn validate_submit(
    job: Option<&Job>,
    task_desc: &JobTaskDescription,
) -> Option<SubmitResponse> {
    match &task_desc {
        JobTaskDescription::Array { ids, .. } => {
            if let Some(job) = job {
                for id in ids.iter() {
                    let id = JobTaskId::new(id);
                    if job.tasks.contains_key(&id) {
                        return Some(SubmitResponse::TaskIdAlreadyExists(id));
                    }
                }
            }
        }
        JobTaskDescription::Graph { tasks } => {
            if let Some(job) = job {
                for task in tasks {
                    if job.tasks.contains_key(&task.id) {
                        let id = task.id;
                        return Some(SubmitResponse::TaskIdAlreadyExists(id));
                    }
                }
            }
            let mut task_ids = Set::new();
            for task in tasks {
                if !task_ids.insert(task.id) {
                    return Some(SubmitResponse::NonUniqueTaskId(task.id));
                }
            }
            for task in tasks {
                for dep_id in &task.task_deps {
                    if *dep_id == task.id
                        || (!task_ids.contains(dep_id)
                            && !job
                                .map(|job| job.tasks.contains_key(dep_id))
                                .unwrap_or(false))
                    {
                        return Some(SubmitResponse::InvalidDependencies(*dep_id));
                    }
                }
            }
        }
    }
    None
}

#[allow(clippy::await_holding_refcell_ref)] // Disable lint as it does not work well with drop
pub(crate) async fn handle_submit(
    state_ref: &StateRef,
    senders: &Senders,
    mut message: SubmitRequest,
) -> ToClientMessage {
    let mut state = state_ref.get_mut();
    if let Some(err) = validate_submit(
        message.job_id.and_then(|job_id| state.get_job(job_id)),
        &message.submit_desc.task_desc,
    ) {
        return ToClientMessage::SubmitResponse(err);
    }

    let (job_id, new_job) = if let Some(job_id) = message.job_id {
        if let Some(job) = state.get_job(job_id) {
            if !job.is_open() {
                return ToClientMessage::SubmitResponse(SubmitResponse::JobNotOpened);
            }
            match &mut message.submit_desc.task_desc {
                JobTaskDescription::Array { ids, entries, .. } => {
                    if ids.is_empty() {
                        let new_id = job.max_id().map(|x| x.as_num() + 1).unwrap_or(0);
                        if let Some(entries) = entries {
                            *ids =
                                IntArray::from_range(new_id, new_id + entries.len() as JobTaskCount)
                        } else {
                            *ids = IntArray::from_id(new_id)
                        }
                    }
                }
                JobTaskDescription::Graph { .. } => {}
            }
        } else {
            return ToClientMessage::SubmitResponse(SubmitResponse::JobNotFound);
        }
        (job_id, false)
    } else {
        match &mut message.submit_desc.task_desc {
            JobTaskDescription::Array { ids, entries, .. } => {
                /* Try fillin task ids */
                if ids.is_empty() {
                    if let Some(entries) = entries {
                        *ids = IntArray::from_range(0, entries.len() as JobTaskCount)
                    } else {
                        *ids = IntArray::from_id(0)
                    }
                }
            }
            JobTaskDescription::Graph { .. } => { /* Do nothing */ }
        }
        (state.new_job_id(), true)
    };

    senders.events.on_job_submitted(job_id, &message).unwrap();

    let SubmitRequest {
        job_desc,
        submit_desc,
        job_id: _,
    } = message;

    if new_job {
        let job = Job::new(job_id, job_desc, false);
        state.add_job(job);
    }

    let new_tasks = submit_job_desc(&mut state, job_id, submit_desc);
    senders.autoalloc.on_job_created(job_id);

    let job_detail = state
        .get_job(job_id)
        .unwrap()
        .make_job_detail(Some(&TaskSelector {
            id_selector: TaskIdSelector::All,
            status_selector: TaskStatusSelector::All,
        }));
    drop(state);

    match senders
        .backend
        .send_tako_message(FromGatewayMessage::NewTasks(new_tasks))
        .await
        .unwrap()
    {
        ToGatewayMessage::NewTasksResponse(_) => { /* Ok */ }
        r => panic!("Invalid response: {r:?}"),
    };

    ToClientMessage::SubmitResponse(SubmitResponse::Ok {
        job: job_detail,
        server_uid: state_ref.get().server_info().server_uid.clone(),
    })
}

/// Prefills placeholders in the submit request and creates job ID
fn prepare_job(job_id: JobId, submit_desc: &mut JobSubmitDescription, state: &mut State) {
    // Prefill currently known placeholders eagerly
    if let JobTaskDescription::Array {
        ref mut task_desc, ..
    } = submit_desc.task_desc
    {
        match &mut task_desc.kind {
            TaskKind::ExternalProgram(TaskKindProgram { program, .. }) => {
                fill_placeholders_after_submit(
                    program,
                    job_id,
                    &submit_desc.submit_dir,
                    &state.server_info().server_uid,
                );
            }
        }
    };

    if let Some(path) = &mut submit_desc.stream_path {
        fill_placeholders_log(
            path,
            job_id,
            &submit_desc.submit_dir,
            &state.server_info().server_uid,
        );
        *path = normalize_path(path.as_path(), &submit_desc.submit_dir);
    }
}

fn serialize_task_body(
    job_id: JobId,
    task_id: JobTaskId,
    entry: Option<BString>,
    task_desc: &TaskDescription,
    submit_dir: &PathBuf,
    stream_path: Option<&PathBuf>,
) -> Box<[u8]> {
    let body_msg = TaskBuildDescription {
        task_kind: Cow::Borrowed(&task_desc.kind),
        job_id,
        task_id,
        submit_dir: Cow::Borrowed(submit_dir),
        stream_path: stream_path.map(Cow::Borrowed),
        entry,
    };
    let body = tako::comm::serialize(&body_msg).expect("Could not serialize task body");
    // Make sure that `into_boxed_slice` is a no-op.
    debug_assert_eq!(body.capacity(), body.len());
    body.into_boxed_slice()
}

fn build_tasks_array(
    job_id: JobId,
    ids: &IntArray,
    entries: Option<Vec<BString>>,
    task_desc: &TaskDescription,
    submit_dir: &PathBuf,
    stream_path: Option<&PathBuf>,
) -> NewTasksMessage {
    let build_task_conf = |body: Box<[u8]>, tako_id: TakoTaskId| TaskConfiguration {
        id: tako_id,
        shared_data_index: 0,
        task_deps: ThinVec::new(),
        dataobj_deps: ThinVec::new(),
        body,
    };

    let tasks = match entries {
        None => ids
            .iter()
            .map(|task_id| {
                build_task_conf(
                    serialize_task_body(
                        job_id,
                        task_id.into(),
                        None,
                        task_desc,
                        submit_dir,
                        stream_path,
                    ),
                    make_tako_id(job_id, task_id.into()),
                )
            })
            .collect(),
        Some(entries) => ids
            .iter()
            .zip(entries)
            .map(|(task_id, entry)| {
                build_task_conf(
                    serialize_task_body(
                        job_id,
                        task_id.into(),
                        Some(entry),
                        task_desc,
                        submit_dir,
                        stream_path,
                    ),
                    make_tako_id(job_id, task_id.into()),
                )
            })
            .collect(),
    };

    NewTasksMessage {
        tasks,
        shared_data: vec![SharedTaskConfiguration {
            resources: task_desc.resources.clone(),
            time_limit: task_desc.time_limit,
            priority: task_desc.priority,
            crash_limit: task_desc.crash_limit,
            data_flags: TaskDataFlags::empty(),
        }],
        adjust_instance_id: Default::default(),
    }
}

fn build_tasks_graph(
    job_id: JobId,
    tasks: &[TaskWithDependencies],
    submit_dir: &PathBuf,
    stream_path: Option<&PathBuf>,
) -> NewTasksMessage {
    let mut shared_data = vec![];
    let mut shared_data_map =
        Map::<(Cow<ResourceRequestVariants>, Option<Duration>, Priority), usize>::new();
    let mut allocate_shared_data = |task: &TaskDescription, data_flags: TaskDataFlags| -> u32 {
        shared_data_map
            .get(&(
                Cow::Borrowed(&task.resources),
                task.time_limit,
                task.priority,
            ))
            .copied()
            .unwrap_or_else(|| {
                let index = shared_data.len();
                shared_data_map.insert(
                    (
                        Cow::Owned(task.resources.clone()),
                        task.time_limit,
                        task.priority,
                    ),
                    index,
                );
                shared_data.push(SharedTaskConfiguration {
                    resources: task.resources.clone(),
                    time_limit: task.time_limit,
                    priority: task.priority,
                    crash_limit: task.crash_limit,
                    data_flags,
                });
                index
            }) as u32
    };

    let mut task_configs = Vec::with_capacity(tasks.len());
    for task in tasks {
        let body = serialize_task_body(
            job_id,
            task.id,
            None,
            &task.task_desc,
            submit_dir,
            stream_path,
        );
        let shared_data_index = allocate_shared_data(&task.task_desc, task.data_flags);

        let task_deps = task
            .task_deps
            .iter()
            .map(|task_id| make_tako_id(job_id, *task_id))
            .collect();

        let dataobj_deps = task
            .dataobj_deps
            .iter()
            .map(|job_do_id| job_do_id.to_dataobj_id(job_id))
            .collect();

        task_configs.push(TaskConfiguration {
            id: make_tako_id(job_id, task.id),
            shared_data_index,
            task_deps,
            dataobj_deps,
            body,
        });
    }

    NewTasksMessage {
        tasks: task_configs,
        shared_data,
        adjust_instance_id: Default::default(),
    }
}

pub(crate) fn handle_open_job(
    state_ref: &StateRef,
    senders: &Senders,
    job_description: JobDescription,
) -> ToClientMessage {
    let job_id = state_ref.get_mut().new_job_id();
    senders
        .events
        .on_job_opened(job_id, job_description.clone());
    let job = Job::new(job_id, job_description, true);
    state_ref.get_mut().add_job(job);
    ToClientMessage::OpenJobResponse(OpenJobResponse { job_id })
}

#[cfg(test)]
mod tests {
    use crate::common::arraydef::IntArray;
    use crate::make_tako_id;
    use crate::server::client::submit::build_tasks_graph;
    use crate::server::client::validate_submit;
    use crate::server::job::Job;
    use crate::transfer::messages::{
        JobDescription, JobSubmitDescription, JobTaskDescription, PinMode, SubmitResponse,
        TaskDescription, TaskKind, TaskKindProgram, TaskWithDependencies,
    };
    use smallvec::smallvec;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tako::gateway::{
        NewTasksMessage, ResourceRequest, ResourceRequestEntry, ResourceRequestVariants,
    };
    use tako::program::ProgramDefinition;
    use tako::resources::{AllocationRequest, ResourceAmount, CPU_RESOURCE_NAME};
    use tako::Priority;

    #[test]
    fn test_build_graph_deduplicate_shared_confs() {
        let desc_a = || task_desc(None, 0, 1);
        let desc_b = || task_desc(Some(Duration::default()), 0, 1);
        let desc_c = || task_desc(None, 0, 3);

        let tasks = vec![
            task(0, desc_a(), vec![]),
            task(1, desc_c(), vec![]),
            task(2, desc_a(), vec![]),
            task(3, desc_b(), vec![]),
            task(4, desc_a(), vec![]),
        ];

        let msg = build_tasks_graph(1.into(), &tasks, &PathBuf::from("foo"), None);

        check_shared_data(&msg, vec![desc_a(), desc_c(), desc_b()]);
        assert_eq!(
            msg.tasks
                .into_iter()
                .map(|t| t.shared_data_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 0, 2, 0]
        );
    }

    #[test]
    fn test_validate_submit() {
        let mut job = Job::new(
            10.into(),
            JobDescription {
                name: "".to_string(),
                max_fails: None,
            },
            true,
        );
        job.attach_submit(Arc::new(JobSubmitDescription {
            task_desc: JobTaskDescription::Array {
                ids: IntArray::from_range(100, 10),
                entries: None,
                task_desc: task_desc(None, 0, 1),
            },
            submit_dir: Default::default(),
            stream_path: None,
        }));

        let job_task_desc = JobTaskDescription::Array {
            ids: IntArray::from_range(109, 2),
            entries: None,
            task_desc: task_desc(None, 0, 1),
        };
        assert!(validate_submit(None, &job_task_desc).is_none());
        assert!(matches!(
            validate_submit(Some(&job), &job_task_desc),
            Some(SubmitResponse::TaskIdAlreadyExists(x)) if x.as_num() == 109
        ));
        let job_task_desc = JobTaskDescription::Graph {
            tasks: vec![TaskWithDependencies {
                id: 102.into(),
                task_desc: task_desc(None, 0, 1),
                dependencies: vec![],
            }],
        };
        assert!(validate_submit(None, &job_task_desc).is_none());
        assert!(matches!(
            validate_submit(Some(&job), &job_task_desc),
            Some(SubmitResponse::TaskIdAlreadyExists(x)) if x.as_num() == 102
        ));
        let job_task_desc = JobTaskDescription::Graph {
            tasks: vec![
                TaskWithDependencies {
                    id: 2.into(),
                    task_desc: task_desc(None, 0, 1),
                    dependencies: vec![],
                },
                TaskWithDependencies {
                    id: 2.into(),
                    task_desc: task_desc(None, 0, 1),
                    dependencies: vec![],
                },
            ],
        };
        assert!(matches!(
            validate_submit(None, &job_task_desc),
            Some(SubmitResponse::NonUniqueTaskId(x)) if x.as_num() == 2
        ));
        let job_task_desc = JobTaskDescription::Graph {
            tasks: vec![TaskWithDependencies {
                id: 2.into(),
                task_desc: task_desc(None, 0, 1),
                dependencies: vec![3.into()],
            }],
        };
        assert!(matches!(
            validate_submit(None, &job_task_desc),
            Some(SubmitResponse::InvalidDependencies(x)) if x.as_num() == 3
        ));
        let job_task_desc = JobTaskDescription::Graph {
            tasks: vec![TaskWithDependencies {
                id: 2.into(),
                task_desc: task_desc(None, 0, 1),
                dependencies: vec![2.into()],
            }],
        };
        assert!(matches!(
            validate_submit(None, &job_task_desc),
            Some(SubmitResponse::InvalidDependencies(x)) if x.as_num() == 2
        ));
    }

    #[test]
    fn test_build_graph_with_dependencies() {
        let desc = || task_desc(None, 0, 1);
        let tasks = vec![
            task(0, desc(), vec![2, 1]),
            task(1, desc(), vec![0]),
            task(2, desc(), vec![3, 4]),
            task(3, desc(), vec![]),
            task(4, desc(), vec![0]),
        ];

        let msg = build_tasks_graph(1.into(), &tasks, &PathBuf::from("foo"), None);
        assert_eq!(
            msg.tasks[0].task_deps,
            vec![
                make_tako_id(1.into(), 2.into()),
                make_tako_id(1.into(), 1.into())
            ]
        );
        assert_eq!(
            msg.tasks[1].task_deps,
            vec![make_tako_id(1.into(), 0.into())]
        );
        assert_eq!(
            msg.tasks[2].task_deps,
            vec![
                make_tako_id(1.into(), 3.into()),
                make_tako_id(1.into(), 4.into())
            ]
        );
        assert_eq!(msg.tasks[3].task_deps, vec![]);
        assert_eq!(
            msg.tasks[4].task_deps,
            vec![make_tako_id(1.into(), 0.into()),]
        );
    }

    fn check_shared_data(msg: &NewTasksMessage, expected: Vec<TaskDescription>) {
        assert_eq!(msg.shared_data.len(), expected.len());
        for (shared, expected) in msg.shared_data.iter().zip(expected) {
            assert_eq!(shared.resources, expected.resources);
            assert_eq!(shared.time_limit, expected.time_limit);
            assert_eq!(shared.priority, expected.priority);
        }
    }

    fn task_desc(
        time_limit: Option<Duration>,
        priority: Priority,
        cpu_count: u32,
    ) -> TaskDescription {
        TaskDescription {
            kind: TaskKind::ExternalProgram(TaskKindProgram {
                program: ProgramDefinition {
                    args: vec![],
                    env: Default::default(),
                    stdout: Default::default(),
                    stderr: Default::default(),
                    stdin: vec![],
                    cwd: Default::default(),
                },
                pin_mode: PinMode::None,
                task_dir: false,
            }),
            resources: ResourceRequestVariants::new_simple(ResourceRequest {
                n_nodes: 0,
                min_time: Duration::from_secs(2),
                resources: smallvec![ResourceRequestEntry {
                    resource: CPU_RESOURCE_NAME.to_string(),
                    policy: AllocationRequest::Compact(ResourceAmount::new_units(cpu_count)),
                }],
            }),
            time_limit,
            priority,
            crash_limit: 5,
        }
    }

    fn task(id: u32, task_desc: TaskDescription, dependencies: Vec<u32>) -> TaskWithDependencies {
        TaskWithDependencies {
            id: id.into(),
            task_desc,
            dependencies: dependencies.into_iter().map(|id| id.into()).collect(),
        }
    }
}
