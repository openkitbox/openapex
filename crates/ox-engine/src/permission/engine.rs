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
