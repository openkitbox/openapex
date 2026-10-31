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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, parent: Option<&str>) -> FamilyMember {
        FamilyMember {
            id: id.to_string(),
            title: None,
            agent: None,
            time: Default::default(),
            running: false,
            parent_id: parent.map(str::to_string),
        }
    }

    fn rows(sessions: &[FamilyMember], current: &str) -> Vec<(String, String)> {
        family(sessions, current)
            .into_iter()
            .map(|e| (e.session.id.clone(), e.prefix))
            .collect()
    }

    fn row(id: &str, prefix: &str) -> (String, String) {
        (id.to_string(), prefix.to_string())
    }

    #[test]
    fn unknown_current_is_empty() {
        assert!(rows(&[s("a", None)], "zzz").is_empty());
    }

    #[test]
    fn lone_root_is_empty() {
        assert!(rows(&[s("a", None)], "a").is_empty());
    }

    #[test]
    fn first_level_children_have_empty_prefix() {
        let l = [s("a", None), s("b", Some("a")), s("c", Some("a"))];
        assert_eq!(rows(&l, "a"), vec![row("b", ""), row("c", "")]);
    }

    #[test]
    fn grandchildren_carry_branch_marker_only() {
        // First-level ancestors list is `[]`, grandchildren's is
        // `[lastOfLevel1]`, whose `slice(1)` is empty: no continuation bar.
        let l = [
            s("a", None),
            s("b", Some("a")),
            s("c", Some("a")),
            s("b1", Some("b")),
            s("b2", Some("b")),
            s("c1", Some("c")),
        ];
        assert_eq!(
            rows(&l, "a"),
            vec![
                row("b", ""),
                row("b1", "├─ "),
                row("b2", "└─ "),
                row("c", ""),
                row("c1", "└─ "),
            ]
        );
    }

    #[test]
    fn great_grandchildren_use_both_continuation_kinds() {
        let l = [
            s("a", None),
            s("b", Some("a")),
            s("b1", Some("b")), // not last under b
            s("b2", Some("b")), // last under b
            s("b1x", Some("b1")),
            s("b2x", Some("b2")),
        ];
        assert_eq!(
            rows(&l, "a"),
            vec![
                row("b", ""),
                row("b1", "├─ "),
                row("b1x", "│  └─ "),
                row("b2", "└─ "),
                row("b2x", "   └─ "),
            ]
        );
    }

    #[test]
    fn child_resolves_to_whole_tree() {
        let l = [s("a", None), s("b", Some("a")), s("c", Some("a"))];
        assert_eq!(rows(&l, "c"), rows(&l, "a"));
        assert_eq!(rows(&l, "c").len(), 2);
    }

    #[test]
    fn missing_parent_makes_session_the_root() {
        let l = [s("b", Some("gone")), s("c", Some("b"))];
        assert_eq!(rows(&l, "c"), vec![row("c", "")]);
    }

    #[test]
    fn parent_cycle_terminates() {
        let l = [s("a", Some("b")), s("b", Some("a"))];
        let out = rows(&l, "a");
        assert_eq!(out.len(), 1);
    }
}
