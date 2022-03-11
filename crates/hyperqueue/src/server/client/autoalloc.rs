use crate::common::manager::info::ManagerType;
use crate::common::serverdir::ServerDir;
use crate::server::autoalloc::{
    prepare_descriptor_cleanup, Allocation, AllocationStatus, DescriptorId, PbsHandler,
    QueueDescriptor, QueueHandler, QueueInfo, RateLimiter, SlurmHandler, SubmitMode,
};
use crate::server::state::StateRef;
use crate::transfer::messages::{
    AllocationQueueParams, AutoAllocListResponse, AutoAllocRequest, AutoAllocResponse,
    QueueDescriptorData, ToClientMessage,
};
use anyhow::Context;
use chrono::{NaiveTime, Timelike};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::time::{Duration, SystemTime};
use tempdir::TempDir;

pub async fn handle_autoalloc_message(
    server_dir: &ServerDir,
    state_ref: &StateRef,
    request: AutoAllocRequest,
) -> ToClientMessage {
    match request {
        AutoAllocRequest::List => {
            let state = state_ref.get();
            let autoalloc = state.get_autoalloc_state();
            ToClientMessage::AutoAllocResponse(AutoAllocResponse::List(AutoAllocListResponse {
                descriptors: autoalloc
                    .descriptors()
                    .map(|(id, descriptor)| {
                        (
                            id,
                            QueueDescriptorData {
                                info: descriptor.descriptor.info().clone(),
                                name: descriptor.descriptor.name().map(|v| v.to_string()),
                                manager_type: descriptor.descriptor.manager().clone(),
                            },
                        )
                    })
                    .collect(),
            }))
        }
        AutoAllocRequest::DryRun {
            manager,
            parameters,
        } => {
            if let Err(e) = try_submit_allocation(manager, parameters).await {
                ToClientMessage::Error(e.to_string())
            } else {
                ToClientMessage::AutoAllocResponse(AutoAllocResponse::DryRunSuccessful)
            }
        }
        AutoAllocRequest::AddQueue {
            manager,
            parameters,
            dry_run,
        } => {
            if dry_run {
                if let Err(e) = try_submit_allocation(manager.clone(), parameters.clone()).await {
                    return ToClientMessage::Error(e.to_string());
                }
            }

            create_queue(server_dir, state_ref, manager, parameters)
        }
        AutoAllocRequest::Events { descriptor } => get_event_log(state_ref, descriptor),
        AutoAllocRequest::Info { descriptor } => get_allocations(state_ref, descriptor),
        AutoAllocRequest::RemoveQueue { descriptor, force } => {
            remove_queue(state_ref, descriptor, force).await
        }
    }
}

async fn remove_queue(state_ref: &StateRef, id: DescriptorId, force: bool) -> ToClientMessage {
    let remove_alloc_fut = {
        let mut server_state = state_ref.get_mut();
        let descriptor_state = server_state
            .get_autoalloc_state_mut()
            .get_descriptor_mut(id);

        let fut = match descriptor_state {
            Some(state) => {
                let has_running_allocations =
                    state.all_allocations().any(|alloc| alloc.is_running());
                if has_running_allocations && !force {
                    return ToClientMessage::Error(
                        "Allocation queue has running jobs, so it will \
not be removed. Use `--force` if you want to remove the queue anyway"
                            .to_string(),
                    );
                }

                prepare_descriptor_cleanup(state)
            }
            None => return ToClientMessage::Error("Allocation queue not found".to_string()),
        };

        server_state.get_autoalloc_state_mut().remove_descriptor(id);
        fut
    };

    for (result, allocation_id) in futures::future::join_all(remove_alloc_fut).await {
        match result {
            Ok(_) => log::info!("Allocation {} was removed", allocation_id),
            Err(e) => log::error!("Failed to remove allocation {}: {:?}", allocation_id, e),
        }
    }

    state_ref
        .get_mut()
        .get_event_storage_mut()
        .on_allocation_queue_removed(id);

    ToClientMessage::AutoAllocResponse(AutoAllocResponse::QueueRemoved(id))
}

fn get_allocations(state_ref: &StateRef, descriptor: DescriptorId) -> ToClientMessage {
    let state = state_ref.get();
    let autoalloc = state.get_autoalloc_state();

    match autoalloc.get_descriptor(descriptor) {
        Some(descriptor) => ToClientMessage::AutoAllocResponse(AutoAllocResponse::Info(
            descriptor.all_allocations().cloned().collect(),
        )),
        None => ToClientMessage::Error(format!("Descriptor {} not found", descriptor)),
    }
}

