use anyhow::Result;
use ox_db::turso::Value as SqlValue;
use salvo::prelude::*;
use serde_json::{Value, json};

use ox_db::Db;

use super::cursor::{
    Anchor, CursorQuery, Direction, SessionsCursor, ascending_of, decode_cursor, encode_cursor,
    resolve_archived, resolve_limit, resolve_order,
};
use super::response::{invalid_cursor, invalid_request, unknown_error};
use super::session_row::{SESSION_COLUMNS, SessionRow, map_session_row};
use crate::api::state;

// ---------------------------------------------------------------------------
// session.list — GET /api/session
// ---------------------------------------------------------------------------

struct SessionsPage {
    directory: Option<String>,
    project: Option<String>,
    subpath: Option<String>,
    search: Option<String>,
    parent_id: Option<String>,
    archived: Option<bool>,
    ascending: bool,
    anchor: Option<(i64, String)>,
    limit: i64,
}

fn validate_scope(
    directory: Option<&str>,
    project: Option<&str>,
    subpath: Option<&str>,
) -> Result<(), String> {
    match (directory, project, subpath) {
        (Some(_), None, None) | (None, Some(_), None | Some(_)) | (None, None, None) => Ok(()),
        (Some(_), _, _) => Err("directory cannot be combined with project or subpath".to_owned()),
        (None, None, Some(_)) => Err("subpath requires project".to_owned()),
    }
}

async fn query_sessions(db: &Db, page: SessionsPage) -> Result<Vec<SessionRow>> {
    let SessionsPage {
        directory,
        project,
        subpath,
        search,
        parent_id,
        archived,
        ascending,
        anchor,
        limit,
    } = page;

    let mut sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2");
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<SqlValue> = Vec::new();

    // These are the three mutually exclusive upstream input shapes. In
    // particular, a project query must not acquire a directory predicate from
    // request headers or from the server's working directory.
    if let Some(directory) = directory {
        params.push(SqlValue::Text(directory));
        conds.push(format!("directory = ?{}", params.len()));
    } else if let Some(project) = project {
        params.push(SqlValue::Text(project));
        conds.push(format!("project_id = ?{}", params.len()));
        if let Some(subpath) = subpath {
            params.push(SqlValue::Text(subpath));
            conds.push(format!("path = ?{}", params.len()));
        }
    }
    if let Some(search) = search {
        params.push(SqlValue::Text(format!("%{search}%")));
        conds.push(format!("title LIKE ?{}", params.len()));
    }
    if let Some(archived) = archived {
        conds.push(
            if archived {
                "time_archived IS NOT NULL"
            } else {
                "time_archived IS NULL"
            }
            .to_owned(),
        );
    }
    if let Some(parent_id) = parent_id {
        if parent_id == "null" {
            conds.push("parent_id IS NULL".to_owned());
        } else {
            params.push(SqlValue::Text(parent_id));
            conds.push(format!("parent_id = ?{}", params.len()));
        }
    }
    if let Some((time, anchor_id)) = anchor {
        params.push(SqlValue::Integer(time));
        let time_idx = params.len();
        params.push(SqlValue::Text(anchor_id));
        let id_idx = params.len();
        if ascending {
            conds.push(format!(
                "(time_updated > ?{time_idx} OR (time_updated = ?{time_idx} AND id > ?{id_idx}))"
            ));
        } else {
            conds.push(format!(
                "(time_updated < ?{time_idx} OR (time_updated = ?{time_idx} AND id < ?{id_idx}))"
            ));
        }
    }

    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(if ascending {
        " ORDER BY time_updated ASC, id ASC"
    } else {
        " ORDER BY time_updated DESC, id DESC"
    });
    sql.push_str(&format!(" LIMIT {limit}"));

    let conn = db.acquire().await?;
    let mut stmt = conn.prepare(&sql).await?;
    let mut rows = stmt.query(params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(map_session_row(&row)?);
    }
    Ok(out)
}

