//! Opening encrypted files with the file key their password really gives.
//!
//! For the older encryption revisions (RC4 and AES-128) the file key is derived from the open
//! password, encoded as PDFDocEncoding. A file opened with its owner password, with a password
//! that is not plain ASCII, or one whose owner password is empty, needs that open password
//! recovered or encoded first: a key derived from the typed text instead garbles every string and
//! stream, and saving the document again would destroy its content. So an encrypted file is
//! loaded with its objects still encrypted, the password is checked against the encryption
//! dictionary, and only then are the objects decrypted, with the key of the open password.

use std::collections::{BTreeMap, HashMap};

use lopdf::encryption::decrypt_object;
use lopdf::xref::XrefEntry;
use lopdf::{Document, Object, ObjectId, ObjectStream};
use md5::{Digest, Md5};

use crate::error::{Error, Result};

/// Loads `bytes`, decrypting them when the file is encrypted. Without a password only files that
/// open with an empty one (permission-only protection) can be read.
pub(crate) fn load(bytes: &[u8], password: Option<&str>) -> Result<Document> {
    let Some(mut doc) = load_still_encrypted(bytes)? else {
        let doc = Document::load_mem(bytes)?;
        if doc.is_encrypted() || doc.was_encrypted() {
            // The encryption dictionary is named in a way the loader above did not find.
            return Err(Error::Unsupported("this file's encryption could not be read".into()));
        }
        return Ok(doc);
    };
    let revision = doc
        .get_encrypted()
        .and_then(|dict| dict.get(b"R"))
        .and_then(Object::as_i64)
        .map_err(|_| Error::Unsupported("this file's encryption dictionary is damaged".into()))?;

    // An empty password first: a file that needs none opens without asking, as elsewhere.
    let mut candidates = vec![Candidate::Raw(Vec::new())];
    if let Some(password) = password {
        if revision >= 5 {
            // Prepared the way the standard says (SASLprep), with the exact bytes as a fallback.
            candidates.push(Candidate::Text(password.to_string()));
        } else if let Some(encoded) = pdf_doc_encoding(password) {
            candidates.push(Candidate::Raw(encoded));
        }
        candidates.push(Candidate::Raw(password.as_bytes().to_vec()));
    }
    for candidate in candidates {
        let open = match candidate {
            Candidate::Raw(bytes) if revision <= 4 => open_password_r4(&doc, &bytes).map(Candidate::Raw),
            Candidate::Raw(bytes) => doc.authenticate_raw_password(&bytes).is_ok().then_some(Candidate::Raw(bytes)),
            Candidate::Text(text) => doc.authenticate_password(&text).is_ok().then_some(Candidate::Text(text)),
        };
        if let Some(open) = open {
            decrypt(&mut doc, &open)?;
            return Ok(doc);
        }
    }
    Err(if password.is_some() { Error::WrongPassword } else { Error::PasswordRequired })
}

enum Candidate {
    /// Used byte for byte.
    Raw(Vec<u8>),
    /// Prepared by lopdf the way the encryption revision asks for.
    Text(String),
}

/// The file with its trailer's `/Encrypt` hidden from the loader, so every object is read as it is
/// stored, still encrypted, and with object streams left packed (their content is encrypted too).
/// `None` when the file is not encrypted.
fn load_still_encrypted(bytes: &[u8]) -> Result<Option<Document>> {
    let Some(hidden) = rename_key(bytes, ENCRYPT, HIDDEN_ENCRYPT) else { return Ok(None) };
    let hidden = rename_key(&hidden, OBJECT_STREAM, HIDDEN_OBJECT_STREAM).unwrap_or(hidden);
    let mut doc = Document::load_mem(&hidden)?;
    let Some(encrypt) = doc.trailer.remove(&HIDDEN_ENCRYPT[1..]) else { return Ok(None) };
    doc.trailer.set("Encrypt", encrypt);
    for object in doc.objects.values_mut() {
        restore(object);
    }
    Ok(Some(doc))
}

const ENCRYPT: &[u8] = b"/Encrypt";
const HIDDEN_ENCRYPT: &[u8] = b"/Cb-Encr";
const OBJECT_STREAM: &[u8] = b"/ObjStm";
const HIDDEN_OBJECT_STREAM: &[u8] = b"/Cb-ObS";