fn get_event_log(state_ref: &StateRef, descriptor: DescriptorId) -> ToClientMessage {
    let state = state_ref.get();
    let autoalloc = state.get_autoalloc_state();

    match autoalloc.get_descriptor(descriptor) {
        Some(descriptor) => ToClientMessage::AutoAllocResponse(AutoAllocResponse::Events(
            descriptor.get_events().iter().cloned().collect(),
        )),
        None => ToClientMessage::Error(format!("Descriptor {} not found", descriptor)),
    }
}

// The code doesn't compile if the Box closures are removed
#[allow(clippy::redundant_closure)]
pub fn create_allocation_handler(
    manager: &ManagerType,
    name: Option<String>,
    directory: PathBuf,
) -> anyhow::Result<Box<dyn QueueHandler>> {
    match manager {
        ManagerType::Pbs => {
            let handler = PbsHandler::new(directory, name);
            handler.map::<Box<dyn QueueHandler>, _>(|handler| Box::new(handler))
        }
        ManagerType::Slurm => {
            let handler = SlurmHandler::new(directory, name);
            handler.map::<Box<dyn QueueHandler>, _>(|handler| Box::new(handler))
        }
    }
}

pub fn create_queue_info(params: AllocationQueueParams) -> QueueInfo {
    let AllocationQueueParams {
        name: _name,
        workers_per_alloc,
        backlog,
        timelimit,
        additional_args,
        worker_cpu_arg,
        worker_resources_args,
        max_worker_count,
        on_server_lost,
        max_kept_directories: _,
    } = params;
    QueueInfo::new(
        backlog,
        workers_per_alloc,
        timelimit,
        on_server_lost,
        additional_args,
        worker_cpu_arg,
        worker_resources_args,
        max_worker_count,
    )
}

/// Maximum number of successive allocation submission failures permitted
/// before the allocation queue will be removed.
const MAX_SUBMISSION_FAILS: usize = 10;
/// Maximum number of successive allocation execution failures permitted
/// before the allocation queue will be removed.
const MAX_ALLOCATION_FAILS: usize = 3;

fn create_rate_limiter() -> RateLimiter {
    RateLimiter::new(
        vec![
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::from_secs(15 * 60),
            Duration::from_secs(30 * 60),
            Duration::from_secs(60 * 60),
        ],
        MAX_SUBMISSION_FAILS,
        MAX_ALLOCATION_FAILS,
    )
}

fn create_queue(
    server_dir: &ServerDir,
    state_ref: &StateRef,
    manager: ManagerType,
    params: AllocationQueueParams,
) -> ToClientMessage {
    let server_directory = server_dir.directory().to_path_buf();
    let name = params.name.clone();
    let max_kept_directories = params.max_kept_directories;
    let handler = create_allocation_handler(&manager, name.clone(), server_directory);
    let queue_info = create_queue_info(params.clone());

    match handler {
        Ok(handler) => {
            let descriptor =
                QueueDescriptor::new(manager, queue_info, name, handler, max_kept_directories);
            let id = {
                let mut state = state_ref.get_mut();
                let id = state.get_autoalloc_state_mut().create_id();

                state.get_autoalloc_state_mut().add_descriptor(
                    id,
                    descriptor,
                    create_rate_limiter(),
                );
                state
                    .get_event_storage_mut()
                    .on_allocation_queue_created(id, params);
                id
            };

            ToClientMessage::AutoAllocResponse(AutoAllocResponse::QueueCreated(id))
        }
        Err(err) => ToClientMessage::Error(format!("Could not create autoalloc queue: {}", err)),
    }
}

struct QueueLimits {
    walltime: Option<Duration>,
    backlog: Option<u32>,
    workers: Option<u32>,
}

// Returns value of given arg, in formats: keyVALUE, key=VALUE, [key, VALUE]
fn get_arg_by_key(key: String, args: &[String]) -> Option<String> {
    let index_opt = args.iter().position(|r| r.contains(&key));

    if let Some(index) = index_opt {
        let mut value = args[index].clone();

        value = value.replace(&key, "");
        value = value.replace('=', "");

        return if !value.is_empty() {
            Some(value)
        } else {
            args.get(index + 1).cloned()
        };
    }
    None
}

