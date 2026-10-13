//! Minimal DER encoding and walking, enough to build and read CMS SignedData and RFC 3161
//! messages.
//!
//! `read` and `children` accept DER only. CMS from other signers often comes in BER, with
//! indefinite lengths; `read_ber` and `children_ber` accept that too, and `normalize` turns such an
//! element back into DER where its encoding is hashed.

pub const SEQUENCE: u8 = 0x30;
pub const SET: u8 = 0x31;
pub const INTEGER: u8 = 0x02;
pub const OCTET_STRING: u8 = 0x04;
pub const NULL: u8 = 0x05;
pub const OID: u8 = 0x06;
pub const BOOLEAN: u8 = 0x01;

pub fn length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        let mut out = vec![0x80 | (bytes.len() - skip) as u8];
        out.extend_from_slice(&bytes[skip..]);
        out
    }
}

pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(length(content.len()));
    out.extend_from_slice(content);
    out
}

pub fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

pub fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    tlv(SEQUENCE, &concat(parts))
}

pub fn set(parts: &[&[u8]]) -> Vec<u8> {
    tlv(SET, &concat(parts))
}

pub fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(OCTET_STRING, bytes)
}

pub fn null() -> Vec<u8> {
    vec![NULL, 0]
}

pub fn boolean(value: bool) -> Vec<u8> {
    tlv(BOOLEAN, &[if value { 0xFF } else { 0x00 }])
}

pub fn integer_small(value: u32) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(3);
    let mut content = bytes[skip..].to_vec();
    if content[0] & 0x80 != 0 {
        content.insert(0, 0);
    }
    tlv(INTEGER, &content)
}

/// Positive big-endian integer (nonce, serial numbers).
pub fn integer_unsigned(bytes: &[u8]) -> Vec<u8> {
    let trimmed: &[u8] = {
        let skip = bytes.iter().take_while(|b| **b == 0).count().min(bytes.len().saturating_sub(1));
        &bytes[skip..]
    };
    let mut content = trimmed.to_vec();
    if content.first().is_some_and(|b| b & 0x80 != 0) {
        content.insert(0, 0);
    }
    tlv(INTEGER, &content)
}

/// Object identifier from its arcs.
pub fn oid(arcs: &[u32]) -> Vec<u8> {
    let mut content = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut stack = vec![(arc & 0x7F) as u8];
        let mut rest = arc >> 7;
        while rest > 0 {
            stack.push(0x80 | (rest & 0x7F) as u8);
            rest >>= 7;
        }
        stack.reverse();
        content.extend(stack);
    }
    tlv(OID, &content)
}

/// Context-specific tag; `constructed` marks the usual EXPLICIT/IMPLICIT-of-constructed case.
pub fn context(number: u8, constructed: bool, content: &[u8]) -> Vec<u8> {
    tlv(0x80 | number | if constructed { 0x20 } else { 0 }, content)
}

/// A parsed element: tag, content and the full encoding (header included).
#[derive(Debug, Clone, Copy)]
pub struct Element<'a> {
    pub tag: u8,
    pub content: &'a [u8],
    pub raw: &'a [u8],
}

/// Reads one element from the front of `data`, returning it and the remaining bytes.
pub fn read(data: &[u8]) -> Option<(Element<'_>, &[u8])> {
    read_with(data, None)
}

/// Like `read`, but also accepts the indefinite length BER allows for constructed elements. The
/// element's `content` then runs up to its end-of-contents marker, which `raw` includes.
pub fn read_ber(data: &[u8]) -> Option<(Element<'_>, &[u8])> {
    read_with(data, Some(0))
}

/// How deeply indefinite-length elements may nest (and how deep `normalize` goes).
const MAX_DEPTH: usize = 64;

