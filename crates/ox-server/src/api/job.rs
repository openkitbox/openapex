//! Durable background job endpoints.

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};

use ox_tools::jobs::{JobFilter, JobSnapshot, JobStatus};

use super::state;

#[derive(Debug, Default, Deserialize)]
struct Query {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    descendants: Option<bool>,
}

#[derive(Debug, Serialize)]
struct JobResponse {
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    name: String,
    title: String,
    status: String,
    time: JobTime,
    metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct JobTime {
    created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<i64>,
}

fn response(job: JobSnapshot) -> JobResponse {
    JobResponse {
        id: job.id,
        session_id: job.session_id,
        name: job.kind,
        title: job.title,
        status: job.status.as_str().to_owned(),
        time: JobTime {
            created: job.started,
            completed: job.completed,
        },
        metadata: job.metadata,
    }
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
    res.render(Json(
        serde_json::json!({ "data": data.into_iter().map(response).collect::<Vec<_>>() }),
    ));
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
