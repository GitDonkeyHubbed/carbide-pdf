//! Low-level helpers for adding content and resources to an existing page without disturbing it.

use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream};

use crate::error::Result;
use crate::fonts::StandardFont;
use crate::pdf::Pdf;

impl Pdf {
    /// Appends `stamp` after the page's existing content. The original content is wrapped in
    /// `q`/`Q` so that any graphics state it leaves behind cannot affect the stamp.
    pub(crate) fn append_content(&mut self, page_id: ObjectId, stamp: Vec<u8>) -> Result<()> {
        let existing = self.doc.get_page_contents(page_id);
        let open = self
            .doc
            .add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
        let close = self
            .doc
            .add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
        let stamp_id = self.doc.add_object(Stream::new(Dictionary::new(), stamp));

        let mut list: Vec<Object> = vec![Object::Reference(open)];
        list.extend(existing.into_iter().map(Object::Reference));
        list.push(Object::Reference(close));
        list.push(Object::Reference(stamp_id));
        self.doc
            .get_dictionary_mut(page_id)?
            .set("Contents", Object::Array(list));
        Ok(())
    }

    /// Adds `value` under `/Resources/<category>` on the page and returns the unique resource name.
    ///
    /// Inherited or indirect resource dictionaries are copied onto the page first, so other pages
    /// that share them are not modified.
    pub(crate) fn add_page_resource(
        &mut self,
        page_id: ObjectId,
        category: &str,
        base_name: &str,
        value: Object,
    ) -> Result<String> {
        let mut resources = self
            .inherited(page_id, b"Resources")
            .and_then(|o| o.as_dict().ok().cloned())
            .unwrap_or_default();
        let mut group = match resources.get(category.as_bytes()) {
            Ok(entry) => self
                .resolve(entry)
                .as_dict()
                .ok()
                .cloned()
                .unwrap_or_default(),
            Err(_) => Dictionary::new(),
        };
        let mut n = 1;
        let name = loop {
            let candidate = format!("{base_name}{n}");
            if !group.has(candidate.as_bytes()) {
                break candidate;
            }
            n += 1;
        };
        group.set(name.as_bytes().to_vec(), value);
        resources.set(category.as_bytes().to_vec(), Object::Dictionary(group));
        self.doc
            .get_dictionary_mut(page_id)?
            .set("Resources", Object::Dictionary(resources));
        Ok(name)
    }

    /// Registers one of the standard fonts on the page and returns its resource name.
    pub(crate) fn add_standard_font(
        &mut self,
        page_id: ObjectId,
        font: StandardFont,
    ) -> Result<String> {
        let font_id = self.doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => font.base_font(),
            "Encoding" => "WinAnsiEncoding",
        });
        self.add_page_resource(page_id, "Font", "CarbideF", Object::Reference(font_id))
    }

    /// Registers a constant-alpha graphics state (for both fills and strokes).
    pub(crate) fn add_alpha_state(&mut self, page_id: ObjectId, alpha: f32) -> Result<String> {
        let state = dictionary! {
            "Type" => "ExtGState",
            "ca" => Object::Real(alpha),
            "CA" => Object::Real(alpha),
        };
        self.add_page_resource(page_id, "ExtGState", "CarbideGS", Object::Dictionary(state))
    }
}

