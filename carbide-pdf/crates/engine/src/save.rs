//! Saving a document back over the file it came from.
//!
//! A normal save writes the whole document again, pruned, so nothing that was removed or replaced
//! survives anywhere in the file. Appending an update instead is the exception: it is only used to
//! keep existing signatures or saved revisions valid, and only when no edit since the last save took
//! content out of the document. An encrypted file is written back with the encryption it was opened
//! with, so its open password, its owner password and its permissions all keep working.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use lopdf::{Document, EncryptionState, LoadOptions, Object, ObjectId, SaveOptions};

use crate::error::{Error, Result};
use crate::pdf::Pdf;
use crate::revisions::CATALOG_KEY;

/// How saving writes the document over its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePlan {
    /// Nothing differs from the file; its bytes are kept exactly as they are.
    Unchanged,
    /// The changes are appended to the file, so its signatures and saved revisions stay valid.
    Incremental,
    /// The whole document is written again and nothing removed or replaced survives. The flags say
    /// what that costs compared with appending.
    FullRewrite { breaks_signatures: bool, drops_revisions: bool },
}

impl SavePlan {
    /// True when the user has to agree first, because the rewrite invalidates signatures or drops
    /// saved revisions.
    pub fn needs_confirmation(&self) -> bool {
        matches!(self, SavePlan::FullRewrite { breaks_signatures, drops_revisions } if *breaks_signatures || *drops_revisions)
    }
}

/// The file a document was opened from or last saved to: what the next save is written over.
#[derive(Clone)]
pub struct SavedFile {
    bytes: Vec<u8>,
    /// Exactly as typed; it is needed again to read the file back.
    password: Option<String>,
    protection: Option<Protection>,
    /// Fingerprints of the text and data the file holds, per object.
    content: HashMap<ObjectId, Fingerprint>,
}

/// How the file was encrypted.
#[derive(Clone)]
struct Protection {
    state: EncryptionState,
    /// The trailer's file identifier; older encryption revisions derive the file key from it.
    id: Option<Object>,
}

impl SavedFile {
    /// Records `bytes`, the file `pdf` was just loaded from, opened with `password` if it needed one.
    pub fn new(bytes: Vec<u8>, pdf: &Pdf, password: Option<String>) -> SavedFile {
        let protection = pdf.doc.encryption_state.clone().map(|state| Protection {
            state,
            id: pdf.doc.trailer.get(b"ID").ok().cloned(),
        });
        let content = reachable(&pdf.doc)
            .into_iter()
            .filter_map(|id| Some((id, Fingerprint::of(pdf.doc.objects.get(&id)?)?)))
            .collect();
        SavedFile { bytes, password, protection, content }
    }

    /// Opens `bytes` as a document and records them as its file.
    pub fn open(bytes: Vec<u8>, password: Option<String>) -> Result<(Pdf, SavedFile)> {
        let pdf = match &password {
            Some(password) => Pdf::from_bytes_with_password(&bytes, password)?,
            None => Pdf::from_bytes(&bytes)?,
        };
        let saved = SavedFile::new(bytes, &pdf, password);
        Ok((pdf, saved))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// True for an encrypted file, including one that opens without a password.
    pub fn is_protected(&self) -> bool {
        self.protection.is_some()
    }

    /// True when `pdf` has lost text or data this file holds: an object that is no longer part of
    /// the document, or one that no longer carries all of the strings or stream data it had. Objects
    /// that hold neither (page-tree nodes, resource dictionaries) are not content and do not count.
    pub fn removed_from(&self, pdf: &Pdf) -> bool {
        let reachable = reachable(&pdf.doc);
        self.content.iter().any(|(id, held)| match pdf.doc.objects.get(id) {
            Some(object) if reachable.contains(id) => !held.kept_by(object),
            _ => true,
        })
    }

    /// The file as a document again, decrypted with the same password.
    fn load(&self) -> Result<Document> {
        let doc = match &self.password {
            Some(password) => Document::load_mem_with_options(&self.bytes, LoadOptions::with_password(password))
                .map_err(|e| match e {
                    lopdf::Error::InvalidPassword | lopdf::Error::Decryption(_) => Error::WrongPassword,
                    other => other.into(),
                })?,
            None => Document::load_mem(&self.bytes)?,
        };
        if doc.is_encrypted() {
            return Err(Error::PasswordRequired);
        }
        Ok(doc)
    }
}

impl Pdf {
    /// Decides how saving this document over `saved` writes it. `content_removed` is the caller's
    /// record that an edit since the last save took content out (applying redactions, deleting
    /// pages); edits that dropped content of the file are also found by comparing with it.
    ///
    /// Without a file the plan is always a full rewrite.
    pub fn save_plan(&self, saved: Option<&SavedFile>, content_removed: bool) -> Result<SavePlan> {
        let Some(saved) = saved else {
            return Ok(SavePlan::FullRewrite { breaks_signatures: false, drops_revisions: false });
        };
        let base = Pdf { doc: saved.load()? };
        if self.changes_since(&base.doc).is_empty() {
            return Ok(SavePlan::Unchanged);
        }
        let signed = base.has_signatures();
        let revisions = !base.revisions().is_empty();
        let removed = content_removed || saved.removed_from(self);
        // A file whose cross-reference table had to be rebuilt has nothing for an update to point
        // back to, so appending would leave the earlier objects unreachable.
        let appendable = base.doc.xref_start != 0;
        if (signed || revisions) && !removed && appendable {
            Ok(SavePlan::Incremental)
        } else {
            Ok(SavePlan::FullRewrite { breaks_signatures: signed, drops_revisions: revisions })
        }
    }