/// `bytes` with every name `key` (followed by whitespace or a delimiter, so `/EncryptMetadata` is
/// left alone) replaced by `hidden`, which has the same length so no offset moves. `None` when
/// there is none.
fn rename_key(bytes: &[u8], key: &[u8], hidden: &[u8]) -> Option<Vec<u8>> {
    debug_assert_eq!(key.len(), hidden.len());
    let mut out: Option<Vec<u8>> = None;
    let mut at = 0;
    while let Some(found) = find(&bytes[at..], key) {
        let start = at + found;
        let end = start + key.len();
        if bytes.get(end).is_some_and(|&b| ends_name(b)) {
            out.get_or_insert_with(|| bytes.to_vec())[start..end].copy_from_slice(hidden);
        }
        at = end;
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn ends_name(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// Undoes the renaming wherever it hit something other than the trailer: a dictionary key or
/// type, or (by chance) bytes inside a string or stream.
fn restore(object: &mut Object) {
    match object {
        Object::Name(name) if name.as_slice() == &HIDDEN_OBJECT_STREAM[1..] => *name = OBJECT_STREAM[1..].to_vec(),
        Object::Name(name) if name.as_slice() == &HIDDEN_ENCRYPT[1..] => *name = ENCRYPT[1..].to_vec(),
        Object::String(bytes, _) => restore_bytes(bytes),
        Object::Array(items) => items.iter_mut().for_each(restore),
        Object::Dictionary(dict) => restore_dict(dict),
        Object::Stream(stream) => {
            restore_dict(&mut stream.dict);
            restore_bytes(&mut stream.content);
        }
        _ => {}
    }
}

fn restore_dict(dict: &mut lopdf::Dictionary) {
    if let Some(value) = dict.remove(&HIDDEN_ENCRYPT[1..]) {
        dict.set(&ENCRYPT[1..], value);
    }
    for (_, value) in dict.iter_mut() {
        restore(value);
    }
}

fn restore_bytes(bytes: &mut Vec<u8>) {
    for (hidden, key) in [(HIDDEN_ENCRYPT, ENCRYPT), (HIDDEN_OBJECT_STREAM, OBJECT_STREAM)] {
        if let Some(restored) = rename_key(bytes, hidden, key) {
            *bytes = restored;
        }
    }
}

/// The open password, padded to 32 bytes when it was recovered, when `password` opens a file of
/// encryption revision 2 to 4 as its open password or as its owner password.
fn open_password_r4(doc: &Document, password: &[u8]) -> Option<Vec<u8>> {
    if doc.authenticate_raw_user_password(password).is_ok() {
        return Some(password.to_vec());
    }
    doc.authenticate_raw_owner_password(password).ok()?;
    let user = recover_open_password(doc, password)?;
    doc.authenticate_raw_user_password(&user).is_ok().then_some(user)
}

/// Algorithm 7 of ISO 32000-2: the open password, as stored in `/O`, decrypted with a key derived
/// from the owner password. It comes back padded to 32 bytes, which is how the file key is
/// derived from it anyway.
fn recover_open_password(doc: &Document, owner: &[u8]) -> Option<Vec<u8>> {
    let dict = doc.get_encrypted().ok()?;
    let revision = dict.get(b"R").ok()?.as_i64().ok()?;
    let version = dict.get(b"V").ok()?.as_i64().ok()?;
    let length = match dict.get(b"Length").ok().map(Object::as_i64) {
        Some(length) => length.ok()?,
        None if version == 4 => 128,
        None => 40,
    };
    let n = if revision >= 3 { usize::try_from(length / 8).ok()? } else { 5 };
    if !(5..=16).contains(&n) {
        return None;
    }
    let stored = dict.get(b"O").ok()?.as_str().ok()?;
    let used = owner.len().min(32);
    let mut hash: [u8; 16] = Md5::new().chain_update(&owner[..used]).chain_update(&PADDING[..32 - used]).finalize().into();
    if revision >= 3 {
        for _ in 0..50 {
            hash = Md5::digest(hash).into();
        }
    }
    let key = &hash[..n];
    let mut user = stored.get(..32)?.to_vec();
    if revision == 2 {
        user = rc4(key, &user);
    } else {
        for round in (0..=19u8).rev() {
            let round_key: Vec<u8> = key.iter().map(|b| b ^ round).collect();
            user = rc4(&round_key, &user);
        }
    }
    Some(user)
}

const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08, 0x2e, 0x2e, 0x00,
    0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut state: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|byte| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(state[i as usize]);
            state.swap(i as usize, j as usize);
            byte ^ state[state[i as usize].wrapping_add(state[j as usize]) as usize]
        })
        .collect()
}

