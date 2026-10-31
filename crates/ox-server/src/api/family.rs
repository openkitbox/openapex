//! `GET /ox/session/{id}/family` — an ox-only read of the whole session tree
//! that `{id}` belongs to, in one round trip.
//!
//! Upstream's `parentID` filter returns direct children only, so a client
//! would otherwise crawl to the root and walk the tree level by level, and
//! still learn nothing about each session's agent or run state. This endpoint
//! answers root first, then descendants breadth-first, with `running` taken
//! from the runner.
//!
//! Bounds: at most [`MAX_SESSIONS`] sessions, [`MAX_DEPTH`] levels up to the
//! root and [`MAX_DEPTH`] levels down. A tree past a bound is truncated
//! silently. [`admit_level`] owns ordering, de-duplication (cycle safety) and
//! the session cap; the async functions only fetch rows.

use anyhow::Result;
use ox_db::Db;
use ox_db::turso::Value as SqlValue;
use salvo::http::StatusCode;
use salvo::prelude::*;
use serde_json::{Value, json};

use super::session::{invalid_request, session_not_found};
use super::{compat, state};

const MAX_SESSIONS: usize = 200;
const MAX_DEPTH: usize = 16;

const FAMILY_COLUMNS: &str = "id, parent_id, title, agent, time_created, time_updated";

/// One session of the family, only the columns the response needs.
struct Member {
    id: String,
    parent_id: Option<String>,
    title: Option<String>,
    agent: Option<String>,
    created: i64,
    updated: i64,
}

impl Member {
    fn json(&self, running: bool) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "agent": self.agent,
            "parentID": self.parent_id,
            "time": { "created": self.created, "updated": self.updated },
            "running": running,
        })
    }
}

fn map_member(row: &ox_db::turso::Row) -> ox_db::turso::Result<Member> {
    Ok(Member {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        title: row.get(2)?,
        agent: row.get(3)?,
        created: row.get(4)?,
        updated: row.get(5)?,
    })
}

/// Append one breadth-first level to `out`: oldest first (ties by id), ids
/// already present skipped (a cycle can never re-admit a session), and the
/// total capped at [`MAX_SESSIONS`]. Returns how many sessions were added.
fn admit_level(out: &mut Vec<Member>, mut level: Vec<Member>) -> usize {
    level.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    let before = out.len();
    for member in level {
        if out.len() >= MAX_SESSIONS {
            break;
        }
        if out.iter().all(|seen| seen.id != member.id) {
            out.push(member);
        }
    }
    out.len() - before
}

async fn load_member(db: &Db, id: &str) -> Result<Option<Member>> {
    let sql = format!("SELECT {FAMILY_COLUMNS} FROM session_v2 WHERE id = ?1");
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let row = ox_db::optional(&mut stmt, [id]).await?;
    Ok(row.map(|row| map_member(&row)).transpose()?)
}

/// Walk `parent_id` upward from `start` to the root. Stops early at a missing
/// parent row, at a session already on the trail (a cycle), or after
/// [`MAX_DEPTH`] steps.
async fn find_root(db: &Db, start: Member) -> Result<Member> {
    let mut trail = vec![start.id.clone()];
    let mut current = start;
    for _ in 0..MAX_DEPTH {
        let Some(parent) = current.parent_id.as_deref() else {
            break;
        };
        if trail.iter().any(|seen| seen == parent) {
            break;
        }
        let Some(next) = load_member(db, parent).await? else {
            break;
        };
        trail.push(next.id.clone());
        current = next;
    }
    Ok(current)
}

/// Direct children of any of `parents`, oldest first. At most
/// `2 * MAX_SESSIONS` rows are read: ids already admitted (at most
/// `MAX_SESSIONS`) may come back and be skipped, and any further row would be
/// refused by the cap anyway, so a parent with a huge fan-out stays cheap.
async fn load_children_of(db: &Db, parents: &[Member]) -> Result<Vec<Member>> {
    let placeholders = (1..=parents.len())
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {FAMILY_COLUMNS} FROM session_v2 WHERE parent_id IN ({placeholders}) \
         ORDER BY time_created, id LIMIT {}",
        2 * MAX_SESSIONS
    );
    let params: Vec<SqlValue> = parents
        .iter()
        .map(|member| SqlValue::Text(member.id.clone()))
        .collect();
    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut children = Vec::new();
    while let Some(row) = rows.next().await? {
        children.push(map_member(&row)?);
    }
    Ok(children)
}