/// `ber_depth` is `None` for DER, else how many indefinite-length elements enclose this one.
fn read_with(data: &[u8], ber_depth: Option<usize>) -> Option<(Element<'_>, &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)?;
    if first == 0x80 {
        let depth = ber_depth?;
        if tag & 0x20 == 0 || depth >= MAX_DEPTH {
            return None;
        }
        let mut rest = &data[2..];
        while !rest.starts_with(&[0, 0]) {
            rest = read_with(rest, Some(depth + 1))?.1;
        }
        let content_end = data.len() - rest.len();
        return Some((Element { tag, content: &data[2..content_end], raw: &data[..content_end + 2] }, &rest[2..]));
    }
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 4 || data.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for byte in &data[2..2 + n] {
            len = (len << 8) | *byte as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    if data.len() < end {
        return None;
    }
    Some((Element { tag, content: &data[header..end], raw: &data[..end] }, &data[end..]))
}

/// All elements directly inside `content`.
pub fn children(content: &[u8]) -> Vec<Element<'_>> {
    children_with(content, read)
}

/// All elements directly inside `content`, which may be BER.
pub fn children_ber(content: &[u8]) -> Vec<Element<'_>> {
    children_with(content, read_ber)
}

fn children_with<'a>(content: &'a [u8], read: fn(&'a [u8]) -> Option<(Element<'a>, &'a [u8])>) -> Vec<Element<'a>> {
    let mut out = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        match read(rest) {
            Some((element, next)) => {
                out.push(element);
                rest = next;
            }
            None => break,
        }
    }
    out
}

/// The value of a small non-negative INTEGER element's content.
pub fn small_unsigned(content: &[u8]) -> Option<u64> {
    if content.is_empty() || content[0] & 0x80 != 0 || content.len() > 9 {
        return None;
    }
    let digits = if content.len() == 9 && content[0] == 0 { &content[1..] } else { content };
    if digits.len() > 8 {
        return None;
    }
    Some(digits.iter().fold(0u64, |value, b| (value << 8) | *b as u64))
}

/// The DER encoding of a (possibly BER) element: definite lengths throughout, and a string sent
/// in pieces joined into one. `None` when it nests too deeply or does not parse.
pub fn normalize(element: &Element<'_>) -> Option<Vec<u8>> {
    normalize_at(element, 0)
}

fn normalize_at(element: &Element<'_>, depth: usize) -> Option<Vec<u8>> {
    if element.tag & 0x20 == 0 {
        return Some(tlv(element.tag, element.content));
    }
    if depth >= MAX_DEPTH {
        return None;
    }
    // Constructed OCTET STRING and BIT STRING are BER only; DER sends them in one piece.
    if matches!(element.tag, 0x24 | 0x23) {
        return Some(tlv(element.tag & !0x20, &string_value(element)?));
    }
    let mut content = Vec::with_capacity(element.content.len());
    let mut rest = element.content;
    while !rest.is_empty() {
        let (child, next) = read_ber(rest)?;
        content.extend(normalize_at(&child, depth + 1)?);
        rest = next;
    }
    Some(tlv(element.tag, &content))
}

/// The bytes of a string element (OCTET STRING, or an implicitly tagged one), joining the pieces of
/// a constructed (BER) string.
pub fn string_value(element: &Element<'_>) -> Option<Vec<u8>> {
    string_value_at(element, 0)
}

fn string_value_at(element: &Element<'_>, depth: usize) -> Option<Vec<u8>> {
    if element.tag & 0x20 == 0 {
        return Some(element.content.to_vec());
    }
    if depth >= MAX_DEPTH {
        return None;
    }
    let mut out = Vec::new();
    let mut rest = element.content;
    while !rest.is_empty() {
        let (piece, next) = read_ber(rest)?;
        out.extend(string_value_at(&piece, depth + 1)?);
        rest = next;
    }
    Some(out)
}