/// `password` in PDFDocEncoding, which the older revisions derive their keys from. Characters the
/// encoding does not have are left out, as other PDF software does. `None` when that is the same
/// as the UTF-8 bytes.
fn pdf_doc_encoding(password: &str) -> Option<Vec<u8>> {
    if password.is_ascii() {
        return None;
    }
    const SPECIAL: [(u16, u8); 40] = [
        (0x02d8, 0x18), (0x02c7, 0x19), (0x02c6, 0x1a), (0x02d9, 0x1b), (0x02dd, 0x1c), (0x02db, 0x1d),
        (0x02da, 0x1e), (0x02dc, 0x1f), (0x2022, 0x80), (0x2020, 0x81), (0x2021, 0x82), (0x2026, 0x83),
        (0x2014, 0x84), (0x2013, 0x85), (0x0192, 0x86), (0x2044, 0x87), (0x2039, 0x88), (0x203a, 0x89),
        (0x2212, 0x8a), (0x2030, 0x8b), (0x201e, 0x8c), (0x201c, 0x8d), (0x201d, 0x8e), (0x2018, 0x8f),
        (0x2019, 0x90), (0x201a, 0x91), (0x2122, 0x92), (0xfb01, 0x93), (0xfb02, 0x94), (0x0141, 0x95),
        (0x0152, 0x96), (0x0160, 0x97), (0x0178, 0x98), (0x017d, 0x99), (0x0131, 0x9a), (0x0142, 0x9b),
        (0x0153, 0x9c), (0x0161, 0x9d), (0x017e, 0x9e), (0x20ac, 0xa0),
    ];
    let encoded = password
        .encode_utf16()
        .filter_map(|unit| match unit {
            0x20..=0x7e | 0xa1..=0xac | 0xae..=0xff => Some(unit as u8),
            _ => SPECIAL.iter().find(|(code, _)| *code == unit).map(|(_, byte)| *byte),
        })
        .collect();
    Some(encoded)
}

/// Decrypts every object of the still-encrypted `doc` with the key `password` opens it with, the
/// way the loader would have: an object that does not decrypt is left as it is.
fn decrypt(doc: &mut Document, password: &Candidate) -> Result<()> {
    let encrypt_id = doc.trailer.get(b"Encrypt").and_then(Object::as_reference)?;
    // lopdf derives the encryption state, and records where the dictionary lives so that an
    // update can point back at it, while decrypting a document. One that holds nothing but the
    // dictionary gives the state alone.
    let mut shell = Document::new();
    shell.trailer = doc.trailer.clone();
    shell.objects.insert(encrypt_id, doc.get_object(encrypt_id)?.clone());
    match password {
        Candidate::Raw(bytes) => shell.decrypt_raw(bytes)?,
        Candidate::Text(text) => shell.decrypt(text)?,
    }
    let state = shell.encryption_state.take().ok_or_else(|| Error::Unsupported("the file could not be decrypted".into()))?;

    for (&id, object) in doc.objects.iter_mut() {
        if id != encrypt_id {
            let _ = decrypt_object(&state, id, object);
        }
    }
    unpack_object_streams(doc);
    doc.trailer.remove(b"Encrypt");
    doc.objects.remove(&encrypt_id);
    doc.encryption_state = Some(state);
    Ok(())
}