#[handler] // operation_id: session.list
pub(crate) async fn list(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };

    let raw_cursor: Option<String> = req.query("cursor");
    // A cursor carries the whole query including its order, so a request-level
    // `order` alongside a cursor is simply ignored (upstream `session.list`).

    let decoded: Option<SessionsCursor> = match raw_cursor.as_deref() {
        Some(raw) => match decode_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                invalid_cursor(res, "Invalid cursor");
                return;
            }
        },
        None => None,
    };

    // Validate the request-level filters even when a cursor carries the query:
    // an unparseable value fails closed instead of being silently ignored.
    let archived = match resolve_archived(req.query::<String>("archived")) {
        Ok(archived) => archived,
        Err(message) => {
            invalid_request(res, &message);
            return;
        }
    };

    // A cursor carries the whole query; otherwise read it from the request.
    let (query, direction) = match &decoded {
        Some(cursor) => (cursor.query.clone(), cursor.anchor.direction),
        None => {
            let mut query = match request_query(req) {
                Ok(query) => query,
                Err(message) => {
                    invalid_request(res, &message);
                    return;
                }
            };
            query.archived = archived;
            (query, Direction::Next)
        }
    };

    let limit = match resolve_limit(req.query::<String>("limit")) {
        Ok(limit) => limit,
        Err(message) => {
            invalid_request(res, &message);
            return;
        }
    };
    let ascending = ascending_of(query.order.as_deref(), direction);
    let anchor = decoded
        .as_ref()
        .map(|cursor| (cursor.anchor.time, cursor.anchor.id.clone()));

    if let Err(message) = validate_scope(
        query.directory.as_deref(),
        query.project.as_deref(),
        query.subpath.as_deref(),
    ) {
        invalid_request(res, &message);
        return;
    }
    let page = SessionsPage {
        directory: query.directory.clone(),
        project: query.project.clone(),
        subpath: query.subpath.clone(),
        search: query.search.clone(),
        parent_id: query.parent_id.clone(),
        archived: query.archived,
        ascending,
        anchor,
        limit,
    };

    let rows = match query_sessions(&app.db, page).await {
        Ok(rows) => rows,
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    // The fetch walks opposite to the requested order for `previous`; reverse
    // so the page comes out in the requested order (upstream store.ts).
    let mut rows = rows;
    if direction == Direction::Previous {
        rows.reverse();
    }

    let data: Vec<Value> = rows.iter().map(SessionRow::info).collect();
    let previous = rows.first().map(|row| {
        encode_cursor(&SessionsCursor {
            query: query.clone(),
            anchor: Anchor {
                id: row.id.clone(),
                time: row.time_updated,
                direction: Direction::Previous,
            },
        })
    });
    let next = rows.last().map(|row| {
        encode_cursor(&SessionsCursor {
            query: query.clone(),
            anchor: Anchor {
                id: row.id.clone(),
                time: row.time_updated,
                direction: Direction::Next,
            },
        })
    });

    res.render(Json(json!({
        "data": data,
        "cursor": {
            "previous": previous.flatten(),
            "next": next.flatten(),
        },
    })));
}

/// Read the session-list filter query from the request.
fn request_query(req: &Request) -> Result<CursorQuery, String> {
    if let Some(raw) = req.query::<String>("directory")
        && raw.is_empty()
    {
        return Err("Invalid directory: empty value".to_owned());
    }
    let directory = req.query("directory");
    let project = req.query("project");
    let subpath = req.query("subpath");
    validate_scope(directory.as_deref(), project.as_deref(), subpath.as_deref())?;
    Ok(CursorQuery {
        order: resolve_order(req.query::<String>("order"))?,
        search: req.query("search"),
        parent_id: req.query("parentID"),
        directory,
        project,
        subpath,
        // Filled in by `list` so it can be validated even with a cursor.
        archived: None,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_scope;

    #[test]
    fn session_list_scope_is_a_union_of_three_shapes() {
        assert!(validate_scope(Some("/workspace"), None, None).is_ok());
        assert!(validate_scope(None, Some("project"), None).is_ok());
        assert!(validate_scope(None, Some("project"), Some("src")).is_ok());
        assert!(validate_scope(None, None, None).is_ok());

        assert!(validate_scope(Some("/workspace"), Some("project"), None).is_err());
        assert!(validate_scope(Some("/workspace"), None, Some("src")).is_err());
        assert!(validate_scope(None, None, Some("src")).is_err());
    }
}