// Returns value of given path in json, can include nests
fn get_value_by_key_json(
    json: &serde_json::Value,
    path: String,
) -> anyhow::Result<&serde_json::Value> {
    let json_keys = path.split('/');
    let mut value = json;
    for json_key in json_keys {
        value = value
            .get(json_key)
            .context(format!("key {json_key} not found"))?;
    }
    Ok(value)
}

fn get_pbs_queue_limit(queue_type: String) -> anyhow::Result<Option<QueueLimits>> {
    let queue_info = Command::new("qstat")
        .arg("-Q")
        .arg(&queue_type)
        .arg("-f")
        .arg("-F")
        .arg("json")
        .output()?;

    let output = String::from_utf8_lossy(&queue_info.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&output).context("JSON was not well-formatted")?;
    let queue = json
        .get("Queue")
        .and_then(|x| x.get(&queue_type))
        .context("can't find queue info")?;

    parse_pbs_queue_limit(queue)
}

fn parse_pbs_queue_limit(queue: &Value) -> anyhow::Result<Option<QueueLimits>> {
    let (walltime, backlog, workers_max, workers_available) = (
        "resources_max/walltime",
        "max_queued",
        "resources_max/nodect",
        "resources_available/nodes",
    );

    let walltime = match get_value_by_key_json(queue, String::from(walltime)) {
        Ok(value) => {
            let walltime = NaiveTime::from_str(value.as_str().context("can't convert str")?)?;
            let secs: u64 =
                (walltime.hour() * 3600 + walltime.minute() * 60 + walltime.second()) as u64;
            Some(Duration::from_secs(secs))
        }
        Err(_) => None,
    };

    let backlog = match get_value_by_key_json(queue, String::from(backlog)) {
        Ok(value) => Some(
            value
                .to_string()
                .chars()
                .into_iter()
                .filter(|x| x.is_numeric())
                .collect::<String>()
                .parse()?,
        ),
        Err(_) => None,
    };

    let workers: Option<u32> = {
        let possible_workers = match get_value_by_key_json(queue, String::from(workers_max)) {
            Ok(max) => Some(max),
            Err(_) => match get_value_by_key_json(queue, String::from(workers_available)) {
                Ok(available) => Some(available),
                Err(_) => None,
            },
        };

        match possible_workers {
            None => None,
            Some(value) => Some(
                value
                    .to_string()
                    .chars()
                    .into_iter()
                    .filter(|x| x.is_numeric())
                    .collect::<String>()
                    .parse()?,
            ),
        }
    };
    Ok(Option::from(QueueLimits {
        walltime,
        backlog,
        workers,
    }))
}

fn get_slurm_queue_limit(_args: &[String]) -> anyhow::Result<Option<QueueLimits>> {
    Ok(None)
}

pub fn validate_queue_parameters(
    info: &AllocationQueueParams,
    manager: &ManagerType,
) -> anyhow::Result<()> {
    let queue_limit = match manager {
        ManagerType::Pbs => {
            let opt_queue_type = get_arg_by_key(String::from("-q"), &info.additional_args);
            match opt_queue_type {
                Some(queue_type) => get_pbs_queue_limit(queue_type.replace("-q", "")),
                None => Ok(None),
            }
        }
        ManagerType::Slurm => get_slurm_queue_limit(&info.additional_args),
    };

    match queue_limit {
        Err(err) => Err(anyhow::anyhow!("Error while getting queue limit: {err}")),
        Ok(None) => Err(anyhow::anyhow!(
            "Queue limit wasn't retrieved, can't provide additional information."
        )),
        Ok(Some(limit)) => {
            let mut msg = String::new();

            if let Some(walltime) = limit.walltime {
                if walltime < info.timelimit {
                    msg.push_str(
                        "Walltime is larger than the walltime limit of the given queue.\n",
                    );
                }
            }
            if let Some(backlog) = limit.backlog {
                if backlog < info.backlog {
                    msg.push_str(
                        "Backlog size is larger than the max. queue limit of the given queue.\n",
                    );
                }
            }
            if let Some(workers) = limit.workers {
                if workers < info.workers_per_alloc {
                    msg.push_str("Workers per allocation is larger than the max. number of workers of the given queue.\n");
                }
            }

            if !msg.is_empty() {
                return Err(anyhow::anyhow!(
                    "Allocation parameters exceeded queue limits: ".to_owned() + &msg
                ));
            }
            Ok(())
        }
    }
}

