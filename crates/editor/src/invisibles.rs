//! Detection of characters the tail toggle (`hulder::ToggleTail`) reveals:
//! things that render as nothing, as an ordinary space, or as an ASCII
//! letter while being something else entirely.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvisibleKind {
    /// A code point with no visible width (U+200B..U+200F, U+2060, U+FEFF).
    ZeroWidth,
    /// A bidirectional control that can reorder what is displayed
    /// (U+202A..U+202E, U+2066..U+2069).
    BidiControl,
    /// U+00A0, which looks exactly like a space.
    NonBreakingSpace,
    /// A non-ASCII letter that looks like an ASCII letter, inside a word that
    /// also contains ASCII letters (`pаyload`) or whose letters are all
    /// lookalikes (`ѕсоре`).
    Confusable(char),
}

/// Finds every character in `text` that the tail toggle should highlight,
/// returning byte ranges in ascending order.
///
/// Confusables are only reported inside suspicious words, like rustc's
/// `mixed_script_confusables` lint: ordinary Cyrillic or Greek prose is
/// not a homoglyph attack. A word is suspicious when it mixes ASCII letters
/// with lookalikes, or when every letter in it has an ASCII lookalike.
/// Zero-width and bidi controls never split a word, so they cannot be used
/// to hide a mixed-script token from the check.
#[cfg(test)]
pub fn find_invisibles(text: &str) -> Vec<(Range<usize>, InvisibleKind)> {
    find_invisibles_in_chunks(std::iter::once(text))
}

/// Like [`find_invisibles`], but over text that arrives in pieces (for
/// example a rope's chunks), so the buffer never has to be materialized.
/// Byte ranges are relative to the start of the first chunk; words may span
/// chunk boundaries.
pub fn find_invisibles_in_chunks<'a>(
    chunks: impl IntoIterator<Item = &'a str>,
) -> Vec<(Range<usize>, InvisibleKind)> {
    let mut found = Vec::new();
    let mut word = Word::default();
    let mut chunk_start = 0;
    for chunk in chunks {
        for (offset, character) in chunk.char_indices() {
            let offset = chunk_start + offset;
            let range = offset..offset + character.len_utf8();
            word.feed(range, character, &mut found);
        }
        chunk_start += chunk.len();
    }
    word.flush_into(&mut found);
    found
}

#[derive(Default)]
struct Word {
    has_ascii_letter: bool,
    has_other_letter: bool,
    /// Confusables and embedded controls in text order; which of them are
    /// reported is decided once the word ends.
    pending: Vec<(Range<usize>, InvisibleKind)>,
}

impl Word {
    fn feed(
        &mut self,
        range: Range<usize>,
        character: char,
        found: &mut Vec<(Range<usize>, InvisibleKind)>,
    ) {
        match classify_control(character) {
            Some(kind @ (InvisibleKind::ZeroWidth | InvisibleKind::BidiControl)) => {
                self.pending.push((range, kind));
            }
            Some(kind) => {
                self.flush_into(found);
                found.push((range, kind));
            }
            None if character.is_alphanumeric() || character == '_' => {
                self.push(range, character);
            }
            None => self.flush_into(found),
        }
    }

    fn push(&mut self, range: Range<usize>, character: char) {
        if character.is_ascii_alphabetic() {
            self.has_ascii_letter = true;
        } else if let Some(ascii) = confusable_ascii(character) {
            self.pending.push((range, InvisibleKind::Confusable(ascii)));
        } else if character.is_alphabetic() {
            self.has_other_letter = true;
        }
    }

    fn flush_into(&mut self, found: &mut Vec<(Range<usize>, InvisibleKind)>) {
        let has_confusable = self
            .pending
            .iter()
            .any(|(_, kind)| matches!(kind, InvisibleKind::Confusable(_)));
        let suspicious = has_confusable && (self.has_ascii_letter || !self.has_other_letter);
        if suspicious {
            found.append(&mut self.pending);
        } else {
            found.extend(
                self.pending
                    .drain(..)
                    .filter(|(_, kind)| !matches!(kind, InvisibleKind::Confusable(_))),
            );
        }
        self.has_ascii_letter = false;
        self.has_other_letter = false;
    }
}

fn classify_control(character: char) -> Option<InvisibleKind> {
    match character {
        '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}' => Some(InvisibleKind::ZeroWidth),
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => Some(InvisibleKind::BidiControl),
        '\u{00A0}' => Some(InvisibleKind::NonBreakingSpace),
        _ => None,
    }
}

