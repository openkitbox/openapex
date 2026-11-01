//! Session family tree: the flat family list arranged under its root with
//! upstream's tree prefixes.
//!
//! Port of upstream `sessionFamily` (`packages/tui/src/util/session.ts`).
//! Deliberate deviations: the walk up to the root takes at most
//! `sessions.len()` steps, and every session is visited at most once, so a
//! parent cycle in the data cannot loop forever (upstream has no guard).

use std::collections::{HashMap, HashSet};

use ox_client::rest::FamilyMember;

pub struct FamilyEntry<'a> {
    pub session: &'a FamilyMember,
    pub prefix: String,
}

/// Every session in the tree containing `current`, except the root, depth
/// first with siblings in input order. Empty when `current` is unknown.
#[must_use]
pub fn family<'a>(sessions: &'a [FamilyMember], current: &str) -> Vec<FamilyEntry<'a>> {
    let by_id: HashMap<&str, &FamilyMember> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let Some(&start) = by_id.get(current) else {
        return Vec::new();
    };

    let mut children: HashMap<&str, Vec<&FamilyMember>> = HashMap::new();
    for session in sessions {
        if let Some(parent) = session.parent_id.as_deref() {
            children.entry(parent).or_default().push(session);
        }
    }

    // Climb while the parent is in the list; the step bound breaks cycles.
    let mut root = start;
    for _ in 0..sessions.len() {
        match root.parent_id.as_deref().and_then(|p| by_id.get(p)) {
            Some(&parent) => root = parent,
            None => break,
        }
    }

    let mut out = Vec::new();
    let mut seen = HashSet::from([root.id.as_str()]);
    walk(&root.id, &mut Vec::new(), &children, &mut seen, &mut out);
    out
}

/// `ancestors[i]` is whether the ancestor at that depth was the last of its
/// siblings. Mirrors upstream `walk`.
fn walk<'a>(
    parent: &str,
    ancestors: &mut Vec<bool>,
    children: &HashMap<&str, Vec<&'a FamilyMember>>,
    seen: &mut HashSet<&'a str>,
    out: &mut Vec<FamilyEntry<'a>>,
) {
    let Some(group) = children.get(parent) else {
        return;
    };
    for (index, &session) in group.iter().enumerate() {
        if !seen.insert(session.id.as_str()) {
            continue;
        }
        let last = index == group.len() - 1;
        let prefix = if ancestors.is_empty() {
            String::new()
        } else {
            let mut prefix: String = ancestors[1..]
                .iter()
                .map(|&ancestor_last| if ancestor_last { "   " } else { "│  " })
                .collect();
            prefix.push_str(if last { "└─ " } else { "├─ " });
            prefix
        };
        out.push(FamilyEntry { session, prefix });
        ancestors.push(last);
        walk(&session.id, ancestors, children, seen, out);
        ancestors.pop();
    }
}
