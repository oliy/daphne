// Copyright (c) 2022 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

mod aggregator;
mod helper;
mod leader;
pub mod test_routes;

use std::str::FromStr;

use daphne::DapResponse;
use serde::Deserialize;
use worker::{Error, Headers, Response, Result, Router};

use crate::{config::DaphneWorkerRequestState, DEFAULT_RESPONSE_HTML};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Leader,
    Helper,
}

impl Role {
    pub fn is_leader(self) -> bool {
        self == Self::Leader
    }

    #[allow(dead_code)]
    pub fn is_helper(self) -> bool {
        self == Self::Helper
    }
}

impl FromStr for Role {
    type Err = worker::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "leader" => Ok(Self::Leader),
            "helper" => Ok(Self::Helper),
            role => Err(worker::Error::RustError(format!(
                "Unhandled DAP role: {role}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RouterOptions {
    pub enable_default_response: bool,
    pub enable_internal_test: bool,
    pub role: Role,
}

pub(super) type DapRouter<'s> = Router<'s, &'s DaphneWorkerRequestState<'s>>;

pub(super) fn create_router<'s>(
    state: &'s DaphneWorkerRequestState<'s>,
    opts: RouterOptions,
) -> Result<DapRouter<'s>> {
    let router = Router::with_data(state);

    let router = aggregator::add_aggregator_routes(router);

    let router = match opts.role {
        Role::Leader => leader::add_leader_routes(router),
        Role::Helper => helper::add_helper_routes(router),
    };

    let router = if opts.enable_internal_test {
        test_routes::add_internal_test_routes(router, opts.role)
    } else {
        router
    };

    let router = if opts.enable_default_response {
        router.or_else_any_method_async("/*catchall", |_req, ctx| async move {
            match ctx.var("DAP_DEFAULT_RESPONSE_HTML") {
                Ok(text) => Response::from_html(text.to_string()),
                Err(..) => Response::from_html(DEFAULT_RESPONSE_HTML),
            }
        })
    } else {
        router
    };

    Ok(router)
}

fn dap_response_to_worker(resp: DapResponse) -> Result<Response> {
    let mut headers = Headers::new();
    headers.set(
        "Content-Type",
        resp.media_type
            .as_str_for_version(resp.version)
            .ok_or_else(|| {
                Error::RustError(format!(
                    "failed to construct content-type for media type {:?} and version {:?}",
                    resp.media_type, resp.version
                ))
            })?,
    )?;
    let worker_resp = Response::from_bytes(resp.payload)?.with_headers(headers);
    Ok(worker_resp)
}

#[macro_export]
macro_rules! info_span_from_dap_request {
    ($span_name:expr, $req:expr) => {{
        let req: &$crate::DapRequest<_> = &$req;
        let task_id = req
            .task_id
            .clone()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_owned());

        ::tracing::info_span!(
            $span_name,
            dap.task_id = task_id,
            version = req.version.to_string()
        )
    }};
}
