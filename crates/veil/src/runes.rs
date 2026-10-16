//! Elder Futhark transliteration. One char in, one char out, so gpui can map glyphs back
//! onto the original text byte for byte.

/// Runes for the digits `0` to `9`, by index.
const DIGIT_RUNES: [char; 10] = ['ᛜ', 'ᛝ', 'ᛞ', 'ᚦ', 'ᛃ', 'ᛈ', 'ᛖ', 'ᛒ', 'ᛟ', 'ᚨ'];

/// The rune drawn in place of `ch`. Letters are case-folded, digits map by index and everything
/// else is returned unchanged.
pub fn rune_for(ch: char) -> char {
    match ch.to_ascii_lowercase() {
        'a' => 'ᚨ',
        'b' => 'ᛒ',
        'c' => 'ᚲ',
        'd' => 'ᛞ',
        'e' => 'ᛖ',
        'f' => 'ᚠ',
        'g' => 'ᚷ',
        'h' => 'ᚺ',
        'i' => 'ᛁ',
        'j' => 'ᛃ',
        'k' => 'ᚲ',
        'l' => 'ᛚ',
        'm' => 'ᛗ',
        'n' => 'ᚾ',
        'o' => 'ᛟ',
        'p' => 'ᛈ',
        'q' => 'ᚲ',
        'r' => 'ᚱ',
        's' => 'ᛊ',
        't' => 'ᛏ',
        'u' => 'ᚢ',
        'v' => 'ᚹ',
        'w' => 'ᚹ',
        // The spec's `ᚲᛊ` digraph would break the one-char contract, so `x` takes `ᚲ` alone.
        'x' => 'ᚲ',
        'y' => 'ᛇ',
        'z' => 'ᛉ',
        digit @ '0'..='9' => DIGIT_RUNES[(digit as u8 - b'0') as usize],
        _ => ch,
    }
}

/// Transliterates every char of `text` with [`rune_for`].
pub fn transliterate(text: &str) -> String {
    text.chars().map(rune_for).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_case_folded_and_punctuation_is_kept() {
        assert_eq!(transliterate("Hello, World!"), "ᚺᛖᛚᛚᛟ, ᚹᛟᚱᛚᛞ!");
        assert_eq!(transliterate("fn main() {}"), "ᚠᚾ ᛗᚨᛁᚾ() {}");
    }

    #[test]
    fn digits_map_by_index() {
        assert_eq!(transliterate("0123456789"), "ᛜᛝᛞᚦᛃᛈᛖᛒᛟᚨ");
    }

    #[test]
    fn every_letter_has_a_rune() {
        assert_eq!(
            transliterate("abcdefghijklmnopqrstuvwxyz"),
            "ᚨᛒᚲᛞᛖᚠᚷᚺᛁᛃᚲᛚᛗᚾᛟᛈᚲᚱᛊᛏᚢᚹᚹᚲᛇᛉ"
        );
        assert_eq!(rune_for('x'), 'ᚲ');
        assert_eq!(rune_for('X'), 'ᚲ');
    }

    #[test]
    fn one_char_in_one_char_out() {
        let ascii: String = (0u8..128).map(char::from).collect();
        assert_eq!(transliterate(&ascii).chars().count(), ascii.chars().count());
        let mixed = "ünïcödé 日本語 → ᚠ";
        assert_eq!(transliterate(mixed).chars().count(), mixed.chars().count());
    }

    #[test]
    fn non_ascii_passes_through() {
        assert_eq!(transliterate("日本語 → ᚠ"), "日本語 → ᚠ");
        assert_eq!(transliterate("\t\n"), "\t\n");
    }
}