async fn try_submit_allocation(
    manager: ManagerType,
    params: AllocationQueueParams,
) -> anyhow::Result<()> {
    let tmpdir = TempDir::new("hq")?;
    let mut handler =
        create_allocation_handler(&manager, params.name.clone(), tmpdir.as_ref().to_path_buf())?;
    let worker_count = params.workers_per_alloc;
    let queue_info = create_queue_info(params.clone());

    let allocation = handler
        .submit_allocation(0, &queue_info, worker_count as u64, SubmitMode::DryRun)
        .await
        .map_err(|e| anyhow::anyhow!("Could not submit allocation: {:?}", e))?;

    let working_dir = allocation.working_dir().to_path_buf();
    let id = match allocation.into_id() {
        Ok(result) => result,
        Err(basic_error) => {
            return {
                match validate_queue_parameters(&params, &manager) {
                    Ok(_) => Err(anyhow::anyhow!(
                        "Could not submit allocation: {:?}",
                        basic_error
                    )),
                    Err(additional_error) => Err(anyhow::anyhow!(
                        "Could not submit allocation: {:?} \nAdditional errors: {:?}",
                        basic_error,
                        additional_error
                    )),
                }
            }
        }
    };

    let allocation = Allocation {
        id: id.to_string(),
        worker_count: 1,
        queued_at: SystemTime::now(),
        status: AllocationStatus::Queued,
        working_dir,
    };
    handler
        .remove_allocation(&allocation)
        .await
        .map_err(|e| anyhow::anyhow!("Could not cancel allocation {}: {:?}", allocation.id, e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::server::client::autoalloc::{get_arg_by_key, parse_pbs_queue_limit};
    use std::time::Duration;

    #[test]
    fn test_get_arg_by_key() {
        assert_eq!(
            get_arg_by_key(
                String::from("-q"),
                &[String::from("x"), String::from("-qqexp"), String::from("y")]
            )
            .is_some(),
            true
        );
        assert_eq!(
            get_arg_by_key(
                String::from("-q"),
                &[
                    String::from("x"),
                    String::from("-q=qexp"),
                    String::from("y")
                ]
            )
            .is_some(),
            true
        );
        assert_eq!(
            get_arg_by_key(
                String::from("-q"),
                &[
                    String::from("x"),
                    String::from("-q"),
                    String::from("qexp"),
                    String::from("y")
                ]
            )
            .is_some(),
            true
        );
    }

    #[test]
    fn test_get_pbs_queue_limit() {
        let queue_json = r#"{
    "timestamp":1646895019,
    "pbs_version":"20.0.1",
    "pbs_server":"isrv1.barbora.it4i.cz",
    "Queue":{
        "qexp":{
            "queue_type":"Execution",
            "Priority":150,
            "total_jobs":2,
            "state_count":"Transit:0 Queued:0 Held:2 Waiting:0 Running:0 Exiting:0 Begun:0 ",
            "max_queued":"[u:PBS_GENERIC=5]",
            "resources_max":{
                "ncpus":144,
                "nodect":4,
                "walltime":"01:00:00"
            },
            "resources_default":{
                "ncpus":36,
                "walltime":"01:00:00"
            },
            "default_chunk":{
                "ncpus":36,
                "Qlist":"qexp"
            },
            "resources_available":{
                "ncpus":576,
                "nodect":16
            },
            "resources_assigned":{
                "mem":"0kb",
                "mpiprocs":0,
                "ncpus":0,
                "nodect":0
            },
            "max_run":"[u:PBS_GENERIC=1]",
            "max_run_res":{
                "ncpus":"[u:PBS_GENERIC=144]",
                "nodect":"[u:PBS_GENERIC=4]"
            },
            "enabled":"True",
            "started":"True"
        }
    }
}"#;
        // Get queue info
        let json: serde_json::Value = serde_json::from_str(&queue_json).unwrap();
        let queue = json
            .get("Queue")
            .and_then(|x| x.get(&String::from("qexp")))
            .unwrap();

        // Check values
        let result = parse_pbs_queue_limit(queue);
        match result {
            Ok(Some(limit)) => {
                // Max-queued
                assert_eq!(limit.backlog == Some(5), true);
                // resources_max/nodect for this queue
                assert_eq!(limit.workers == Some(4), true);
                // resources_max/walltime
                assert_eq!(limit.walltime == Some(Duration::from_secs(3600)), true);
            }
            _ => assert!(false),
        }
    }
}