/// One token of a content stream, as [`tokens`] gives them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Token<'a> {
    Number(f32),
    /// A name, as written after its `/` (see [`name`] for what it stands for).
    Name(&'a [u8]),
    Operator(&'a [u8]),
    /// Any other operand or part of one: a string, `true`, `false`, `null`, or a bracket that
    /// starts or ends an array or dictionary.
    Other,
}

/// Goes through the content stream `data` token by token, without building a list of its
/// operations: lopdf's parser makes a few hundred bytes of objects for every operator, so a
/// stream of a few megabytes of `q` would take gigabytes. The data of inline images, from the `ID` that follows a `BI`, is skipped.
pub(crate) fn tokens<'a>(data: &'a [u8], mut visit: impl FnMut(Token<'a>)) {
    let is_space = |b: u8| matches!(b, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ');
    let is_delimiter = |b: u8| {
        matches!(
            b,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
    };
    let mut at = 0;
    // Whether a `BI` has begun an inline image whose data has not started: an `ID` stands for the
    // start of data only after one.
    let mut in_image_header = false;
    while at < data.len() {
        let byte = data[at];
        if is_space(byte) {
            at += 1;
            continue;
        }
        match byte {
            b'%' => {
                while at < data.len() && !matches!(data[at], b'\r' | b'\n') {
                    at += 1;
                }
            }
            b'(' => {
                let mut depth = 0usize;
                while at < data.len() {
                    match data[at] {
                        b'\\' => at += 1,
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    at += 1;
                }
                at += 1;
                visit(Token::Other);
            }
            b'<' if data.get(at + 1) == Some(&b'<') => {
                at += 2;
                visit(Token::Other);
            }
            b'<' => {
                at += data[at..]
                    .iter()
                    .position(|&b| b == b'>')
                    .map_or(data.len(), |end| end + 1);
                visit(Token::Other);
            }
            b'>' => {
                at += if data.get(at + 1) == Some(&b'>') {
                    2
                } else {
                    1
                };
                visit(Token::Other);
            }
            b'[' | b']' | b'{' | b'}' | b')' => {
                at += 1;
                visit(Token::Other);
            }
            b'/' => {
                let start = at + 1;
                at = start;
                while at < data.len() && !is_space(data[at]) && !is_delimiter(data[at]) {
                    at += 1;
                }
                visit(Token::Name(&data[start..at]));
            }
            _ => {
                let start = at;
                while at < data.len() && !is_space(data[at]) && !is_delimiter(data[at]) {
                    at += 1;
                }
                let word = &data[start..at];
                if word
                    .iter()
                    .all(|b| matches!(b, b'0'..=b'9' | b'+' | b'-' | b'.'))
                {
                    visit(
                        std::str::from_utf8(word)
                            .ok()
                            .and_then(|w| w.parse().ok())
                            .map_or(Token::Other, Token::Number),
                    );
                } else if matches!(word, b"true" | b"false" | b"null") {
                    visit(Token::Other);
                } else {
                    visit(Token::Operator(word));
                    if word == b"BI" {
                        in_image_header = true;
                    } else if word == b"ID" && std::mem::take(&mut in_image_header) {
                        // Inline image data runs from the byte after ID up to EI standing alone.
                        let image = at + 1;
                        let end = (image..data.len().saturating_sub(1)).find(|&i| {
                            data[i..].starts_with(b"EI")
                                && data.get(i.wrapping_sub(1)).is_some_and(|&b| is_space(b))
                                && data
                                    .get(i + 2)
                                    .is_none_or(|&b| is_space(b) || is_delimiter(b))
                        });
                        at = end.map_or(data.len(), |end| end + 2);
                        visit(Token::Operator(b"EI"));
                    }
                }
            }
        }
    }
}

/// The name a name token stands for: `#` followed by two hexadecimal digits is the byte they give.
pub(crate) fn name(written: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !written.contains(&b'#') {
        return std::borrow::Cow::Borrowed(written);
    }
    let mut out = Vec::with_capacity(written.len());
    let mut at = 0;
    while at < written.len() {
        let hex = written
            .get(at + 1..at + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (written[at], hex) {
            (b'#', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(data: &[u8]) -> Vec<Token<'_>> {
        let mut out = Vec::new();
        tokens(data, |token| out.push(token));
        out
    }

    #[test]
    fn content_is_read_token_by_token() {
        let data = b"q 1 0 0 1 -2.5 .5 cm /Im#201 Do % a comment /NotAName\n(a (nested) \\) string /NotAName) Tj \
<</MCID 3>> BDC [(x) -20 /Y] TJ <414243> Tj EMC Q";
        assert_eq!(
            all(data),
            vec![
                Token::Operator(b"q"),
                Token::Number(1.0),
                Token::Number(0.0),
                Token::Number(0.0),
                Token::Number(1.0),
                Token::Number(-2.5),
                Token::Number(0.5),
                Token::Operator(b"cm"),
                Token::Name(b"Im#201"),
                Token::Operator(b"Do"),
                Token::Other,
                Token::Operator(b"Tj"),
                Token::Other,
                Token::Name(b"MCID"),
                Token::Number(3.0),
                Token::Other,
                Token::Operator(b"BDC"),
                Token::Other,
                Token::Other,
                Token::Number(-20.0),
                Token::Name(b"Y"),
                Token::Other,
                Token::Operator(b"TJ"),
                Token::Other,
                Token::Operator(b"Tj"),
                Token::Operator(b"EMC"),
                Token::Operator(b"Q"),
            ]
        );
        assert_eq!(&*name(b"Im#201"), b"Im 1");
    }

    #[test]
    fn inline_image_data_is_skipped() {
        let data = b"BI /W 2 /H 1 /CS /RGB ID \x00/Fake EI\xffEI Q EI\n/After Do";
        let names: Vec<_> = all(data)
            .into_iter()
            .filter_map(|t| {
                if let Token::Name(n) = t {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(names, vec![&b"W"[..], b"H", b"CS", b"RGB", b"After"]);
    }
}
