//! Confirmed session-family membership and activity, independent of transcript
//! hydration. Snapshots move in at the HTTP boundary; readers borrow one slice.
//! Execution events never grant membership. During one request a bounded overlay
//! preserves newer events, including events for members the answer first names.

use std::collections::{HashMap, HashSet};

use ox_schema::event::{EventEnvelope, SessionEventData};

use crate::rest::{FamilyMember, SessionTime};

#[derive(Debug, Default)]
struct Sync {
    generation: u64,
    running: HashMap<String, bool>,
    membership: HashSet<String>,
}

#[derive(Debug, Default)]
pub struct FamilyState {
    members: Vec<FamilyMember>,
    sync: Option<Sync>,
}

impl FamilyState {
    #[must_use]
    pub fn members(&self) -> &[FamilyMember] {
        &self.members
    }

    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.members.iter().any(|member| member.id == id)
    }

    #[must_use]
    pub fn is_running(&self, id: &str) -> bool {
        self.members
            .iter()
            .any(|member| member.id == id && member.running)
    }

    #[must_use]
    pub fn running_subagents(&self, focused: Option<&str>) -> usize {
        self.members
            .iter()
            .filter(|member| {
                member.parent_id.is_some() && Some(member.id.as_str()) != focused && member.running
            })
            .count()
    }

    pub fn begin_sync(&mut self, generation: u64) {
        self.sync = Some(Sync {
            generation,
            ..Default::default()
        });
    }

    /// Consume only this request. A failed request releases its overlay without
    /// inventing idle states; a later request can retry reconciliation.
    pub fn finish_sync(&mut self, generation: u64, members: Option<Vec<FamilyMember>>) -> bool {
        let Some(sync) = self.sync.take_if(|sync| sync.generation == generation) else {
            return false;
        };
        let Some(mut members) = members else {
            return false;
        };
        members.retain(|member| !sync.membership.contains(&member.id));
        for member in self.members.drain(..) {
            if sync.membership.contains(&member.id) {
                members.push(member);
            }
        }
        for member in &mut members {
            if let Some(running) = sync.running.get(&member.id) {
                member.running = *running;
            }
        }
        self.members = members;
        true
    }

    /// Observe session-level edges before the focused-session filter. Returns
    /// whether confirmed state changed; unknown execution edges remain untrusted.
    pub fn observe(
        &mut self,
        event: &EventEnvelope<SessionEventData>,
        focused: Option<&str>,
    ) -> bool {
        match &event.data {
            SessionEventData::SessionCreated {
                session_id,
                parent_id: Some(parent),
                title,
                agent,
                ..
            } if self.contains(parent) || focused == Some(parent.as_str()) => {
                if let Some(sync) = &mut self.sync {
                    sync.membership.insert(session_id.clone());
                }
                if self.contains(session_id) {
                    return false;
                }
                self.members.push(FamilyMember {
                    id: session_id.clone(),
                    title: title.clone(),
                    agent: agent.clone(),
                    parent_id: Some(parent.clone()),
                    time: SessionTime {
                        created: event.created,
                        ..Default::default()
                    },
                    running: false,
                });
                true
            }
            SessionEventData::SessionDeleted { session_id } => {
                let known = self.contains(session_id);
                if let Some(sync) = &mut self.sync
                    && (known
                        || sync.membership.len() < 256
                        || sync.membership.contains(session_id))
                {
                    sync.membership.insert(session_id.clone());
                }
                let before = self.members.len();
                self.members.retain(|member| member.id != *session_id);
                before != self.members.len()
            }
            SessionEventData::ExecutionStarted { session_id } => self.set_running(session_id, true),
            SessionEventData::ExecutionSucceeded { session_id }
            | SessionEventData::ExecutionFailed { session_id, .. }
            | SessionEventData::ExecutionInterrupted { session_id, .. } => {
                self.set_running(session_id, false)
            }
            _ => false,
        }
    }

    fn set_running(&mut self, id: &str, running: bool) -> bool {
        let known = self.contains(id);
        if let Some(sync) = &mut self.sync
            && (known || sync.running.len() < 256 || sync.running.contains_key(id))
        {
            sync.running.insert(id.to_owned(), running);
        }
        let Some(member) = self.members.iter_mut().find(|member| member.id == id) else {
            return false;
        };
        let changed = member.running != running;
        member.running = running;
        changed
    }
}
