//! Human-readable session slugs, ported from
//! `packages/core/src/util/slug.ts` (`Slug.create`): `"adjective-noun"`.

use rand::Rng;

const ADJECTIVES: [&str; 29] = [
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy",
    "hidden", "jolly", "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud",
    "quick", "quiet", "shiny", "silent", "stellar", "sunny", "swift", "tidy", "witty",
];

const NOUNS: [&str; 30] = [
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest",
    "garden", "harbor", "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula",
    "orchid", "otter", "panda", "pixel", "planet", "river", "rocket", "sailor", "squid", "star",
    "tiger", "wizard",
];

/// `Slug.create()`: a random `"adjective-noun"` pair.
pub fn create() -> String {
    let mut rng = rand::rng();
    let adjective = ADJECTIVES[rng.random_range(..ADJECTIVES.len())];
    let noun = NOUNS[rng.random_range(..NOUNS.len())];
    format!("{adjective}-{noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_adjective_dash_noun() {
        let slug = create();
        let (adjective, noun) = slug.split_once('-').unwrap_or(("", ""));
        assert!(ADJECTIVES.contains(&adjective));
        assert!(NOUNS.contains(&noun));
    }
}
