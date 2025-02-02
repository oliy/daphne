// Copyright (c) 2022 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use daphne::{
    constants::DapMediaType,
    error::DapAbort,
    messages::{CollectionJobId, TaskId},
    roles::DapLeader,
    DapCollectJob, DapResponse, DapVersion,
};
use prio::codec::ParameterizedEncode;
use tracing::{info_span, Instrument};
use worker::{Headers, Request, Response, Result, RouteContext};

use crate::{config::DaphneWorkerRequestState, info_span_from_dap_request};

use super::{dap_response_to_worker, DapRouter};

pub(super) fn add_leader_routes(router: DapRouter<'_>) -> DapRouter<'_> {
    router
        .post_async("/v02/upload", put_report_into_task) // draft02
        .put_async("/:version/tasks/:task_id/reports", put_report_into_task)
        .post_async("/v02/collect", |req, ctx| async move {
            let daph = ctx.data.handler(&ctx.env);
            let req = daph.worker_request_to_dap(req, &ctx).await?;

            let span = info_span_from_dap_request!("collect", req);

            match daph.handle_collect_job_req(&req).instrument(span).await {
                Ok(collect_uri) => {
                    let mut headers = Headers::new();
                    headers.set("Location", collect_uri.as_str())?;
                    Ok(Response::empty()
                        .unwrap()
                        .with_status(303)
                        .with_headers(headers))
                }
                Err(e) => daph.state.dap_abort_to_worker_response(e),
            }
        }) // draft02
        .get_async(
            "/v02/collect/task/:task_id/req/:collect_id",
            |req, ctx| async move {
                let task_id = match ctx.param("task_id").and_then(TaskId::try_from_base64url) {
                    Some(id) => id,
                    None => {
                        return ctx.data.dap_abort_to_worker_response(DapAbort::BadRequest(
                            "missing task_id parameter".to_string(),
                        ))
                    }
                };
                let collect_id = match ctx
                    .param("collect_id")
                    .and_then(CollectionJobId::try_from_base64url)
                {
                    Some(id) => id,
                    None => {
                        return ctx.data.dap_abort_to_worker_response(DapAbort::BadRequest(
                            "missing collect_id parameter".to_string(),
                        ))
                    }
                };
                let daph = ctx.data.handler(&ctx.env);
                let version = daph.extract_version_parameter(&req)?;
                match daph
                    .poll_collect_job(&task_id, &collect_id)
                    .instrument(info_span!("poll_collect_job (draft02)"))
                    .await
                {
                    Ok(DapCollectJob::Done(collect_resp)) => dap_response_to_worker(DapResponse {
                        version: DapVersion::Draft02,
                        media_type: DapMediaType::Collection,
                        payload: collect_resp.get_encoded_with_param(&version),
                    }),
                    Ok(DapCollectJob::Pending) => Ok(Response::empty().unwrap().with_status(202)),
                    // TODO spec: Decide whether to define this behavior.
                    Ok(DapCollectJob::Unknown) => {
                        daph.state
                            .dap_abort_to_worker_response(DapAbort::BadRequest(
                                "unknown collect id".into(),
                            ))
                    }
                    Err(e) => daph.state.dap_abort_to_worker_response(e.into()),
                }
            },
        ) // draft02
        .put_async(
            "/:version/tasks/:task_id/collection_jobs/:collect_job_id",
            |req, ctx| async move {
                let daph = ctx.data.handler(&ctx.env);
                let req = daph.worker_request_to_dap(req, &ctx).await?;

                let span = info_span_from_dap_request!("collect (PUT)", req);

                match daph.handle_collect_job_req(&req).instrument(span).await {
                    Ok(_) => Ok(Response::empty().unwrap().with_status(201)),
                    Err(e) => daph.state.dap_abort_to_worker_response(e),
                }
            },
        )
        .post_async(
            "/:version/tasks/:task_id/collection_jobs/:collect_job_id",
            |req, ctx| async move {
                let daph = ctx.data.handler(&ctx.env);
                let req = daph.worker_request_to_dap(req, &ctx).await?;
                let task_id = match req.task_id() {
                    Ok(id) => id,
                    Err(e) => return daph.state.dap_abort_to_worker_response(e),
                };
                // We cannot check a resource here as the resource is set via
                // media type, and there is no media type when polling.
                //
                // We can unwrap() here as the parameter really must exist.
                let collect_job_id_base64url = ctx.param("collect_job_id").unwrap();
                let collect_job_id =
                    match CollectionJobId::try_from_base64url(collect_job_id_base64url) {
                        Some(id) => id,
                        None => {
                            return daph
                                .state
                                .dap_abort_to_worker_response(DapAbort::BadRequest(
                                    "malformed collect id".into(),
                                ))
                        }
                    };

                let span = info_span!(
                    "poll_collect_job",
                    dap.task_id = %task_id,
                    version = req.version.to_string()
                );

                match daph
                    .poll_collect_job(task_id, &collect_job_id)
                    .instrument(span)
                    .await
                {
                    Ok(DapCollectJob::Done(collect_resp)) => dap_response_to_worker(DapResponse {
                        version: req.version,
                        media_type: DapMediaType::Collection,
                        payload: collect_resp.get_encoded_with_param(&req.version),
                    }),
                    Ok(DapCollectJob::Pending) => Ok(Response::empty().unwrap().with_status(202)),
                    // TODO spec: Decide whether to define this behavior.
                    Ok(DapCollectJob::Unknown) => {
                        daph.state
                            .dap_abort_to_worker_response(DapAbort::BadRequest(
                                "unknown collect id".into(),
                            ))
                    }
                    Err(e) => daph.state.dap_abort_to_worker_response(e.into()),
                }
            },
        )
}

async fn put_report_into_task(
    req: Request,
    ctx: RouteContext<&DaphneWorkerRequestState<'_>>,
) -> Result<Response> {
    let daph = ctx.data.handler(&ctx.env);
    let req = daph.worker_request_to_dap(req, &ctx).await?;

    let span = info_span_from_dap_request!("upload", req);

    match daph.handle_upload_req(&req).instrument(span).await {
        Ok(()) => Response::empty(),
        Err(e) => daph.state.dap_abort_to_worker_response(e),
    }
}