/// The family of `id`, root first; `None` when `id` does not exist.
async fn load_family(db: &Db, id: &str) -> Result<Option<Vec<Member>>> {
    let Some(start) = load_member(db, id).await? else {
        return Ok(None);
    };
    let mut out = vec![find_root(db, start).await?];
    let mut level_start = 0;
    for _ in 0..MAX_DEPTH {
        if out.len() >= MAX_SESSIONS {
            break;
        }
        let children = load_children_of(db, &out[level_start..]).await?;
        level_start = out.len();
        if admit_level(&mut out, children) == 0 {
            break;
        }
    }
    Ok(Some(out))
}

#[handler]
pub(crate) async fn family(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(id) = req.param::<String>("id") else {
        invalid_request(res, "Missing id");
        return;
    };
    match load_family(&app.db, &id).await {
        Ok(Some(members)) => {
            let data: Vec<Value> = members
                .iter()
                .map(|member| member.json(app.runner.is_active(&member.id)))
                .collect();
            let directory = compat::directory_of(req);
            res.render(Json(compat::envelope(&directory, Value::Array(data))));
        }
        Ok(None) => session_not_found(res, &id),
        Err(error) => {
            tracing::error!(%error, "session family lookup failed");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(
                json!({ "_tag": "UnknownError", "message": error.to_string() }),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use salvo::conn::{Listener, TcpListener};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;
    use crate::api::{AppState, build_router};
    use crate::test_support::{seed_session, test_state};

    fn member(id: &str, created: i64) -> Member {
        Member {
            id: id.to_owned(),
            parent_id: None,
            title: None,
            agent: None,
            created,
            updated: created,
        }
    }

    fn ids(members: &[Member]) -> Vec<&str> {
        members.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn admit_level_orders_siblings_by_created_then_id() {
        let mut out = vec![member("root", 0)];
        let added = admit_level(
            &mut out,
            vec![
                member("c", 5),
                member("b", 5),
                member("a", 9),
                member("z", 1),
            ],
        );
        assert_eq!(added, 4);
        assert_eq!(ids(&out), ["root", "z", "b", "c", "a"]);
    }

    #[test]
    fn admit_level_adds_nothing_for_an_empty_level() {
        let mut out = vec![member("root", 0)];
        assert_eq!(admit_level(&mut out, Vec::new()), 0);
        assert_eq!(ids(&out), ["root"]);
    }

    #[test]
    fn admit_level_never_re_admits_a_session_seen_before() {
        // A cycle root -> b -> root hands `root` back as b's child.
        let mut out = vec![member("root", 0), member("b", 1)];
        let added = admit_level(&mut out, vec![member("root", 0), member("c", 2)]);
        assert_eq!(added, 1);
        assert_eq!(ids(&out), ["root", "b", "c"]);
    }

    #[test]
    fn admit_level_drops_duplicates_inside_one_level() {
        let mut out = vec![member("root", 0)];
        let added = admit_level(&mut out, vec![member("a", 1), member("a", 1)]);
        assert_eq!(added, 1);
        assert_eq!(ids(&out), ["root", "a"]);
    }

    #[test]
    fn admit_level_truncates_at_the_session_cap() {
        let mut out = vec![member("root", 0)];
        let level = (0..MAX_SESSIONS + 50)
            .map(|n| member(&format!("s{n:04}"), n as i64))
            .collect();
        let added = admit_level(&mut out, level);
        assert_eq!(added, MAX_SESSIONS - 1);
        assert_eq!(out.len(), MAX_SESSIONS);
        assert_eq!(out[1].id, "s0000", "oldest kept");
        assert_eq!(admit_level(&mut out, vec![member("late", 0)]), 0);
    }

    async fn serve(app: &AppState) -> SocketAddr {
        let acceptor = TcpListener::new("127.0.0.1:0").bind().await;
        let addr = acceptor
            .local_addr()
            .expect("local addr")
            .to_string()
            .parse()
            .expect("socket addr");
        tokio::spawn(Server::new(acceptor).serve(build_router(app.clone())));
        addr
    }

    /// `(status line, body)` of a `GET path`.
    async fn get(addr: SocketAddr, path: &str) -> (String, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut raw = String::new();
        stream
            .read_to_string(&mut raw)
            .await
            .expect("read response");
        let (head, body) = raw.split_once("\r\n\r\n").expect("header end");
        let status = head.lines().next().expect("status line").to_owned();
        (status, body.to_owned())
    }

    /// A body is either plain or chunked; the JSON object is what lies
    /// between the first `{` and the last `}`.
    fn json_of(body: &str) -> Value {
        let start = body.find('{').expect("json start");
        let end = body.rfind('}').expect("json end");
        serde_json::from_str(&body[start..=end]).expect("json body")
    }

    async fn seeded() -> AppState {
        let app = test_state().await;
        seed_session(&app, "ses_root", None).await;
        seed_session(&app, "ses_b", Some("ses_root")).await;
        seed_session(&app, "ses_c", Some("ses_root")).await;
        seed_session(&app, "ses_grand", Some("ses_c")).await;
        app
    }

    #[tokio::test]
    async fn family_of_a_child_is_the_whole_tree_root_first() {
        let app = seeded().await;
        let addr = serve(&app).await;

        let (status, body) = get(addr, "/ox/session/ses_c/family").await;
        assert!(status.contains("200"), "{status} {body}");
        let reply = json_of(&body);
        assert!(reply["location"]["directory"].is_string(), "{reply}");
        let data = reply["data"].as_array().expect("data array");
        let got: Vec<(&str, Option<&str>)> = data
            .iter()
            .map(|m| (m["id"].as_str().unwrap(), m["parentID"].as_str()))
            .collect();
        assert_eq!(got[0], ("ses_root", None), "{reply}");
        assert_eq!(got.len(), 4, "{reply}");
        assert!(got.contains(&("ses_b", Some("ses_root"))), "{reply}");
        assert!(got.contains(&("ses_c", Some("ses_root"))), "{reply}");
        assert_eq!(
            got[3],
            ("ses_grand", Some("ses_c")),
            "grandchild last: {reply}"
        );
        let first = &data[0];
        assert!(first["time"]["created"].is_i64(), "{first}");
        assert!(first["time"]["updated"].is_i64(), "{first}");
        assert_eq!(first["running"], json!(false), "{first}");
        assert!(
            first.get("title").is_some() && first.get("agent").is_some(),
            "{first}"
        );
    }

    #[tokio::test]
    async fn family_of_a_lone_session_is_itself() {
        let app = test_state().await;
        seed_session(&app, "ses_solo", None).await;
        let addr = serve(&app).await;

        let (status, body) = get(addr, "/ox/session/ses_solo/family").await;
        assert!(status.contains("200"), "{status} {body}");
        let reply = json_of(&body);
        assert_eq!(reply["data"].as_array().expect("data").len(), 1, "{reply}");
    }

    #[tokio::test]
    async fn family_of_an_unknown_session_is_404() {
        let app = test_state().await;
        let addr = serve(&app).await;

        let (status, body) = get(addr, "/ox/session/ses_missing/family").await;
        assert!(status.contains("404"), "{status} {body}");
        assert!(body.contains("SessionNotFoundError"), "{body}");
    }

    #[tokio::test]
    async fn family_is_not_served_under_api() {
        let app = seeded().await;
        let addr = serve(&app).await;

        let (status, body) = get(addr, "/api/session/ses_c/family").await;
        assert!(status.contains("404"), "{status} {body}");
        assert!(!body.contains("ses_root"), "{body}");
    }
}
