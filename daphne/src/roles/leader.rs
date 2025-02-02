// Copyright (c) 2023 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use std::{borrow::Cow, collections::HashMap};

use async_trait::async_trait;
use prio::codec::{Decode, ParameterizedDecode, ParameterizedEncode};
use tracing::{debug, error};
use url::Url;

use super::{check_batch, check_request_content_type, resolve_taskprov, DapAggregator};
use crate::{
    constants::DapMediaType,
    error::DapAbort,
    fatal_error,
    messages::{
        AggregateShare, AggregateShareReq, AggregationJobResp, BatchSelector, Collection,
        CollectionJobId, CollectionReq, Interval, PartialBatchSelector, Query, Report, TaskId,
    },
    metrics::DaphneRequestType,
    DapCollectJob, DapError, DapLeaderProcessTelemetry, DapLeaderTransition, DapRequest,
    DapResource, DapResponse, DapTaskConfig, DapVersion, MetaAggregationJobId,
};

struct LeaderHttpRequestOptions<'p> {
    path: &'p str,
    req_media_type: DapMediaType,
    resp_media_type: DapMediaType,
    resource: DapResource,
    req_data: Vec<u8>,
    method: LeaderHttpRequestMethod,
}

enum LeaderHttpRequestMethod {
    Post,
    Put,
}

async fn leader_send_http_request<S>(
    role: &impl DapLeader<S>,
    task_id: &TaskId,
    task_config: &DapTaskConfig,
    opts: LeaderHttpRequestOptions<'_>,
) -> Result<DapResponse, DapError> {
    let LeaderHttpRequestOptions {
        path,
        req_media_type,
        resp_media_type,
        resource,
        req_data,
        method,
    } = opts;

    let url = task_config
        .helper_url
        .join(path)
        .map_err(|e| fatal_error!(err = ?e))?;

    let req = DapRequest {
        version: task_config.version,
        media_type: req_media_type.clone(),
        task_id: Some(task_id.clone()),
        resource,
        url,
        sender_auth: Some(
            role.authorize(task_id, task_config, &req_media_type, &req_data)
                .await?,
        ),
        payload: req_data,
        taskprov: None,
    };

    let resp = match method {
        LeaderHttpRequestMethod::Put => role.send_http_put(req).await?,
        LeaderHttpRequestMethod::Post => role.send_http_post(req).await?,
    };

    check_response_content_type(&resp, resp_media_type)?;
    Ok(resp)
}

/// A party in the DAP protocol who is authorized to send requests to another party.
#[async_trait(?Send)]
pub trait DapAuthorizedSender<S> {
    /// Add authorization to an outbound DAP request with the given task ID, media type, and payload.
    async fn authorize(
        &self,
        task_id: &TaskId,
        task_config: &DapTaskConfig,
        media_type: &DapMediaType,
        payload: &[u8],
    ) -> Result<S, DapError>;
}

/// DAP Leader functionality.
#[async_trait(?Send)]
pub trait DapLeader<S>: DapAuthorizedSender<S> + DapAggregator<S> {
    /// Data type used to guide selection of a set of reports for aggregation.
    type ReportSelector;

    /// Store a report for use later on.
    async fn put_report(&self, report: &Report, task_id: &TaskId) -> Result<(), DapError>;

    /// Fetch a sequence of reports to aggregate, grouped by task ID, then by partial batch
    /// selector. The reports returned are removed from persistent storage.
    async fn get_reports(
        &self,
        selector: &Self::ReportSelector,
    ) -> Result<HashMap<TaskId, HashMap<PartialBatchSelector, Vec<Report>>>, DapError>;

    /// Create a collect job.
    //
    // TODO spec: Figure out if the hostname for the collect URI needs to match the Leader.
    async fn init_collect_job(
        &self,
        task_id: &TaskId,
        collect_job_id: &Option<CollectionJobId>,
        collect_req: &CollectionReq,
    ) -> Result<Url, DapError>;

    /// Check the status of a collect job.
    async fn poll_collect_job(
        &self,
        task_id: &TaskId,
        collect_id: &CollectionJobId,
    ) -> Result<DapCollectJob, DapError>;

    /// Fetch the current collect job queue. The result is the sequence of collect ID and request
    /// pairs, in order of priority.
    async fn get_pending_collect_jobs(
        &self,
    ) -> Result<Vec<(TaskId, CollectionJobId, CollectionReq)>, DapError>;

    /// Complete a collect job by assigning it the completed [`CollectResp`](crate::messages::Collection).
    async fn finish_collect_job(
        &self,
        task_id: &TaskId,
        collect_id: &CollectionJobId,
        collect_resp: &Collection,
    ) -> Result<(), DapError>;