/// Maps a Cyrillic, Greek or non-ASCII Latin letter to the ASCII letter it
/// is commonly mistaken for, if any.
fn confusable_ascii(character: char) -> Option<char> {
    let ascii = match character {
        // Cyrillic lowercase
        'а' => 'a',
        'е' => 'e',
        'о' => 'o',
        'р' => 'p',
        'с' => 'c',
        'у' => 'y',
        'х' => 'x',
        'ѕ' => 's',
        'і' => 'i',
        'ј' => 'j',
        'һ' => 'h',
        'ԁ' => 'd',
        'ԛ' => 'q',
        'ԝ' => 'w',
        'ӏ' => 'l',
        // Latin small letter script g (U+0261)
        'ɡ' => 'g',
        // Cyrillic uppercase
        'А' => 'A',
        'В' => 'B',
        'Е' => 'E',
        'К' => 'K',
        'М' => 'M',
        'Н' => 'H',
        'О' => 'O',
        'Р' => 'P',
        'С' => 'C',
        'Т' => 'T',
        'Х' => 'X',
        'Ѕ' => 'S',
        'І' => 'I',
        'Ј' => 'J',
        'Ӏ' => 'I',
        // Greek lowercase
        'ο' => 'o',
        'ν' => 'v',
        'ι' => 'i',
        'κ' => 'k',
        // Greek uppercase
        'Α' => 'A',
        'Β' => 'B',
        'Ε' => 'E',
        'Ζ' => 'Z',
        'Η' => 'H',
        'Ι' => 'I',
        'Κ' => 'K',
        'Μ' => 'M',
        'Ν' => 'N',
        'Ο' => 'O',
        'Ρ' => 'P',
        'Τ' => 'T',
        'Υ' => 'Y',
        'Χ' => 'X',
        _ => return None,
    };
    Some(ascii)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_has_no_invisibles() {
        assert!(find_invisibles("fn main() { let x = 1; }").is_empty());
    }

    #[test]
    fn finds_non_breaking_space() {
        let text = "a\u{00A0}b";
        assert_eq!(
            find_invisibles(text),
            vec![(1..3, InvisibleKind::NonBreakingSpace)]
        );
    }

    #[test]
    fn finds_zero_width_space() {
        let text = "ab\u{200B}c";
        assert_eq!(
            find_invisibles(text),
            vec![(2..5, InvisibleKind::ZeroWidth)]
        );
    }

    #[test]
    fn finds_cyrillic_a_confusable() {
        let text = "pаypal";
        assert_eq!(
            find_invisibles(text),
            vec![(1..3, InvisibleKind::Confusable('a'))]
        );
    }

    #[test]
    fn finds_bidi_control_and_bom() {
        let text = "\u{FEFF}x\u{202E}y";
        assert_eq!(
            find_invisibles(text),
            vec![
                (0..3, InvisibleKind::ZeroWidth),
                (4..7, InvisibleKind::BidiControl),
            ]
        );
    }

    #[test]
    fn single_script_cyrillic_prose_is_not_flagged() {
        assert!(find_invisibles("Сегодня хорошая погода").is_empty());
        assert!(find_invisibles("Ελληνικά").is_empty());
    }

    #[test]
    fn flags_confusable_only_in_mixed_script_word() {
        let text = "pаyload";
        assert_eq!(
            find_invisibles(text),
            vec![(1..3, InvisibleKind::Confusable('a'))]
        );
        assert_eq!(
            find_invisibles("да pаyload"),
            vec![(6..8, InvisibleKind::Confusable('a'))],
            "an ordinary Cyrillic word next to a mixed word is left alone"
        );
    }

    #[test]
    fn flags_word_made_only_of_lookalikes() {
        let kinds = |text: &str| {
            find_invisibles(text)
                .into_iter()
                .map(|(_, kind)| kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds("ѕсоре"),
            vec![
                InvisibleKind::Confusable('s'),
                InvisibleKind::Confusable('c'),
                InvisibleKind::Confusable('o'),
                InvisibleKind::Confusable('p'),
                InvisibleKind::Confusable('e'),
            ]
        );
        assert_eq!(kinds("аррӏе").len(), 5);
        assert!(kinds("Москва").is_empty());
        assert!(kinds("123").is_empty());
    }

    #[test]
    fn zero_width_characters_do_not_split_a_word() {
        let text = "pа\u{200B}yload";
        assert_eq!(
            find_invisibles(text),
            vec![
                (1..3, InvisibleKind::Confusable('a')),
                (3..6, InvisibleKind::ZeroWidth),
            ]
        );
        let text = "а\u{200B}б";
        assert_eq!(
            find_invisibles(text),
            vec![(2..5, InvisibleKind::ZeroWidth)],
            "the control is still reported inside an innocent word"
        );
    }

    #[test]
    fn chunked_scan_matches_whole_text_scan() {
        let text = "x\u{00A0}pаy\u{200B}load ѕсоре\nДобрый день";
        let whole = find_invisibles(text);
        assert!(!whole.is_empty());
        for split in 1..text.len() {
            if !text.is_char_boundary(split) {
                continue;
            }
            let (left, right) = text.split_at(split);
            assert_eq!(
                find_invisibles_in_chunks([left, right]),
                whole,
                "split at {split}"
            );
        }
    }

    #[test]
    fn reports_every_match_in_order() {
        let text = "\u{00A0}\u{200B}xа";
        let kinds: Vec<_> = find_invisibles(text)
            .into_iter()
            .map(|(_, kind)| kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                InvisibleKind::NonBreakingSpace,
                InvisibleKind::ZeroWidth,
                InvisibleKind::Confusable('a'),
            ]
        );
    }
}
