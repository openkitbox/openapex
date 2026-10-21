//! `GET /api/skill` — the discovered-skill catalog, upstream `skill.list`.
//!
//! Answers `{ location, data }` like every other catalog endpoint: `location`
//! echoes the REQUESTED directory ([`directory_of`], project-memory #5), and
//! `data` lists the skills that directory sees.

use salvo::prelude::*;
use serde_json::json;

use crate::api::compat::directory_of;
use crate::tool::skills;

#[handler]
pub async fn list(req: &mut Request, res: &mut Response) {
    let directory = directory_of(req);
    let skills = skills::discover(std::path::Path::new(&directory));
    let data: Vec<_> = skills
        .iter()
        .map(|skill| {
            json!({
                "id": skill.id,
                "name": skill.name,
                "description": skill.description,
                "path": skill.path,
                "content": skill.content,
            })
        })
        .collect();
    res.render(Json(json!({
        "location": { "directory": directory },
        "data": data,
    })));
}