    /// Send an HTTP POST request.
    async fn send_http_post(&self, req: DapRequest<S>) -> Result<DapResponse, DapError>;

    /// Send an HTTP PUT request.
    async fn send_http_put(&self, req: DapRequest<S>) -> Result<DapResponse, DapError>;

    /// Handle a report from a Client.
    async fn handle_upload_req(&self, req: &DapRequest<S>) -> Result<(), DapAbort> {
        let metrics = self.metrics().with_host(req.host());
        let task_id = req.task_id()?;
        debug!("upload for task {task_id}");

        // Check whether the DAP version indicated by the sender is supported.
        if req.version == DapVersion::Unknown {
            return Err(DapAbort::version_unknown());
        }

        check_request_content_type(req, DapMediaType::Report)?;

        let report = Report::get_decoded_with_param(&req.version, req.payload.as_ref())
            .map_err(|e| DapAbort::from_codec_error(e, task_id.clone()))?;
        debug!("report id is {}", report.report_metadata.id);

        if let Some(taskprov_version) = self.get_global_config().taskprov_version {
            resolve_taskprov(
                self,
                task_id,
                req,
                Some(&report.report_metadata),
                taskprov_version,
            )
            .await?;
        }
        let task_config = self
            .get_task_config_for(Cow::Borrowed(task_id))
            .await?
            .ok_or(DapAbort::UnrecognizedTask)?;

        // Check whether the DAP version in the request matches the task config.
        if task_config.as_ref().version != req.version {
            return Err(DapAbort::version_mismatch(
                req.version,
                task_config.as_ref().version,
            ));
        }

        if report.encrypted_input_shares.len() != 2 {
            // TODO spec: Decide if this behavior should be specified.
            return Err(DapAbort::UnrecognizedMessage {
                detail: format!(
                    "expected exactly two encrypted input shares; got {}",
                    report.encrypted_input_shares.len()
                ),
                task_id: Some(task_id.clone()),
            });
        }

        // Check that the indicated HpkeConfig is present.
        //
        // TODO spec: It's not clear if this behavior is MUST, SHOULD, or MAY.
        if !self
            .can_hpke_decrypt(req.task_id()?, report.encrypted_input_shares[0].config_id)
            .await?
        {
            return Err(DapAbort::ReportRejected {
                detail: "No current HPKE configuration matches the indicated ID.".into(),
            });
        }

        // Check that the task has not expired.
        if report.report_metadata.time >= task_config.as_ref().expiration {
            return Err(DapAbort::ReportTooLate);
        }

        // Store the report for future processing. At this point, the report may be rejected if
        // the Leader detects that the report was replayed or pertains to a batch that has already
        // been collected.
        self.put_report(&report, req.task_id()?).await?;

        metrics.inbound_req_inc(DaphneRequestType::Upload);
        Ok(())
    }

