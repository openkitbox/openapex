//! Pure rule evaluation, ported from upstream
//! `packages/core/src/permission/policy.ts:74-113` and
//! `packages/core/src/permission.ts:195-226`.

use ox_schema::permission::{Effect, Rule};

/// Resource actions whose patterns are file paths, as in upstream
/// `PermissionPolicy.pathActions`.
const PATH_ACTIONS: [&str; 3] = ["external_directory", "read", "edit"];

fn ignores_case(action: &str) -> bool {
    cfg!(target_os = "macos") && PATH_ACTIONS.contains(&action)
}

/// Evaluate ordered rule layers, then saved approvals as an ask-only fallback,
/// then optional unattended rules. `layers` are in precedence order (agent,
/// config, session); within and across layers the last matching rule wins.
#[must_use]
pub fn evaluate(
    layers: &[&[Rule]],
    saved: &[Rule],
    unattended: Option<&[Rule]>,
    action: &str,
    resources: &[String],
) -> Effect {
    let mut asked = false;
    for resource in resources {
        let base = base_effect(layers, action, resource).unwrap_or(Effect::Ask);
        if base == Effect::Deny {
            return Effect::Deny;
        }
        let effect = if base == Effect::Ask {
            last_match(saved, action, resource).map_or(Effect::Ask, |rule| rule.effect)
        } else {
            base
        };
        match effect {
            Effect::Deny => return Effect::Deny,
            Effect::Ask => asked = true,
            Effect::Allow => {}
        }
    }
    let effect = if asked { Effect::Ask } else { Effect::Allow };

    match unattended {
        None => effect,
        Some(_) if effect != Effect::Ask => effect,
        Some(rules) => {
            for resource in resources {
                let mut settled = base_effect(layers, action, resource).unwrap_or(Effect::Ask);
                if settled == Effect::Ask {
                    settled =
                        last_match(saved, action, resource).map_or(Effect::Ask, |rule| rule.effect);
                }
                if let Some(override_rule) = last_match(rules, action, resource) {
                    settled = override_rule.effect;
                }
                if settled != Effect::Allow {
                    return Effect::Deny;
                }
            }
            Effect::Allow
        }
    }
}

fn base_effect(layers: &[&[Rule]], action: &str, resource: &str) -> Option<Effect> {
    let mut found = None;
    for layer in layers {
        if let Some(rule) = last_match(layer, action, resource) {
            found = Some(rule.effect);
        }
    }
    found
}

fn last_match<'a>(rules: &'a [Rule], action: &str, resource: &str) -> Option<&'a Rule> {
    let ignore_case = ignores_case(action);
    rules.iter().rev().find(|rule| {
        wildcard_match(action, &rule.action, false)
            && wildcard_match(resource, &rule.resource, ignore_case)
    })
}

/// Upstream `Wildcard.match` without constructing a `Regex`: backslashes are
/// slashes, regex metacharacters are literal, `*` spans anything and `?` spans
/// one character. A trailing `" *"` compiles to `( .*)?`, so an exact match
/// against the prefix also succeeds.
fn wildcard_match(input: &str, pattern: &str, ignore_case: bool) -> bool {
    if pattern
        .strip_suffix(" *")
        .is_some_and(|prefix| wildcard_core(input, prefix, ignore_case))
    {
        return true;
    }
    wildcard_core(input, pattern, ignore_case)
}

fn split_char(text: &str) -> Option<(char, usize)> {
    let ch = text.chars().next()?;
    Some((ch, ch.len_utf8()))
}

fn normalized(ch: char) -> char {
    if ch == '\\' { '/' } else { ch }
}

fn char_matches(left: char, right: char, ignore_case: bool) -> bool {
    if ignore_case {
        normalized(left)
            .to_lowercase()
            .eq(normalized(right).to_lowercase())
    } else {
        normalized(left) == normalized(right)
    }
}

