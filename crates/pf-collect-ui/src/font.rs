//! A tiny 5x7 bitmap font, hand-authored here (public-domain, fully owned — no vendored table, no
//! RE material). Each glyph is 7 rows of a 5-bit pattern; bit 4 (`0b10000`) is the leftmost column.
//! Text is uppercased before lookup, so only uppercase letters, digits, and the punctuation the
//! collection prompts actually use are defined. An unknown char renders as a hollow box.

pub const GLYPH_W: usize = 5;
pub const GLYPH_H: usize = 7;

/// The 7 row-patterns for a character (uppercased). Bit 4 = leftmost of the 5 columns.
pub fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b01110, 0b10001, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b10001, 0b01110],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        ',' => [0, 0, 0, 0, 0b01100, 0b01100, 0b01000],
        ':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        '/' => [0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000],
        '(' => [0b00110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b00110],
        ')' => [0b01100, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01100],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        '\'' => [0b00100, 0b00100, 0b01000, 0, 0, 0, 0],
        // Unknown glyph -> a hollow box, so a missing char is visible, not silent.
        _ => [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
    }
}

/// The pixel width of a string rendered at `scale` (glyph columns + 1 col spacing, times scale).
pub fn text_width(s: &str, scale: usize) -> usize {
    if s.is_empty() {
        return 0;
    }
    let n = s.chars().count();
    // n glyphs of GLYPH_W columns + (n-1) single-column gaps, all scaled.
    (n * GLYPH_W + n.saturating_sub(1)) * scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_defined_glyph_is_five_bits() {
        // No row may set a bit above the 5-column width (0b11111 = 0x1F).
        for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,:/()-!'".chars() {
            for row in glyph(c) {
                assert!(row <= 0b11111, "glyph {c:?} row {row:#07b} exceeds 5 columns");
            }
        }
    }

    #[test]
    fn text_width_scales() {
        assert_eq!(text_width("", 2), 0);
        // one glyph: 5 columns * scale
        assert_eq!(text_width("A", 3), 5 * 3);
        // two glyphs: 5 + 1 gap + 5 = 11 columns * scale
        assert_eq!(text_width("AB", 2), 11 * 2);
    }
}
