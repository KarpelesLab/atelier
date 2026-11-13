//! Columns, three ways.
//!
//! An LSP position counts a line's **UTF-16 code units** unless another
//! encoding was negotiated (this client negotiates none, so every server
//! agrees). Rust strings are indexed in **bytes**. And what the model reads
//! and writes are **characters**, counted from 1. On an ASCII line all three
//! are the same number, which is why getting it wrong goes unnoticed until
//! a line has an accent (2 bytes, 1 unit), a CJK character (3 bytes, 1 unit)
//! or an emoji (4 bytes, 2 units) before the symbol.
//!
//! Everything here works on one line of text, without its line ending, and
//! clamps instead of failing: a column past the end is the end.

/// The UTF-16 offset of the byte offset `byte` in `line`. An offset inside
/// a character counts as that character's start.
pub(crate) fn utf16_of_byte(line: &str, byte: usize) -> u32 {
    let before = line
        .char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= byte);
    before.map(|(_, c)| c.len_utf16() as u32).sum()
}

/// The byte offset of the UTF-16 offset `units` in `line`. An offset
/// between the two halves of a surrogate pair is the start of that
/// character.
pub(crate) fn byte_of_utf16(line: &str, units: u32) -> usize {
    let mut seen = 0;
    for (i, c) in line.char_indices() {
        if seen + c.len_utf16() as u32 > units {
            return i;
        }
        seen += c.len_utf16() as u32;
    }
    line.len()
}

/// The 1-based character column of the byte offset `byte` in `line`.
pub(crate) fn column_of_byte(line: &str, byte: usize) -> usize {
    let before = line
        .char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= byte);
    before.count() + 1
}

/// The byte offset of the 1-based character column `column` in `line`.
pub(crate) fn byte_of_column(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column.saturating_sub(1))
        .map_or(line.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_columns_are_all_the_same() {
        let line = "let x = foo(1);";
        for byte in 0..=line.len() {
            assert_eq!(utf16_of_byte(line, byte), byte as u32);
            assert_eq!(byte_of_utf16(line, byte as u32), byte);
            assert_eq!(column_of_byte(line, byte), byte + 1);
            assert_eq!(byte_of_column(line, byte + 1), byte);
        }
    }

    #[test]
    fn two_and_three_byte_characters_are_one_unit() {
        // é: 2 bytes, 1 unit. 日: 3 bytes, 1 unit.
        let line = "let é = 日本; foo()";
        let foo = line.find("foo").unwrap();
        assert_eq!(foo, 17);
        assert_eq!(utf16_of_byte(line, foo), 12);
        assert_eq!(byte_of_utf16(line, 12), foo);
        assert_eq!(column_of_byte(line, foo), 13);
        assert_eq!(byte_of_column(line, 13), foo);
    }

    #[test]
    fn astral_characters_are_two_units() {
        // 𝄞 (U+1D11E) and 🦀: 4 bytes, 2 UTF-16 units, 1 character each.
        let line = "\"𝄞🦀\"; bar";
        let bar = line.find("bar").unwrap();
        assert_eq!(bar, 12);
        assert_eq!(utf16_of_byte(line, bar), 8);
        assert_eq!(byte_of_utf16(line, 8), bar);
        assert_eq!(column_of_byte(line, bar), 7);
        assert_eq!(byte_of_column(line, 7), bar);
        // Every character boundary round-trips.
        for (byte, _) in line.char_indices() {
            assert_eq!(byte_of_utf16(line, utf16_of_byte(line, byte)), byte);
            assert_eq!(byte_of_column(line, column_of_byte(line, byte)), byte);
        }
    }

    #[test]
    fn offsets_inside_a_character_round_down_to_its_start() {
        let line = "a🦀b";
        // Bytes 1..5 are the crab; units 1..3.
        for inside in 2..5 {
            assert_eq!(utf16_of_byte(line, inside), 1);
            assert_eq!(column_of_byte(line, inside), 2);
        }
        // Unit 2 is the crab's low surrogate.
        assert_eq!(byte_of_utf16(line, 2), 1);
        assert_eq!(byte_of_utf16(line, 3), 5);
    }

    #[test]
    fn columns_past_the_end_are_the_end() {
        let line = "é";
        assert_eq!(utf16_of_byte(line, 99), 1);
        assert_eq!(byte_of_utf16(line, 99), 2);
        assert_eq!(column_of_byte(line, 99), 2);
        assert_eq!(byte_of_column(line, 99), 2);
        assert_eq!(byte_of_column(line, 0), 0);
        assert_eq!(byte_of_utf16("", 0), 0);
    }
}
