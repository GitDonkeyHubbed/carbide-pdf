//! The standard-14 fonts used for stamps, with their advance widths and WinAnsi text encoding.
//!
//! Widths are in 1/1000 em for ASCII 32..=126 and were checked against the system Helvetica
//! metrics. Characters outside the table fall back to an average width, so centering of text with
//! accents or symbols is approximate.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardFont {
    Helvetica,
    HelveticaBold,
    Courier,
}

const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

impl StandardFont {
    pub fn base_font(self) -> &'static str {
        match self {
            StandardFont::Helvetica => "Helvetica",
            StandardFont::HelveticaBold => "Helvetica-Bold",
            StandardFont::Courier => "Courier",
        }
    }

    /// Distance from the baseline to the top of a capital letter, per em.
    pub fn cap_height(self) -> f32 {
        match self {
            StandardFont::Courier => 0.562,
            _ => 0.718,
        }
    }

    fn advance(self, byte: u8) -> f32 {
        let table: &[u16; 95] = match self {
            StandardFont::Helvetica => &HELVETICA,
            StandardFont::HelveticaBold => &HELVETICA_BOLD,
            StandardFont::Courier => return 600.0,
        };
        match byte {
            32..=126 => table[(byte - 32) as usize] as f32,
            _ => 556.0,
        }
    }

    /// Width of `text` at `size` points.
    pub fn text_width(self, text: &str, size: f32) -> f32 {
        win_ansi(text).iter().map(|&b| self.advance(b)).sum::<f32>() * size / 1000.0
    }
}

/// Encodes text for a font using `/WinAnsiEncoding`. Unmappable characters become `?`.
pub fn win_ansi(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match c as u32 {
            0x20..=0x7E | 0xA0..=0xFF => c as u8,
            _ => match c {
                '€' => 0x80,
                '‚' => 0x82,
                'ƒ' => 0x83,
                '„' => 0x84,
                '…' => 0x85,
                '†' => 0x86,
                '‡' => 0x87,
                'ˆ' => 0x88,
                '‰' => 0x89,
                'Š' => 0x8A,
                '‹' => 0x8B,
                'Œ' => 0x8C,
                'Ž' => 0x8E,
                '‘' => 0x91,
                '’' => 0x92,
                '“' => 0x93,
                '”' => 0x94,
                '•' => 0x95,
                '–' => 0x96,
                '—' => 0x97,
                '˜' => 0x98,
                '™' => 0x99,
                'š' => 0x9A,
                '›' => 0x9B,
                'œ' => 0x9C,
                'ž' => 0x9E,
                'Ÿ' => 0x9F,
                _ => b'?',
            },
        })
        .collect()
}

/// A PDF literal string (`(...)`) for already-encoded bytes.
pub fn literal_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'(');
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            0x20..=0x7E => out.push(b),
            _ => out.extend_from_slice(format!("\\{:03o}", b).as_bytes()),
        }
    }
    out.push(b')');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_match_known_values() {
        // "Hello" in Helvetica: H722 e556 l222 l222 o556 = 2278 per 1000 em.
        let w = StandardFont::Helvetica.text_width("Hello", 1000.0);
        assert!((w - 2278.0).abs() < 0.5, "{w}");
        assert!((StandardFont::Courier.text_width("abcd", 10.0) - 24.0).abs() < 0.01);
    }

    #[test]
    fn encoding_and_escaping() {
        assert_eq!(win_ansi("é€?"), vec![0xE9, 0x80, b'?']);
        assert_eq!(win_ansi("日"), vec![b'?']);
        assert_eq!(literal_string(b"a(b)\\"), b"(a\\(b\\)\\\\)".to_vec());
        assert_eq!(literal_string(&[0xE9]), b"(\\351)".to_vec());
    }
}
