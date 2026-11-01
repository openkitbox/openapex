use anyhow::Result;
use salvo::prelude::*;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

use ox_schema::event::SessionEventData;
use ox_schema::id;
use ox_schema::support::SkillAttachment;

use super::inbox::load_inbox_row;
use super::response::{
    conflict_error, invalid_request, invalid_request_field, parse_body, session_not_found,
    skill_not_found, unknown_error,
};
use super::session_row::load_session;
use crate::api::{attachment, state};

// ---------------------------------------------------------------------------
// session.prompt — POST /api/session/{sessionID}/prompt
// ---------------------------------------------------------------------------

/// Request body of `session.prompt` (OpenAPI `PromptInput`): the prompt
/// fields plus admission controls. `delivery` defaults to `steer` upstream.
#[derive(Default, Deserialize)]
struct PromptBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Option<Vec<ox_schema::support::FileInput>>,
    #[serde(default)]
    agents: Option<Vec<ox_schema::support::AgentAttachment>>,
    #[serde(default)]
    skills: Option<Vec<ox_schema::support::SkillAttachment>>,
    #[serde(default)]
    metadata: Option<ox_schema::support::Record>,
    #[serde(default)]
    delivery: Option<ox_schema::inbox::Delivery>,
    #[serde(default)]
    resume: Option<bool>,
}

/// Resolve prompt skill attachments against an already-discovered skill set
/// (upstream `SessionPrompt.prepare`): ids become `{id, name, text}` with the
/// `<skill_content>` envelope, the first occurrence of an id carries the body
/// and later duplicates do not (upstream's de-dup), and an unknown id fails
/// the whole request with `SkillNotFoundError`.
///
/// Lookup is by id only — a display name in the `id` slot is an unknown id —
/// and touches no filesystem beyond a per-skill companion scan: discovery is
/// the caller's, one scan per admission answers every id here.
///
/// The returned `Err` id becomes a 404 `SkillNotFoundError`, which therefore
/// means either "no such skill" or "the skill exists but could not be read":
/// discovery skips unreadable roots and files, so both fail closed alike.
fn resolve_skills(
    skills: &[ox_tools::skills::Skill],
    requested: Option<Vec<SkillAttachment>>,
) -> Result<Option<Vec<SkillAttachment>>, String> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    if requested.is_empty() {
        return Ok(None);
    }
    // Resolved id -> display name, in first-occurrence order (upstream's
    // `prepared` map): a repeat emits its name but never the body again.
    let mut prepared: Vec<(String, String)> = Vec::with_capacity(requested.len());
    let mut resolved: Vec<SkillAttachment> = Vec::with_capacity(requested.len());
    for attachment in requested {
        let id = attachment.id;
        let mention = attachment.mention;
        if let Some((_, name)) = prepared.iter().find(|(known, _)| *known == id) {
            resolved.push(SkillAttachment {
                id,
                name: Some(name.clone()),
                text: None,
                mention,
            });
            continue;
        }
        let Some(skill) =
            ox_tools::skills::prepare_from(skills, &id, &ox_core::config::user_home())
        else {
            return Err(id);
        };
        prepared.push((skill.id.clone(), skill.name.clone()));
        resolved.push(SkillAttachment {
            id: skill.id,
            name: Some(skill.name),
            text: Some(skill.text),
            mention,
        });
    }
    Ok(Some(resolved))
}

#[handler] // operation_id: session.prompt
pub(crate) async fn prompt(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<PromptBody>(req, res).await else {
        return;
    };
    let Some(text) = body.text else {
        invalid_request(res, "Missing text");
        return;
    };
    let session = match load_session(&app.db, &session_id).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };
    let message_id = body.id.clone().unwrap_or_else(|| id::id("msg"));
    let delivery = body.delivery.unwrap_or(ox_schema::inbox::Delivery::Steer);
    // Upstream `admission.reconcile`: re-prompting with the same id and
    // delivery is idempotent; a delivery mismatch is a prompt conflict.
    let payload = match load_inbox_row(&app.db, &session_id, &message_id).await {
        Ok(Some(existing)) => {
            if existing.delivery != delivery {
                conflict_error(
                    res,
                    &format!(
                        "Prompt message ID conflicts with an existing durable record: {message_id}"
                    ),
                    &message_id,
                );
                return;
            }
            let info = existing.info;
            if body.resume != Some(false)
                && let Err(error) = app.runner.dispatch(&session_id, &message_id).await
            {
                unknown_error(res, &error);
                return;
            }
            res.render(Json(json!({ "data": info })));
            return;
        }
        // First admission only: skills resolve against the session's directory
        // (upstream `SessionPrompt.prepare`, which runs after the inbox
        // reconcile). A replay returns the payload stored back then — body
        // included, even if the skill was renamed or deleted since — so every
        // stored payload carries the resolved body while a replay scans
        // nothing. Discovery happens once here and answers every requested id,
        // and a prompt that requests no skill scans nothing at all.
        Ok(None) => {
            let discovered = if body
                .skills
                .as_ref()
                .is_some_and(|skills| !skills.is_empty())
            {
                ox_tools::skills::discover(Path::new(&session.directory))
            } else {
                Vec::new()
            };
            let skills = match resolve_skills(&discovered, body.skills) {
                Ok(skills) => skills,
                Err(skill) => {
                    skill_not_found(res, &skill);
                    return;
                }
            };
            // Upstream materializes `uri` inputs into stored attachments at
            // admission; file reads are blocking, so they run off the reactor.
            let files = match body.files {
                Some(inputs) if !inputs.is_empty() => {
                    match tokio::task::spawn_blocking(move || attachment::materialize(inputs)).await
                    {
                        Ok(Ok(files)) => Some(files),
                        Ok(Err(error)) => {
                            tracing::debug!(label = %error.label, "prompt attachment rejected");
                            invalid_request_field(res, &error.message, "files");
                            return;
                        }
                        Err(error) => {
                            unknown_error(res, &error.into());
                            return;
                        }
                    }
                }
                _ => None,
            };
            ox_schema::inbox::UserPayload {
                prompt: ox_schema::support::Prompt {
                    text,
                    files,
                    agents: body.agents,
                    skills,
                },
                metadata: body.metadata,
            }
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    // `payload` is cloned once, for the response; the original moves into the
    // event. Attachment data is `Arc<str>`, so the clone shares the big field.
    let info = ox_schema::inbox::Info::User {
        id: message_id.clone(),
        session_id: session_id.clone(),
        time: ox_schema::inbox::EnqueuedTime {
            created: id::now_millis(),
        },
        payload: payload.clone(),
        delivery,
    };
    if let Err(error) = app
        .bus
        .publish(SessionEventData::InboxEnqueued {
            session_id: session_id.clone(),
            inbox_id: message_id.clone(),
            item: ox_schema::inbox::Item::User { payload, delivery },
        })
        .await
    {
        unknown_error(res, &error);
        return;
    }
    // Upstream wakes the session unless `resume` is explicitly false.
    if body.resume != Some(false)
        && let Err(error) = app.runner.dispatch(&session_id, &message_id).await
    {
        unknown_error(res, &error);
        return;
    }
    res.render(Json(DataEnvelope { data: &info }));
}

/// `{"data": …}` around a borrowed value, so a prompt response does not copy
/// the payload (attachments can be megabytes) into a `serde_json::Value`.
#[derive(serde::Serialize)]
struct DataEnvelope<'a, T> {
    data: &'a T,
}
