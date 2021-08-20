use crate::client::globalsettings::GlobalSettings;
use crate::client::job::{get_worker_map, print_job_detail, print_job_list};
use crate::client::status::{job_status, Status};
use crate::rpc_call;
use crate::transfer::connection::ClientConnection;
use crate::transfer::messages::{
    CancelJobResponse, CancelRequest, FromClientMessage, JobDetailRequest, JobInfoRequest,
    Selector, ToClientMessage,
};
use crate::JobId;

pub async fn get_last_job_id(connection: &mut ClientConnection) -> crate::Result<Option<JobId>> {
    let message = FromClientMessage::JobInfo(JobInfoRequest {
        selector: Selector::LastN(1),
    });
    let response = rpc_call!(connection, message, ToClientMessage::JobInfoResponse(r) => r).await?;

    Ok(response.jobs.last().map(|job| job.id))
}

pub async fn get_job_ids(connection: &mut ClientConnection) -> crate::Result<Option<Vec<JobId>>> {
    let message = FromClientMessage::JobInfo(JobInfoRequest {
        selector: Selector::All,
    });
    let response = rpc_call!(connection, message, ToClientMessage::JobInfoResponse(r) => r).await?;

    let mut ids: Vec<JobId> = Vec::new();
    for job in response.jobs {
        ids.push(job.id);
    }
    Ok(Option::from(ids))
}

pub async fn output_job_list(
    gsettings: &GlobalSettings,
    connection: &mut ClientConnection,
    job_filters: Vec<Status>,
) -> anyhow::Result<()> {
    let message = FromClientMessage::JobInfo(JobInfoRequest {
        selector: Selector::All,
    });
    let mut response =
        rpc_call!(connection, message, ToClientMessage::JobInfoResponse(r) => r).await?;

    if !job_filters.is_empty() {
        response
            .jobs
            .retain(|j| job_filters.contains(&job_status(j)));
    }
    response.jobs.sort_unstable_by_key(|j| j.id);
    print_job_list(gsettings, response.jobs);
    Ok(())
}

pub async fn output_job_detail(
    gsettings: &GlobalSettings,
    connection: &mut ClientConnection,
    selector: Selector,
    show_tasks: bool,
) -> anyhow::Result<()> {
    if matches!(selector, Selector::All) {
        log::warn!("Job detail doesn't support --all specifier, did you mean: job list?");
        return Ok(());
    };

    let message = FromClientMessage::JobDetail(JobDetailRequest {
        selector,
        include_tasks: true,
    });
    let responses =
        rpc_call!(connection, message, ToClientMessage::JobDetailResponse(r) => r).await?;

    for response in responses {
        if let Some(job) = response.1 {
            print_job_detail(
                gsettings,
                job,
                false,
                show_tasks,
                get_worker_map(connection).await?,
            );
        } else {
            log::error!("Job {} not found", response.0);
        }
    }
    Ok(())
}

pub async fn cancel_job(
    _gsettings: &GlobalSettings,
    connection: &mut ClientConnection,
    selector: Selector,
) -> anyhow::Result<()> {
    let mut responses = rpc_call!(connection, FromClientMessage::Cancel(CancelRequest {
         selector,
    }), ToClientMessage::CancelJobResponse(r) => r)
    .await?;
    responses.sort_unstable_by_key(|x| x.0);

    if responses.is_empty() {
        log::info!("There is nothing to cancel")
    }

    for (job_id, response) in responses {
        match response {
            CancelJobResponse::Canceled(canceled, already_finished) if !canceled.is_empty() => {
                log::info!(
                    "Job {} canceled ({} tasks canceled, {} tasks already finished)",
                    job_id,
                    canceled.len(),
                    already_finished
                )
            }
            CancelJobResponse::Canceled(_, _) => {
                log::error!(
                    "Canceling job {} failed; all tasks are already finished",
                    job_id
                )
            }
            CancelJobResponse::InvalidJob => {
                log::error!("Canceling job {} failed; job not found", job_id)
            }
            CancelJobResponse::Failed(msg) => {
                log::error!("Canceling job {} failed; {}", job_id, msg)
            }
        }
    }
    Ok(())
}