    /// The bytes that replace `saved` on disk, written the way `plan` says.
    ///
    /// A protected file comes out encrypted with the same file key and the same encryption
    /// dictionary values, so every password and permission of the original still applies. When
    /// that cannot be done the save fails instead of producing plaintext.
    pub fn save_bytes(&mut self, plan: SavePlan, saved: Option<&SavedFile>) -> Result<Vec<u8>> {
        if saved.is_some_and(|s| s.password.is_some() && s.protection.is_none()) {
            return Err(Error::Other(NOT_ENCRYPTED.into()));
        }
        let protection = saved.and_then(|s| s.protection.as_ref());
        let bytes = match (plan, saved) {
            (SavePlan::Unchanged, Some(saved)) => return Ok(saved.bytes.clone()),
            (SavePlan::Incremental, Some(saved)) => self.append_update(&saved.bytes, saved.load()?)?,
            (SavePlan::FullRewrite { .. }, _) => self.rewrite(protection)?,
            (_, None) => return Err(Error::Other("there is no file to update".into())),
        };
        if protection.is_some() && !is_encrypted_file(&bytes) {
            return Err(Error::Other(NOT_ENCRYPTED.into()));
        }
        Ok(bytes)
    }

    /// The whole document, pruned, encrypted again when `protection` is given.
    fn rewrite(&mut self, protection: Option<&Protection>) -> Result<Vec<u8>> {
        self.doc.prune_objects();
        let mut copy = self.doc.clone();
        // Saved revisions are byte offsets into the old file and mean nothing in the new one.
        if let Ok(catalog) = copy.catalog_mut() {
            if catalog.remove(CATALOG_KEY).is_some() {
                copy.prune_objects();
            }
        }
        let mut out = Vec::new();
        match protection {
            None => copy.save_to(&mut out)?,
            Some(protection) => {
                copy.trailer.remove(b"Encrypt");
                match &protection.id {
                    Some(id) => copy.trailer.set("ID", id.clone()),
                    None => {
                        copy.trailer.remove(b"ID");
                    }
                }
                copy.encrypt(&protection.state).map_err(|e| Error::Other(format!("{NOT_ENCRYPTED} ({e})")))?;
                // As for new protected files: no object streams, which lopdf cannot encrypt.
                let options = SaveOptions::builder().use_object_streams(false).use_xref_streams(false).build();
                copy.save_with_options(&mut out, options)?;
            }
        }
        Ok(out)
    }
}

const NOT_ENCRYPTED: &str = "the document could not be encrypted again, so it was not saved";

/// True when `bytes` is a PDF that carries an encryption dictionary.
fn is_encrypted_file(bytes: &[u8]) -> bool {
    Document::load_mem(bytes).is_ok_and(|doc| doc.is_encrypted() || doc.was_encrypted())
}

/// Every object reachable from the trailer.
fn reachable(doc: &Document) -> HashSet<ObjectId> {
    fn references(object: &Object, out: &mut Vec<ObjectId>) {
        match object {
            Object::Reference(id) => out.push(*id),
            Object::Array(items) => items.iter().for_each(|item| references(item, out)),
            Object::Dictionary(dict) => dict.iter().for_each(|(_, value)| references(value, out)),
            Object::Stream(stream) => stream.dict.iter().for_each(|(_, value)| references(value, out)),
            _ => {}
        }
    }
    let mut pending = Vec::new();
    doc.trailer.iter().for_each(|(_, value)| references(value, &mut pending));
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if seen.insert(id) {
            if let Some(object) = doc.objects.get(&id) {
                references(object, &mut pending);
            }
        }
    }
    seen
}

/// The text and data of one object: hashes of its strings and of its stream content.
#[derive(Clone)]
struct Fingerprint {
    strings: Vec<u64>,
    stream: Option<u64>,
}

impl Fingerprint {
    /// `None` for an object that holds neither strings nor stream data.
    fn of(object: &Object) -> Option<Fingerprint> {
        fn strings(object: &Object, out: &mut Vec<u64>) {
            match object {
                Object::String(bytes, _) => out.push(hash(bytes)),
                Object::Array(items) => items.iter().for_each(|item| strings(item, out)),
                Object::Dictionary(dict) => dict.iter().for_each(|(_, value)| strings(value, out)),
                Object::Stream(stream) => stream.dict.iter().for_each(|(_, value)| strings(value, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        strings(object, &mut found);
        found.sort_unstable();
        let stream = object.as_stream().ok().map(|s| hash(&s.content));
        (stream.is_some() || !found.is_empty()).then_some(Fingerprint { strings: found, stream })
    }

    /// True when `object` still carries all of this content; added strings do not matter.
    fn kept_by(&self, object: &Object) -> bool {
        let Some(now) = Fingerprint::of(object) else { return false };
        self.stream == now.stream && self.strings.iter().all(|s| now.strings.binary_search(s).is_ok())
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
