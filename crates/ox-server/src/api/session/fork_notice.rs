use ox_schema::event::SessionEventData;

pub(crate) const NOTICE_TEXT: &str = "<fork-notice>\nThis conversation is a fork of another conversation, copied up to the previous message. The original conversation keeps going on its own and may be active right now, as may other forks of it.\n- Work started before the fork belongs to the original: background subagents, background shells, schedules, and follow-ups promised there. Their results and notifications go to the original, never here, and you can't continue those subagents. Don't wait for them, redo them, or report their results as yours; if it matters, check what already happened (files, git, PRs, CI).\n- Files, repositories, branches and external services are shared with the original. Before changing something the original was working on (its uncommitted changes, branches, worktrees, PRs, deploys, messages), check its current state, and put work that diverges from it on a new branch or worktree.\n- Otherwise work normally: new tasks need no extra checks, branches or questions.\n- If it's unclear whether this fork or the original should do something, ask.\n</fork-notice>";

pub(crate) fn notice_event(session_id: &str) -> SessionEventData {
    SessionEventData::InboxEnqueued {
        session_id: session_id.to_owned(),
        inbox_id: format!("msg_fork_notice_{session_id}"),
        item: ox_schema::inbox::Item::Synthetic {
            payload: ox_schema::inbox::SyntheticPayload {
                text: NOTICE_TEXT.to_owned(),
                description: Some("Fork notice".to_owned()),
                metadata: None,
            },
            delivery: ox_schema::inbox::Delivery::Steer,
        },
    }
}