    /// Handle a collect job from the Collector. The response is the URI that the Collector will
    /// poll later on to get the collection.
    async fn handle_collect_job_req(&self, req: &DapRequest<S>) -> Result<Url, DapAbort> {
        let now = self.get_current_time();
        let metrics = self.metrics().with_host(req.host());
        let task_id = req.task_id()?;
        debug!("collect for task {task_id}");

        // Check whether the DAP version indicated by the sender is supported.
        if req.version == DapVersion::Unknown {
            return Err(DapAbort::version_unknown());
        }

        check_request_content_type(req, DapMediaType::CollectReq)?;

        if let Some(taskprov_version) = self.get_global_config().taskprov_version {
            resolve_taskprov(self, task_id, req, None, taskprov_version).await?;
        }

        let wrapped_task_config = self
            .get_task_config_for(Cow::Borrowed(req.task_id()?))
            .await?
            .ok_or(DapAbort::UnrecognizedTask)?;
        let task_config = wrapped_task_config.as_ref();

        if let Some(reason) = self.unauthorized_reason(task_config, req).await? {
            error!("aborted unauthorized collect request: {reason}");
            return Err(DapAbort::UnauthorizedRequest {
                detail: reason,
                task_id: task_id.clone(),
            });
        }

        let mut collect_req =
            CollectionReq::get_decoded_with_param(&req.version, req.payload.as_ref())
                .map_err(|e| DapAbort::from_codec_error(e, task_id.clone()))?;

        // Check whether the DAP version in the request matches the task config.
        if task_config.version != req.version {
            return Err(DapAbort::version_mismatch(req.version, task_config.version));
        }

        if collect_req.query == Query::FixedSizeCurrentBatch {
            // This is where we assign the current batch, and convert the
            // Query::FixedSizeCurrentBatch into a Query::FixedSizeByBatchId.
            //
            // TODO(bhalleycf) Note that currently we are just looking at the
            // head of the uncollected batch queue, so there is no parallelism
            // possible for collectors on a given task.  To allow multiple
            // batches for a task to be collected concurrently for the same task,
            // we'd need a more complex DO state that allowed us to have batch
            // state go from unassigned -> in-progress -> complete.
            let batch_id = self.current_batch(task_id).await?;
            debug!("FixedSize batch id is {batch_id}");
            collect_req.query = Query::FixedSizeByBatchId { batch_id };
        }

        // Ensure the batch boundaries are valid and that the batch doesn't overlap with previosuly
        // collected batches.
        let batch_selector = BatchSelector::try_from(collect_req.query.clone())?;
        check_batch(
            self,
            task_config,
            task_id,
            &batch_selector,
            &collect_req.agg_param,
            now,
        )
        .await?;

        // draft02 compatibility: In draft02, the collection job ID is generated as a result of the
        // initial collection request, whereas in the latest draft, the collection job ID is parsed
        // from the request path.
        let collect_job_id = match (req.version, &req.resource) {
            (DapVersion::Draft02, DapResource::Undefined) => None,
            (DapVersion::Draft07, DapResource::CollectionJob(ref collect_job_id)) => {
                Some(collect_job_id.clone())
            }
            (DapVersion::Draft07, DapResource::Undefined) => {
                return Err(DapAbort::BadRequest("undefined resource".into()));
            }
            _ => unreachable!("unhandled resource {:?}", req.resource),
        };

        let collect_job_uri = self
            .init_collect_job(task_id, &collect_job_id, &collect_req)
            .await?;

        metrics.inbound_req_inc(DaphneRequestType::Collect);
        Ok(collect_job_uri)
    }

    /// Run an aggregation job for a set of reports. Return the number of reports that were
    /// aggregated successfully.
    //
    // TODO Handle non-encodable messages gracefully. The length of `reports` may be too long to
    // encode in `AggregationJobInitReq`, in which case this method will panic. We should increase
    // the capacity of this message in the spec. In the meantime, we should at a minimum log this
    // when it happens.
    async fn run_agg_job(
        &self,
        task_id: &TaskId,
        task_config: &DapTaskConfig,
        part_batch_sel: &PartialBatchSelector,
        reports: Vec<Report>,
        host: &str,
    ) -> Result<u64, DapAbort> {
        let metrics = self.metrics().with_host(host);

        // Prepare AggregationJobInitReq.
        let agg_job_id = MetaAggregationJobId::gen_for_version(&task_config.version);
        let transition = task_config
            .vdaf
            .produce_agg_job_init_req(
                self,
                self,
                task_id,
                task_config,
                &agg_job_id,
                part_batch_sel,
                reports,
                &metrics,
            )
            .await?;
        let (state, agg_job_init_req) = match transition {
            DapLeaderTransition::Continue(state, agg_job_init_req) => (state, agg_job_init_req),
            DapLeaderTransition::Skip => return Ok(0),
            DapLeaderTransition::Uncommitted(..) => {
                return Err(fatal_error!(err = "unexpected state transition (uncommitted)").into())
            }
        };
        let method = if task_config.version != DapVersion::Draft02 {
            LeaderHttpRequestMethod::Put
        } else {
            LeaderHttpRequestMethod::Post
        };
        let url_path = if task_config.version == DapVersion::Draft02 {
            "aggregate".to_string()
        } else {
            format!(
                "tasks/{}/aggregation_jobs/{}",
                task_id.to_base64url(),
                agg_job_id.to_base64url()
            )
        };

        // Send AggregationJobInitReq and receive AggregationJobResp.
        let resp = leader_send_http_request(
            self,
            task_id,
            task_config,
            LeaderHttpRequestOptions {
                path: &url_path,
                req_media_type: DapMediaType::AggregationJobInitReq,
                resp_media_type: DapMediaType::AggregationJobResp,
                resource: agg_job_id.for_request_path(),
                req_data: agg_job_init_req.get_encoded_with_param(&task_config.version),
                method,
            },
        )
        .await?;
        let agg_job_resp = AggregationJobResp::get_decoded(&resp.payload)
            .map_err(|e| DapAbort::from_codec_error(e, task_id.clone()))?;

        // Prepare AggreagteContinueReq.
        let transition = task_config.vdaf.handle_agg_job_resp(
            task_id,
            &agg_job_id,
            state,
            agg_job_resp,
            task_config.version,
            &metrics,
        )?;
        let (uncommited, agg_job_cont_req) = match transition {
            DapLeaderTransition::Uncommitted(uncommited, agg_job_cont_req) => {
                (uncommited, agg_job_cont_req)
            }
            DapLeaderTransition::Skip => return Ok(0),
            DapLeaderTransition::Continue(..) => {
                return Err(fatal_error!(err = "unexpected state transition (continue)").into())
            }
        };

        // Send AggregationJobContinueReq and receive AggregationJobResp.
        let resp = leader_send_http_request(
            self,
            task_id,
            task_config,
            LeaderHttpRequestOptions {
                path: &url_path,
                req_media_type: DapMediaType::AggregationJobContinueReq,
                resp_media_type: DapMediaType::agg_job_cont_resp_for_version(task_config.version),
                resource: agg_job_id.for_request_path(),
                req_data: agg_job_cont_req.get_encoded_with_param(&task_config.version),
                method: LeaderHttpRequestMethod::Post,
            },
        )
        .await?;
        let agg_job_resp = AggregationJobResp::get_decoded(&resp.payload)
            .map_err(|e| DapAbort::from_codec_error(e, task_id.clone()))?;

        // Commit the output shares.
        let agg_share_span = task_config.vdaf.handle_final_agg_job_resp(
            task_config,
            uncommited,
            agg_job_resp,
            &metrics,
        )?;
        let out_shares_count = agg_share_span.report_count() as u64;

        // At this point we're committed to aggregating the reports: if we do detect a report was
        // replayed at this stage, then we may end up with a batch mismatch. However, this should
        // only happen if there are multiple aggregation jobs in-flight that include the same
        // report.

        let replayed = self
            .try_put_agg_share_span(task_id, task_config, agg_share_span)
            .await?;

        if let Some(replayed) = replayed {
            tracing::warn!(
                replay_count = replayed.len(),
                "tried to aggregate replayed reports"
            );
        }

        metrics.report_inc_by("aggregated", out_shares_count);
        Ok(out_shares_count)
    }