/// Adds the objects stored in object streams, each from the stream the cross-reference table names
/// for it.
fn unpack_object_streams(doc: &mut Document) {
    let mut unpacked: HashMap<u32, BTreeMap<ObjectId, Object>> = HashMap::new();
    let packed: Vec<(u32, u32)> = doc
        .reference_table
        .entries
        .iter()
        .filter_map(|(&number, entry)| match entry {
            XrefEntry::Compressed { container, .. } => Some((number, *container)),
            _ => None,
        })
        .collect();
    for (number, container) in packed {
        let members = unpacked.entry(container).or_insert_with(|| {
            doc.objects
                .get(&(container, 0))
                .and_then(|object| object.as_stream().ok())
                .and_then(|stream| ObjectStream::new(stream).ok())
                .map(|stream| stream.objects)
                .unwrap_or_default()
        });
        if let Some(object) = members.get(&(number, 0)) {
            doc.objects.insert((number, 0), object.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::Pdf;

    fn write_object(out: &mut Vec<u8>, offsets: &mut [usize], number: usize, dict: &str, stream: Option<&[u8]>) {
        offsets[number] = out.len();
        out.extend_from_slice(format!("{number} 0 obj\n{dict}\n").as_bytes());
        if let Some(data) = stream {
            out.extend_from_slice(b"stream\n");
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream\n");
        }
        out.extend_from_slice(b"endobj\n");
    }

    /// An RC4 file (revision 2, empty open password) written the way other software writes them:
    /// the catalog and page tree packed into an object stream, and a cross-reference stream that
    /// names the encryption dictionary.
    fn packed_rc4_file(text: &str) -> Vec<u8> {
        let id = b"0123456789abcdef";
        let permissions: i32 = -4;
        let pad = |password: &[u8]| {
            let mut padded = password[..password.len().min(32)].to_vec();
            padded.extend_from_slice(&PADDING[..32 - padded.len()]);
            padded
        };
        let owner_key: [u8; 16] = Md5::digest(pad(b"owner")).into();
        let o = rc4(&owner_key[..5], &pad(b""));
        let file_key: [u8; 16] =
            Md5::new().chain_update(pad(b"")).chain_update(&o).chain_update(permissions.to_le_bytes()).chain_update(id).finalize().into();
        let key = &file_key[..5];
        let u = rc4(key, &PADDING);
        let encrypt = |number: u32, data: &[u8]| {
            let object_key: [u8; 16] =
                Md5::new().chain_update(key).chain_update(&number.to_le_bytes()[..3]).chain_update([0u8, 0]).finalize().into();
            rc4(&object_key[..10], data)
        };
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();

        let catalog = "<< /Type /Catalog /Pages 3 0 R >>";
        let pages = "<< /Type /Pages /Kids [4 0 R] /Count 1 >>";
        let header = format!("2 0 3 {} ", catalog.len() + 1);
        let packed = format!("{header}{catalog} {pages}");
        let content = format!("BT /F1 18 Tf 72 700 Td ({text}) Tj ET");

        let mut out = b"%PDF-1.5\n".to_vec();
        let mut offsets = [0usize; 9];
        let sealed = encrypt(1, packed.as_bytes());
        let dict = format!("<< /Type /ObjStm /N 2 /First {} /Length {} >>", header.len(), sealed.len());
        write_object(&mut out, &mut offsets, 1, &dict, Some(&sealed));
        let page = "<< /Type /Page /Parent 3 0 R /MediaBox [0 0 612 792] /Contents 5 0 R /Resources << /Font << /F1 6 0 R >> >> >>";
        write_object(&mut out, &mut offsets, 4, page, None);
        let sealed = encrypt(5, content.as_bytes());
        write_object(&mut out, &mut offsets, 5, &format!("<< /Length {} >>", sealed.len()), Some(&sealed));
        write_object(&mut out, &mut offsets, 6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>", None);
        let dict = format!("<< /Filter /Standard /V 1 /R 2 /O <{}> /U <{}> /P {permissions} >>", hex(&o), hex(&u));
        write_object(&mut out, &mut offsets, 7, &dict, None);
        offsets[8] = out.len();
        let mut entries = Vec::new();
        for number in 0..9 {
            let (kind, field, index): (u8, u32, u16) = match number {
                0 => (0, 0, 0xffff),
                2 => (2, 1, 0),
                3 => (2, 1, 1),
                n => (1, offsets[n] as u32, 0),
            };
            entries.push(kind);
            entries.extend_from_slice(&field.to_be_bytes());
            entries.extend_from_slice(&index.to_be_bytes());
        }
        let dict = format!(
            "<< /Type /XRef /Size 9 /W [1 4 2] /Root 2 0 R /Encrypt 7 0 R /ID [<{0}> <{0}>] /Length {1} >>",
            hex(id),
            entries.len()
        );
        write_object(&mut out, &mut offsets, 8, &dict, Some(&entries));
        out.extend_from_slice(format!("startxref\n{}\n%%EOF\n", offsets[8]).as_bytes());
        out
    }

    #[test]
    fn object_streams_of_encrypted_files_are_unpacked_after_decrypting() {
        let file = packed_rc4_file("PACKED SECRET");
        // The fixture is a valid file: lopdf reads it on its own (its key is right here).
        let reference = Pdf { doc: Document::load_mem(&file).unwrap() };
        assert_eq!(reference.page_count(), 1);

        let pdf = Pdf { doc: load(&file, None).unwrap() };
        assert!(pdf.doc.encryption_state.is_some());
        assert_eq!(pdf.page_count(), 1);
        let content = pdf.doc.get_page_content(pdf.page_id(0).unwrap());
        assert!(content.windows(13).any(|w| w == b"PACKED SECRET"), "{}", String::from_utf8_lossy(&content));
        assert!(!pdf.doc.trailer.has(b"Encrypt"));
    }

    #[test]
    fn names_that_only_start_like_the_key_are_left_alone() {
        let bytes = b"<< /EncryptMetadata false /Encrypt 7 0 R /Encrypt<< >> /Encrypted 1 >>";
        let renamed = rename_key(bytes, ENCRYPT, HIDDEN_ENCRYPT).unwrap();
        assert_eq!(renamed.as_slice(), &b"<< /EncryptMetadata false /Cb-Encr 7 0 R /Cb-Encr<< >> /Encrypted 1 >>"[..]);
        assert!(rename_key(b"<< /Root 1 0 R >>", ENCRYPT, HIDDEN_ENCRYPT).is_none());
    }
}