/// Decodes an OID element's content to dotted-decimal text.
pub fn oid_to_string(content: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut value: u64 = 0;
    for (i, byte) in content.iter().enumerate() {
        value = (value << 7) | (*byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            if i == content.iter().position(|_| true).unwrap_or(0) && parts.is_empty() {
                parts.push((value / 40).min(2).to_string());
                parts.push((value - (value / 40).min(2) * 40).to_string());
            } else {
                parts.push(value.to_string());
            }
            value = 0;
        }
    }
    parts.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_and_integers() {
        assert_eq!(length(5), vec![5]);
        assert_eq!(length(200), vec![0x81, 200]);
        assert_eq!(length(0x1234), vec![0x82, 0x12, 0x34]);
        assert_eq!(integer_small(1), vec![2, 1, 1]);
        assert_eq!(integer_small(128), vec![2, 2, 0, 0x80]);
        assert_eq!(integer_unsigned(&[0, 0, 0xFF]), vec![2, 2, 0, 0xFF]);
    }

    #[test]
    fn oid_round_trip() {
        // sha256: 2.16.840.1.101.3.4.2.1
        let encoded = oid(&[2, 16, 840, 1, 101, 3, 4, 2, 1]);
        assert_eq!(encoded, vec![0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01]);
        let (element, _) = read(&encoded).unwrap();
        assert_eq!(oid_to_string(element.content), "2.16.840.1.101.3.4.2.1");
        let rsa = oid(&[1, 2, 840, 113549, 1, 1, 11]);
        let (element, _) = read(&rsa).unwrap();
        assert_eq!(oid_to_string(element.content), "1.2.840.113549.1.1.11");
    }

    #[test]
    fn indefinite_lengths_are_read_as_ber_only() {
        let ber = [0x30, 0x80, 0x02, 0x01, 0x07, 0x00, 0x00, 0xAA];
        assert!(read(&ber).is_none(), "DER has no indefinite lengths");
        let (element, rest) = read_ber(&ber).unwrap();
        assert_eq!(element.content, &[0x02, 0x01, 0x07]);
        assert_eq!(element.raw, &ber[..7]);
        assert_eq!(rest, &[0xAA]);
        assert_eq!(normalize(&element).unwrap(), vec![0x30, 0x03, 0x02, 0x01, 0x07]);

        // Nested indefinite lengths, a constructed OCTET STRING sent in two pieces.
        let nested = [0xA0, 0x80, 0x24, 0x80, 0x04, 0x01, b'a', 0x04, 0x02, b'b', b'c', 0x00, 0x00, 0x00, 0x00];
        let (outer, rest) = read_ber(&nested).unwrap();
        assert!(rest.is_empty());
        let inner = children_ber(outer.content);
        assert_eq!(inner.len(), 1);
        assert_eq!(string_value(&inner[0]).unwrap(), b"abc");
        assert_eq!(normalize(&outer).unwrap(), vec![0xA0, 0x05, 0x04, 0x03, b'a', b'b', b'c']);

        // Primitive elements cannot be indefinite, and nesting is bounded.
        assert!(read_ber(&[0x04, 0x80, 0x00, 0x00]).is_none());
        let mut deep: Vec<u8> = std::iter::repeat([0x30, 0x80]).take(200).flatten().collect();
        deep.extend(std::iter::repeat(0).take(400));
        assert!(read_ber(&deep).is_none());
    }

    #[test]
    fn small_integers() {
        assert_eq!(small_unsigned(&[0x14]), Some(20));
        assert_eq!(small_unsigned(&[0x00, 0x80]), Some(128));
        assert_eq!(small_unsigned(&[0x80]), None);
        assert_eq!(small_unsigned(&[]), None);
    }

    #[test]
    fn walk_nested_structures() {
        let data = sequence(&[&integer_small(7), &set(&[&octet_string(b"abc")])]);
        let (root, rest) = read(&data).unwrap();
        assert!(rest.is_empty());
        let kids = children(root.content);
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].tag, INTEGER);
        assert_eq!(children(kids[1].content)[0].content, b"abc");
    }
}