    /// Handle a pending collection job. If the results are ready, then compute the aggregate
    /// results and store them to be retrieved by the Collector later. Returns the number of
    /// reports in the batch.
    async fn run_collect_job(
        &self,
        task_id: &TaskId,
        collect_id: &CollectionJobId,
        task_config: &DapTaskConfig,
        collect_req: &CollectionReq,
        host: &str,
    ) -> Result<u64, DapAbort> {
        let metrics = self.metrics().with_host(host);

        debug!("collecting id {collect_id}");
        let batch_selector = BatchSelector::try_from(collect_req.query.clone())?;
        let leader_agg_share = self.get_agg_share(task_id, &batch_selector).await?;

        // Check the batch size. If not not ready, then return early.
        //
        // TODO Consider logging this error, as it should never happen.
        if !task_config.is_report_count_compatible(task_id, leader_agg_share.report_count)? {
            return Ok(0);
        }

        let batch_selector = BatchSelector::try_from(collect_req.query.clone())?;

        // Prepare the Leader's aggregate share.
        let leader_enc_agg_share = task_config.vdaf.produce_leader_encrypted_agg_share(
            &task_config.collector_hpke_config,
            task_id,
            &batch_selector,
            &leader_agg_share,
            task_config.version,
        )?;

        // Prepare AggregateShareReq.
        let agg_share_req = AggregateShareReq {
            draft02_task_id: task_id.for_request_payload(&task_config.version),
            batch_sel: batch_selector.clone(),
            agg_param: collect_req.agg_param.clone(),
            report_count: leader_agg_share.report_count,
            checksum: leader_agg_share.checksum,
        };

        let url_path = if task_config.version == DapVersion::Draft02 {
            "aggregate_share".to_string()
        } else {
            format!("tasks/{}/aggregate_shares", task_id.to_base64url())
        };

        // Send AggregateShareReq and receive AggregateShareResp.
        let resp = leader_send_http_request(
            self,
            task_id,
            task_config,
            LeaderHttpRequestOptions {
                path: &url_path,
                req_media_type: DapMediaType::AggregateShareReq,
                resp_media_type: DapMediaType::AggregateShare,
                resource: DapResource::Undefined,
                req_data: agg_share_req.get_encoded_with_param(&task_config.version),
                method: LeaderHttpRequestMethod::Post,
            },
        )
        .await?;
        let agg_share_resp = AggregateShare::get_decoded(&resp.payload)
            .map_err(|e| DapAbort::from_codec_error(e, task_id.clone()))?;
        // For draft07 and later, the Collection message includes the smallest quantized time
        // interval containing all reports in the batch.
        let interval = match task_config.version {
            DapVersion::Draft02 => None,
            DapVersion::Draft07 => {
                let low = task_config.quantized_time_lower_bound(leader_agg_share.min_time);
                let high = task_config.quantized_time_upper_bound(leader_agg_share.max_time);
                Some(Interval {
                    start: low,
                    duration: if high > low {
                        high - low
                    } else {
                        // This should never happen!
                        task_config.time_precision
                    },
                })
            }
            _ => unreachable!("unhandled version {}", task_config.version),
        };

        // Complete the collect job.
        let collection = Collection {
            part_batch_sel: batch_selector.into(),
            report_count: leader_agg_share.report_count,
            interval,
            encrypted_agg_shares: vec![leader_enc_agg_share, agg_share_resp.encrypted_agg_share],
        };
        self.finish_collect_job(task_id, collect_id, &collection)
            .await?;

        // Mark reports as collected.
        self.mark_collected(task_id, &agg_share_req.batch_sel)
            .await?;

        metrics.report_inc_by("collected", agg_share_req.report_count);
        Ok(agg_share_req.report_count)
    }