fn wildcard_core(input: &str, pattern: &str, ignore_case: bool) -> bool {
    let (mut input, mut pattern) = (input, pattern);
    loop {
        let Some((pattern_ch, pattern_width)) = split_char(pattern) else {
            return input.is_empty();
        };
        pattern = &pattern[pattern_width..];
        if pattern_ch == '*' {
            let rest = pattern.trim_start_matches('*');
            let mut remaining = input;
            loop {
                if wildcard_core(remaining, rest, ignore_case) {
                    return true;
                }
                let Some((_, width)) = split_char(remaining) else {
                    return false;
                };
                remaining = &remaining[width..];
            }
        }
        let Some((input_ch, input_width)) = split_char(input) else {
            return false;
        };
        if pattern_ch == '?' || char_matches(pattern_ch, input_ch, ignore_case) {
            input = &input[input_width..];
        } else {
            return false;
        }
    }
}

#[cfg(test)]
mod permission_rules_tests {
    use ox_schema::permission::{Effect, Rule};

    use super::evaluate;

    fn rule(action: &str, resource: &str, effect: Effect) -> Rule {
        Rule {
            action: action.to_owned(),
            resource: resource.to_owned(),
            effect,
        }
    }

    #[test]
    fn last_match_wins_within_a_layer_and_across_layers() {
        let inner = [
            rule("*", "*", Effect::Allow),
            rule("shell", "git*", Effect::Deny),
        ];
        assert_eq!(
            evaluate(&[&inner], &[], None, "shell", &["git push".to_owned()]),
            Effect::Deny,
            "a later rule in the same layer must override an earlier one"
        );

        let early = [rule("shell", "git*", Effect::Deny)];
        let late = [rule("*", "*", Effect::Allow)];
        assert_eq!(
            evaluate(
                &[&early, &late],
                &[],
                None,
                "shell",
                &["git push".to_owned()]
            ),
            Effect::Allow,
            "a later layer must override an earlier layer"
        );
        assert_eq!(
            evaluate(
                &[&late, &early],
                &[],
                None,
                "shell",
                &["git push".to_owned()]
            ),
            Effect::Deny,
            "layer order is significant"
        );
    }

    #[test]
    fn base_deny_is_not_overridden_by_saved_or_unattended() {
        let base = [
            rule("*", "*", Effect::Allow),
            rule("shell", "rm -rf*", Effect::Deny),
        ];
        let saved = [rule("*", "*", Effect::Allow)];
        let unattended = [rule("*", "*", Effect::Allow)];
        assert_eq!(
            evaluate(
                &[&base],
                &saved,
                None,
                "shell",
                &["rm -rf build".to_owned()]
            ),
            Effect::Deny,
            "deny short-circuits before saved fallback"
        );
        assert_eq!(
            evaluate(
                &[&base],
                &saved,
                Some(&unattended),
                "shell",
                &["rm -rf build".to_owned()]
            ),
            Effect::Deny,
            "unattended rules must not undo a deny"
        );
    }

    #[test]
    fn saved_only_settles_ask_and_never_overrides_allow() {
        let base = [
            rule("*", "*", Effect::Allow),
            rule("read", ".env*", Effect::Ask),
        ];
        let saved = [rule("read", ".env.local", Effect::Allow)];
        assert_eq!(
            evaluate(&[&base], &saved, None, "read", &[".env.local".to_owned()]),
            Effect::Allow,
            "saved approval settles an ask"
        );
        assert_eq!(
            evaluate(
                &[&base],
                &saved,
                None,
                "read",
                &[".env.production".to_owned()]
            ),
            Effect::Ask,
            "an unmatched ask stays an ask"
        );

        let deny_saved = [rule("*", "*", Effect::Deny)];
        assert_eq!(
            evaluate(
                &[&base],
                &deny_saved,
                None,
                "shell",
                &["git status".to_owned()]
            ),
            Effect::Allow,
            "saved rules never override an allow"
        );
    }

    #[test]
    fn unmatched_rules_ask_and_no_resources_allow() {
        assert_eq!(
            evaluate(&[], &[], None, "shell", &["git status".to_owned()]),
            Effect::Ask,
            "upstream's default effect is ask"
        );
        assert_eq!(
            evaluate(&[], &[], None, "shell", &[]),
            Effect::Allow,
            "evaluateAll of zero resources is allow"
        );
    }

