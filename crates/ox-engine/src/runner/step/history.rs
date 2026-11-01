use anyhow::{Context as _, Result};
use serde_json::Value;

use ox_schema::message::Message;
/// The session transcript in `seq` order, decoded from `session_message`.
pub(crate) async fn load_history(db: &ox_db::Db, session_id: &str) -> Result<Vec<Message>> {
    load_history_after(db, session_id, i64::MIN).await
}

/// The text of the newest assistant row, if the transcript has one. Used by the
/// todo reminder to tell a stop from a hand-off to the user. Reads the tail
/// only: the reminder runs once per turn end, and a full transcript scan there
/// would be wasteful on long sessions.
pub(crate) async fn last_assistant_text(
    db: &ox_db::Db,
    session_id: &str,
) -> Result<Option<String>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT data FROM session_message
             WHERE session_id = ?1 AND type = 'assistant'
             ORDER BY seq DESC LIMIT 1",
        )
        .await?;
    let Some(row) = ox_db::optional(&mut stmt, [session_id]).await? else {
        return Ok(None);
    };
    let raw: String = row.get(0)?;
    let value: Value = serde_json::from_str(&raw).context("decode session_message.data")?;
    let message =
        Message::from_row_data("", "assistant", value).context("decode assistant message row")?;
    Ok(match message {
        Message::Assistant(assistant) => Some(
            assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    ox_schema::message::AssistantContent::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    })
}

/// Load a session transcript above `floor`: rows with a greater ordinal. The
/// magic-context head stands in for everything at or below the floor it
/// reports, so the request carries only the live tail.
pub(crate) async fn load_history_after(
    db: &ox_db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<Message>> {
    rows_after(db, session_id, floor)
        .await?
        .into_iter()
        .map(|row| decode_row(&row))
        .collect()
}

/// The tail with the magic-context §N§ overlay applied: tags minted (stable
/// across passes), tag prefixes rendered beside each taggable item, and the
/// content of an applied drop replaced by its placeholder.
pub(crate) async fn load_overlaid_tail(
    db: &ox_db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<Message>> {
    let mut rows = rows_after(db, session_id, floor).await?;
    let tags = crate::mctx::tagger::Tagger
        .assign(db, session_id, &rows)
        .await
        .context("assign §N§ tags")?;
    let drops = crate::mctx::tagger::drops(db, session_id)
        .await
        .context("load drops")?;
    crate::mctx::tagger::apply_overlay(
        &mut rows,
        &tags,
        &drops,
        crate::mctx::tagger::OverlayMode::Request,
    );
    rows.into_iter().map(decode_row_owned).collect()
}

/// One `session_message` row as the decoded message JSON: the row's `data`
/// column with `id`/`type` merged back in, exactly the shape the tagger's
/// `part_path` addresses (`decode({ ...row.data, id, type })` upstream).
/// [`decode_row`] reads `id`/`type` off the same object.
async fn rows_after(
    db: &ox_db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<serde_json::Value>> {
    Ok(rows_after_with_seq(db, session_id, floor)
        .await?
        .into_iter()
        .map(|(_, value)| value)
        .collect())
}

/// As [`rows_after`], but each row is paired with its `seq` ordinal. The
/// historian addresses coverage by ordinal (`seq`), which the decoded message
/// JSON does not carry, so it reads the tail through this variant.
pub(crate) async fn rows_after_with_seq(
    db: &ox_db::Db,
    session_id: &str,
    floor: i64,
) -> Result<Vec<(i64, serde_json::Value)>> {
    let conn = db.acquire().await?;
    let mut stmt = conn
        .prepare(
            "SELECT id, type, data, seq FROM session_message
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )
        .await?;
    let mut rows = stmt.query((session_id, floor)).await?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let data: String = row.get(2)?;
        let seq: i64 = row.get(3)?;
        // Parsed inside the loop, not after collecting the raw JSON: holding
        // every row's string and every row's value at once would double the
        // tail's footprint for the whole load.
        let mut value: serde_json::Value =
            serde_json::from_str(&data).context("decode session_message.data")?;
        Message::merge_row_columns(&id, &kind, &mut value);
        values.push((seq, value));
    }
    Ok(values)
}

pub(crate) fn decode_row(value: &serde_json::Value) -> Result<Message> {
    decode_row_owned(value.clone())
}

/// [`decode_row`] without the row copy.
///
/// Both tail loaders hold the whole transcript as JSON values already (the §N§
/// overlay is applied across rows before anything is decoded), so each row's
/// value is dead the moment it is decoded: merging `id`/`type` in place and
/// deserializing from the moved value keeps the tail from being copied a second
/// time on the way into [`Message`]s.
pub(crate) fn decode_row_owned(value: serde_json::Value) -> Result<Message> {
    if value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        anyhow::bail!("session_message row without an id");
    }
    if value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        anyhow::bail!("session_message row without a type");
    }
    Ok(Message::from_merged_row(value)?)
}
