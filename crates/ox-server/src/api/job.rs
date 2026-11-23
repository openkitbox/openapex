//! Durable background job endpoints.

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::Value;

use ox_tools::jobs::{JobFilter, JobStatus};

use super::state;

#[derive(Debug, Default, Deserialize)]
struct Query {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    descendants: Option<bool>,
}

#[handler]
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let query = req.parse_queries::<Query>().unwrap_or_default();
    let status = query.status.as_deref().and_then(JobStatus::parse);
    let data = match app
        .services()
        .tools
        .jobs
        .durable_snapshot(JobFilter {
            session_id: query.session_id.as_deref(),
            kind: query.kind.as_deref(),
            status,
            descendants: query.descendants.unwrap_or(false),
        })
        .await
    {
        Ok(data) => data,
        Err(error) => {
            tracing::warn!(error = %error, "unable to read authoritative job state");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    let data = data
        .into_iter()
        .filter_map(|job| match job.to_info() {
            Some(info) => Some(info),
            None => {
                tracing::warn!(job_id = %job.id, "skipping invalid durable job record");
                None
            }
        })
        .collect::<Vec<Value>>();
    res.render(Json(serde_json::json!({ "data": data })));
}

#[handler]
pub(crate) async fn cancel(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    };
    let _ = app.services().tools.jobs.cancel(&id);
    res.status_code(StatusCode::NO_CONTENT);
}
