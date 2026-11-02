//! Rollout names: a memorable `word.word.word` derived from a rollout's id.
//!
//! The name encodes the id's leading 33 bits (its first nine hex digits) as
//! three 11-bit indices into the BIP-39 wordlist, so it is a pure function of
//! the id: the server and the UI derive it without storing it, and a name
//! narrows a lookup to the ids sharing an eight-digit prefix
//! ([`RolloutName::id_prefix`]). Names are not guaranteed unique (1 in 2^33
//! per pair); the server avoids issuing a taken one and refuses ambiguous
//! lookups.

use crate::words::WORDS;
use std::fmt;

const BITS: u32 = 11;
const MASK: u64 = (1 << BITS) - 1;
/// Hex digits read from the id: 36 bits, of which the top 33 are kept.
const DIGITS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RolloutName(u64);

impl RolloutName {
    /// The name of a rollout id, or `None` for an id that does not start with
    /// nine hex digits (hyphens are skipped, as in a UUID).
    pub fn of(id: &str) -> Option<Self> {
        let mut bits = 0u64;
        let mut digits = 0;
        for c in id.chars().filter(|c| *c != '-') {
            bits = bits << 4 | u64::from(c.to_digit(16)?);
            digits += 1;
            if digits == DIGITS {
                return Some(Self(bits >> 3));
            }
        }
        None
    }

    /// Parses a name typed by a person: case-insensitive, words separated by
    /// `.`, `-`, `_`, `/` or spaces (so `///w.w.w` works), and each word may
    /// be shortened to any prefix of four or more letters that names one word.
    pub fn parse(s: &str) -> Option<Self> {
        let mut words = s
            .split(|c: char| matches!(c, '.' | '-' | '_' | '/') || c.is_whitespace())
            .filter(|w| !w.is_empty());
        let mut bits = 0u64;
        for _ in 0..3 {
            bits = bits << BITS | word_index(&words.next()?.to_ascii_lowercase())? as u64;
        }
        words.next().is_none().then_some(Self(bits))
    }

    /// The first eight hex digits (lowercase) of every id with this name.
    pub fn id_prefix(&self) -> String {
        format!("{:08x}", self.0 >> 1)
    }
}

impl fmt::Display for RolloutName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = |shift: u32| WORDS[((self.0 >> shift) & MASK) as usize];
        write!(f, "{}.{}.{}", word(2 * BITS), word(BITS), word(0))
    }
}

/// The index of a word, or of the one word a 4+ letter prefix starts.
fn word_index(w: &str) -> Option<usize> {
    match WORDS.binary_search(&w) {
        Ok(i) => Some(i),
        Err(i) if w.len() >= 4 && WORDS.get(i)?.starts_with(w) => {
            let unique = WORDS.get(i + 1).is_none_or(|next| !next.starts_with(w));
            unique.then_some(i)
        }
        Err(_) => None,
    }
}

/// The display name of a rollout id, if it has one.
pub fn rollout_name(id: &str) -> Option<String> {
    RolloutName::of(id).map(|n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const ID: &str = "3f2a9c1e-8b4d-4f6a-9c2e-1d7b5a3e9f01";

    #[test]
    fn a_name_is_three_words_derived_from_the_id() {
        let name = rollout_name(ID).unwrap();
        assert_eq!(name.split('.').count(), 3);
        assert_eq!(rollout_name(ID), Some(name), "deterministic");
        assert_ne!(
            rollout_name(ID),
            rollout_name("3f2a9c1f-8b4d-4f6a-9c2e-1d7b5a3e9f01")
        );
    }

    #[test]
    fn a_name_round_trips_to_its_ids_prefix() {
        let name = RolloutName::of(ID).unwrap();
        assert_eq!(RolloutName::parse(&name.to_string()), Some(name));
        assert!(ID.starts_with(&name.id_prefix()));
    }

    #[rstest]
    #[case::extremes_of_the_bit_range("00000000-0")]
    #[case::all_bits_set("ffffffff-f")]
    fn every_bit_pattern_round_trips(#[case] id: &str) {
        let name = RolloutName::of(id).unwrap();
        assert_eq!(RolloutName::parse(&name.to_string()), Some(name));
        assert!(id.starts_with(&name.id_prefix()));
    }

    #[rstest]
    #[case::dots("abandon.ability.able")]
    #[case::w3w_slashes("///abandon.ability.able")]
    #[case::hyphens_and_case("Abandon-ABILITY-able")]
    #[case::spaces(" abandon  ability able ")]
    #[case::four_letter_prefixes("aban.abil.able")]
    fn typed_names_are_forgiving(#[case] input: &str) {
        assert_eq!(
            RolloutName::parse(input).map(|n| n.to_string()).as_deref(),
            Some("abandon.ability.able")
        );
    }

    #[rstest]
    #[case::two_words("abandon.ability")]
    #[case::four_words("abandon.ability.able.about")]
    #[case::unknown_word("abandon.ability.zzzz")]
    #[case::short_prefix("aba.ability.able")]
    #[case::uuid(ID)]
    fn malformed_names_are_rejected(#[case] input: &str) {
        assert_eq!(RolloutName::parse(input), None);
    }

    #[rstest]
    #[case::too_short("3f2a9c1e")]
    #[case::not_hex("rollout-42")]
    fn ids_without_nine_hex_digits_have_no_name(#[case] id: &str) {
        assert_eq!(rollout_name(id), None);
    }
}