    #[test]
    fn any_resource_deny_or_ask_wins_over_allow() {
        let base = [
            rule("*", "*", Effect::Allow),
            rule("*", "bar", Effect::Ask),
            rule("*", "baz", Effect::Deny),
        ];
        assert_eq!(
            evaluate(
                &[&base],
                &[],
                None,
                "shell",
                &["foo".to_owned(), "bar".to_owned()]
            ),
            Effect::Ask
        );
        assert_eq!(
            evaluate(
                &[&base],
                &[],
                None,
                "shell",
                &["bar".to_owned(), "baz".to_owned()]
            ),
            Effect::Deny,
            "deny has the highest precedence"
        );
    }

    #[test]
    fn wildcard_matches_upstream_special_forms() {
        let base = [
            rule("*", "*", Effect::Allow),
            rule("shell", "npm test *", Effect::Ask),
            rule("read", "src/?.rs", Effect::Deny),
            rule("read", "a.b", Effect::Deny),
            rule("read", "src\\foo.rs", Effect::Deny),
        ];
        assert_eq!(
            evaluate(&[&base], &[], None, "shell", &["npm test".to_owned()]),
            Effect::Ask,
            "trailing ' *' is optional"
        );
        assert_eq!(
            evaluate(
                &[&base],
                &[],
                None,
                "shell",
                &["npm test -- --watch".to_owned()]
            ),
            Effect::Ask,
            "trailing ' *' covers an arbitrary suffix"
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "shell", &["npm tests".to_owned()]),
            Effect::Allow,
            "the space in ' *' is literal"
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "read", &["src/a.rs".to_owned()]),
            Effect::Deny,
            "'?' matches one character"
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "read", &["src/ab.rs".to_owned()]),
            Effect::Allow,
            "'?' does not match two characters"
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "read", &["a.b".to_owned()]),
            Effect::Deny,
            "'.' in a pattern is literal"
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "read", &["axb".to_owned()]),
            Effect::Allow,
            "regex metacharacters are escaped"
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "read", &["src/foo.rs".to_owned()]),
            Effect::Deny,
            "backslashes in patterns are normalized to slashes"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_path_actions_ignore_case_but_shell_does_not() {
        let base = [
            rule("*", "*", Effect::Allow),
            rule("read", ".env*", Effect::Ask),
            rule("edit", "*.rs", Effect::Deny),
            rule("external_directory", "/etc*", Effect::Deny),
            rule("shell", "git push*", Effect::Ask),
        ];
        assert_eq!(
            evaluate(&[&base], &[], None, "read", &[".ENV.local".to_owned()]),
            Effect::Ask
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "edit", &["SRC/Foo.RS".to_owned()]),
            Effect::Deny
        );
        assert_eq!(
            evaluate(
                &[&base],
                &[],
                None,
                "external_directory",
                &["/ETC/passwd".to_owned()]
            ),
            Effect::Deny
        );
        assert_eq!(
            evaluate(&[&base], &[], None, "shell", &["GIT PUSH".to_owned()]),
            Effect::Allow,
            "shell resources remain case-sensitive"
        );
    }

    #[test]
    fn unattended_only_settles_ask_results() {
        let ask = [rule("*", "*", Effect::Ask)];
        let allow_all = [rule("*", "*", Effect::Allow)];
        let deny_all = [rule("*", "*", Effect::Deny)];
        assert_eq!(
            evaluate(
                &[&ask],
                &[],
                Some(&allow_all),
                "shell",
                &["git status".to_owned()]
            ),
            Effect::Allow
        );
        assert_eq!(
            evaluate(&[&ask], &[], Some(&[]), "shell", &["git status".to_owned()]),
            Effect::Deny,
            "unattended with no matching allow rule fails closed"
        );

        let partial = [rule("shell", "git status", Effect::Allow)];
        assert_eq!(
            evaluate(
                &[&ask],
                &[],
                Some(&partial),
                "shell",
                &["git status".to_owned()]
            ),
            Effect::Allow
        );
        assert_eq!(
            evaluate(
                &[&ask],
                &[],
                Some(&partial),
                "shell",
                &["git status".to_owned(), "git push".to_owned()]
            ),
            Effect::Deny,
            "every resource must be allowed for unattended to pass"
        );

        let allow_base = [rule("*", "*", Effect::Allow)];
        assert_eq!(
            evaluate(
                &[&allow_base],
                &[],
                Some(&deny_all),
                "shell",
                &["git status".to_owned()]
            ),
            Effect::Allow,
            "unattended rules only apply to a result that would ask"
        );
    }
}