    /// Fetch a set of reports grouped by task, then run an aggregation job for each task. Once all
    /// jobs are completed, process the collect job queue. It is not safe to run multiple instances
    /// of this function in parallel.
    ///
    /// This method is geared primarily towards testing. It also demonstrates how to properly
    /// synchronize collect and aggregation jobs. If used in a large DAP deployment, it is likely
    /// create a bottleneck. Such deployments can improve throughput by running many aggregation
    /// jobs in parallel.
    async fn process(
        &self,
        selector: &Self::ReportSelector,
        host: &str,
    ) -> Result<DapLeaderProcessTelemetry, DapAbort> {
        let mut telem = DapLeaderProcessTelemetry::default();

        tracing::debug!("RUNNING get_reports");
        // Fetch reports and run an aggregation job for each task.
        for (task_id, reports) in self.get_reports(selector).await?.into_iter() {
            tracing::debug!("RUNNING get_task_config_for {task_id}");
            let task_config = self
                .get_task_config_for(Cow::Owned(task_id.clone()))
                .await?
                .ok_or(DapAbort::UnrecognizedTask)?;

            for (part_batch_sel, reports) in reports.into_iter() {
                // TODO Consider splitting reports into smaller chunks.
                // TODO Consider handling tasks in parallel.
                telem.reports_processed += reports.len() as u64;
                debug!(
                    "process {} reports for task {task_id} with selector {part_batch_sel:?}",
                    reports.len()
                );
                if !reports.is_empty() {
                    tracing::debug!(
                        "RUNNING run_agg_job FOR TID {task_id} AND {part_batch_sel:?} AND {host}"
                    );
                    telem.reports_aggregated += self
                        .run_agg_job(
                            &task_id,
                            task_config.as_ref(),
                            &part_batch_sel,
                            reports,
                            host,
                        )
                        .await?;
                }
            }
        }
        // Process pending collect jobs. We wait until all aggregation jobs are finished before
        // proceeding to this step. This is to prevent a race condition involving an aggregate
        // share computed during a collect job and any output shares computed during an aggregation
        // job.
        tracing::debug!("GETTING get_pending_collect_jobs");
        for (task_id, collect_id, collect_req) in self.get_pending_collect_jobs().await? {
            tracing::debug!("GETTING get_task_config_for {task_id}");
            let task_config = self
                .get_task_config_for(Cow::Owned(task_id.clone()))
                .await?
                .ok_or(DapAbort::UnrecognizedTask)?;

            tracing::debug!("RUNNING run_collect_job FOR TID {task_id} AND {collect_id} AND {collect_req:?} AND {host}");
            telem.reports_collected += self
                .run_collect_job(
                    &task_id,
                    &collect_id,
                    task_config.as_ref(),
                    &collect_req,
                    host,
                )
                .await?;
        }

        Ok(telem)
    }
}

fn check_response_content_type(resp: &DapResponse, expected: DapMediaType) -> Result<(), DapError> {
    let want_str = expected
        .as_str_for_version(resp.version)
        .expect("could not determine string representation for expected content-type");

    if resp.media_type != expected {
        if let Some(got_str) = resp.media_type.as_str_for_version(resp.version) {
            Err(fatal_error!(
                err = "response from peer has unexpected content-type",
                got = got_str,
                want = want_str,
            ))
        } else {
            Err(fatal_error!(
                err = "response from peer has no content-type",
                expected = want_str,
            ))
        }
    } else {
        Ok(())
    }
}
