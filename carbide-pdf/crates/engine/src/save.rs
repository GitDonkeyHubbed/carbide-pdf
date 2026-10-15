//! Saving a document back over the file it came from.
//!
//! A normal save writes the whole document again, pruned, so nothing that was removed or replaced
//! survives anywhere in the file. Appending an update instead is the exception: it is only used to
//! keep existing signatures or saved revisions valid, and only when no edit since the last save took
//! content out of the document. An encrypted file is written back with the encryption it was opened
//! with, so its open password, its owner password and its permissions all keep working. So is a new
//! document made from it (extracted pages, split parts, imposed sheets, a merge): it carries the
//! protection along, and writing it without needs `set_protection(None)` first.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use lopdf::{Document, EncryptionState, Object, ObjectId, SaveOptions};

use crate::error::{Error, Result};
use crate::incremental::file_structure;
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
    FullRewrite {
        breaks_signatures: bool,
        drops_revisions: bool,
    },
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
    /// Objects the file holds that are not part of the document: nothing in it refers to them.
    unused: HashSet<ObjectId>,
    /// How many updates were appended to the file. Each keeps the earlier version of what it
    /// changed in the file.
    updates: usize,
}

/// How a protected file is encrypted: the values of its encryption dictionary and the file key its
/// passwords give. A document written with it opens with the same passwords and grants the same
/// permissions as the file.
#[derive(Clone)]
pub struct Protection {
    state: EncryptionState,
    /// The trailer's file identifier; older encryption revisions derive the file key from it.
    id: Option<Object>,
}

impl Protection {
    /// True when `other` is the same encryption: the same file key, encryption dictionary values
    /// and permissions, so the same passwords open both and allow the same.
    pub fn same_as(&self, other: &Protection) -> bool {
        let (a, b) = (&self.state, &other.state);
        let first_id = |p: &Protection| {
            p.id.as_ref()
                .and_then(|id| id.as_array().ok()?.first().cloned())
        };
        a.file_encryption_key() == b.file_encryption_key()
            && (a.version(), a.revision()) == (b.version(), b.revision())
            && a.permissions() == b.permissions()
            && (a.owner_value(), a.user_value()) == (b.owner_value(), b.user_value())
            && (a.owner_encrypted(), a.user_encrypted())
                == (b.owner_encrypted(), b.user_encrypted())
            && a.permission_encrypted() == b.permission_encrypted()
            && (a.revision() >= 5 || first_id(self) == first_id(other))
    }
}

/// Refusal to merge documents that are protected in different ways.
pub const DIFFERENT_PROTECTION: &str =
    "The documents are protected with different passwords or permissions, \
so one file cannot keep the protection of all of them.";

impl SavedFile {
    /// Records `bytes`, the file `pdf` was just loaded from, opened with `password` if it needed one.
    pub fn new(bytes: Vec<u8>, pdf: &Pdf, password: Option<String>) -> SavedFile {
        let protection = pdf.doc.encryption_state.clone().map(|state| Protection {
            state,
            id: pdf.doc.trailer.get(b"ID").ok().cloned(),
        });
        let reachable = reachable(&pdf.doc);
        let content = reachable
            .iter()
            .filter_map(|&id| Some((id, Fingerprint::of(pdf.doc.objects.get(&id)?)?)))
            .collect();
        let structure = file_structure(&pdf.doc);
        let unused = pdf
            .doc
            .objects
            .keys()
            .filter(|id| !reachable.contains(id) && !structure.contains(id))
            .copied()
            .collect();
        let updates = appended_updates(&bytes, pdf.doc.xref_start);
        SavedFile {
            bytes,
            password,
            protection,
            content,
            unused,
            updates,
        }
    }

    /// Opens `bytes` as a document and records them as its file. The password is used exactly as
    /// given; it may be the open password or the owner password.
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

    /// How the file is encrypted, `None` for an unprotected one.
    pub fn protection(&self) -> Option<&Protection> {
        self.protection.as_ref()
    }

    /// Objects the file holds that nothing in the document refers to. Writing the document in
    /// full leaves them out.
    pub(crate) fn unused_objects(&self) -> &HashSet<ObjectId> {
        &self.unused
    }

    /// How many updates were appended to the file, each keeping the earlier version of what it
    /// changed. Writing the document in full leaves those versions out.
    pub fn appended_updates(&self) -> usize {
        self.updates
    }

    /// True when `pdf` has lost text or data this file holds: an object that is no longer part of
    /// the document, or one that no longer carries all of the strings or stream data it had. An
    /// annotation counts even when it holds neither, since what it draws is content. Other objects
    /// that hold neither (page-tree nodes, resource dictionaries) are not content and do not count.
    pub fn removed_from(&self, pdf: &Pdf) -> bool {
        let reachable = reachable(&pdf.doc);
        self.content
            .iter()
            .any(|(id, held)| match pdf.doc.objects.get(id) {
                Some(object) if reachable.contains(id) => !held.kept_by(object),
                _ => true,
            })
    }

    /// The file as a document again, decrypted with the same password. Its page tree is left as
    /// stored, so that repairs made when the document was opened count as changes and an update
    /// appended to the file carries them.
    fn load(&self) -> Result<Document> {
        Pdf::load_unrepaired(&self.bytes, self.password.as_deref())
    }
}

impl Pdf {
    /// How the file this document was opened from is encrypted, `None` when it was not. Documents
    /// made from it (extracted pages, split parts, imposed sheets, a merge) carry it along, and
    /// [`Pdf::to_file_bytes`] writes them protected the same way.
    pub fn protection(&self) -> Option<Protection> {
        Some(Protection {
            state: self.doc.encryption_state.clone()?,
            id: self.doc.trailer.get(b"ID").ok().cloned(),
        })
    }

    /// Gives the document the protection it is written with. `None` takes it away: writing a
    /// document made from a protected one without protection is a choice made this way.
    pub fn set_protection(&mut self, protection: Option<Protection>) {
        match protection {
            Some(Protection { state, id }) => {
                self.doc.encryption_state = Some(state);
                if let Some(id) = id {
                    self.doc.trailer.set("ID", id);
                }
            }
            None => self.doc.encryption_state = None,
        }
    }

    /// True when the document carries a protection (see [`Pdf::protection`]).
    pub fn is_protected(&self) -> bool {
        self.doc.encryption_state.is_some()
    }

    /// The protection a merge of `documents` carries: the one its protected documents share.
    /// Documents protected in different ways are refused with [`DIFFERENT_PROTECTION`], since the
    /// passwords of one would then open the pages of the others; take the protection off them with
    /// `set_protection(None)` first to merge them without.
    pub fn shared_protection(documents: &[&Pdf]) -> Result<Option<Protection>> {
        let mut shared: Option<Protection> = None;
        for protection in documents.iter().filter_map(|d| d.protection()) {
            match &shared {
                None => shared = Some(protection),
                Some(first) if first.same_as(&protection) => {}
                Some(_) => return Err(Error::Unsupported(DIFFERENT_PROTECTION.into())),
            }
        }
        Ok(shared)
    }

    /// The document as a file of its own, pruned: encrypted with its protection when it carries
    /// one, so the file opens with the same passwords and grants the same permissions as the file
    /// it came from; otherwise unencrypted, as [`Pdf::to_bytes`] gives it. Saved revisions are left
    /// out, as they are positions in another file.
    ///
    /// `password` is the one the protected file was opened with, `None` when it opened without one.
    /// The result is read back with it and has to give the same file key and encryption values;
    /// otherwise this fails rather than give out a file that is unencrypted or opens differently.
    pub fn to_file_bytes(&mut self, password: Option<&str>) -> Result<Vec<u8>> {
        let protection = self.protection();
        let bytes = self.rewrite(protection.as_ref())?;
        if let Some(protection) = &protection {
            check_encryption(&bytes, password, protection)?;
        }
        Ok(bytes)
    }

    /// Decides how saving this document over `saved` writes it. `content_removed` is the caller's
    /// record that an edit since the last save took content out (applying redactions, deleting
    /// pages); edits that dropped content of the file are also found by comparing with it.
    ///
    /// Without a file the plan is always a full rewrite. So it is after an edit that removed
    /// content, even when the document itself shows no change: what was removed may be held only
    /// by the file (an object nothing refers to, an earlier version an update left behind).
    pub fn save_plan(&self, saved: Option<&SavedFile>, content_removed: bool) -> Result<SavePlan> {
        let Some(saved) = saved else {
            return Ok(SavePlan::FullRewrite {
                breaks_signatures: false,
                drops_revisions: false,
            });
        };
        let base = Pdf { doc: saved.load()? };
        if !content_removed && self.changes_since(&base.doc).is_empty() {
            return Ok(SavePlan::Unchanged);
        }
        // A signature the file already fails (after an earlier rewrite, say) is not worth keeping
        // the old bytes for.
        let signed = base.has_intact_signatures(&saved.bytes, saved.password());
        let revisions = !base.revisions().is_empty();
        let removed = content_removed || saved.removed_from(self);
        // A file whose cross-reference table had to be rebuilt has nothing for an update to point
        // back to, so appending would leave the earlier objects unreachable.
        let appendable = base.doc.xref_start != 0;
        if (signed || revisions) && !removed && appendable {
            Ok(SavePlan::Incremental)
        } else {
            Ok(SavePlan::FullRewrite {
                breaks_signatures: signed,
                drops_revisions: revisions,
            })
        }
    }

    /// The bytes that replace `saved` on disk, written the way `plan` says.
    ///
    /// A protected file comes out encrypted with the same file key and the same encryption
    /// dictionary values, so every password and permission of the original still applies. When
    /// that cannot be done the save fails instead of producing plaintext. A new document, without
    /// a file, is written as [`Pdf::to_file_bytes`] writes it: with the protection it carries.
    pub fn save_bytes(&mut self, plan: SavePlan, saved: Option<&SavedFile>) -> Result<Vec<u8>> {
        let Some(saved) = saved else {
            return match plan {
                SavePlan::FullRewrite { .. } => self.to_file_bytes(None),
                _ => Err(Error::Other("there is no file to update".into())),
            };
        };
        if saved.password.is_some() && saved.protection.is_none() {
            return Err(Error::Other(NOT_ENCRYPTED.into()));
        }
        let protection = saved.protection.as_ref();
        let bytes = match plan {
            SavePlan::Unchanged => return Ok(saved.bytes.clone()),
            SavePlan::Incremental => self.append_update(&saved.bytes, saved.load()?)?,
            SavePlan::FullRewrite { .. } => self.rewrite(protection)?,
        };
        if let Some(protection) = protection {
            check_encryption(&bytes, saved.password(), protection)?;
        }
        Ok(bytes)
    }

    /// The whole document, pruned, encrypted again when `protection` is given.
    fn rewrite(&mut self, protection: Option<&Protection>) -> Result<Vec<u8>> {
        prune(&mut self.doc);
        crate::pdf::writable_numbers(&mut self.doc);
        let mut copy = self.doc.clone();
        // Saved revisions are byte offsets into the old file and mean nothing in the new one.
        if let Ok(catalog) = copy.catalog_mut() {
            if catalog.remove(CATALOG_KEY).is_some() {
                prune(&mut copy);
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
                copy.encrypt(&protection.state)
                    .map_err(|e| Error::Other(format!("{NOT_ENCRYPTED} ({e})")))?;
                // As for new protected files: no object streams, which lopdf cannot encrypt.
                let options = SaveOptions::builder()
                    .use_object_streams(false)
                    .use_xref_streams(false)
                    .build();
                copy.save_with_options(&mut out, options)?;
            }
        }
        Ok(out)
    }
}

const NOT_ENCRYPTED: &str = "the document could not be encrypted again, so it was not saved";

/// Fails unless `bytes` open with `password`, the one the protected file was opened with, and that
/// password, checked against the new file's encryption dictionary, gives the file key and the
/// dictionary values of the original. The key is derived afresh here, so a wrong one in memory
/// cannot slip through.
fn check_encryption(bytes: &[u8], password: Option<&str>, protection: &Protection) -> Result<()> {
    let written = match password {
        Some(password) => Pdf::from_bytes_with_password(bytes, password),
        None => Pdf::from_bytes(bytes),
    };
    let state = match written {
        Ok(pdf) => pdf.doc.encryption_state,
        Err(Error::PasswordRequired) => {
            return Err(Error::Other(format!(
                "{NOT_ENCRYPTED} (the password it was opened with is needed)"
            )));
        }
        Err(_) => None,
    };
    let Some(state) = state else {
        return Err(Error::Other(NOT_ENCRYPTED.into()));
    };
    let original = &protection.state;
    let same = state.file_encryption_key() == original.file_encryption_key()
        && (state.version(), state.revision()) == (original.version(), original.revision())
        && state.permissions() == original.permissions()
        && (state.owner_value(), state.user_value())
            == (original.owner_value(), original.user_value())
        && (state.owner_encrypted(), state.user_encrypted())
            == (original.owner_encrypted(), original.user_encrypted());
    if same {
        Ok(())
    } else {
        Err(Error::Other(NOT_ENCRYPTED.into()))
    }
}

/// Every object reachable from the trailer.
pub(crate) fn reachable(doc: &Document) -> HashSet<ObjectId> {
    fn references(object: &Object, out: &mut Vec<ObjectId>) {
        match object {
            Object::Reference(id) => out.push(*id),
            Object::Array(items) => items.iter().for_each(|item| references(item, out)),
            Object::Dictionary(dict) => dict.iter().for_each(|(_, value)| references(value, out)),
            Object::Stream(stream) => stream
                .dict
                .iter()
                .for_each(|(_, value)| references(value, out)),
            _ => {}
        }
    }
    let mut pending = Vec::new();
    doc.trailer
        .iter()
        .for_each(|(_, value)| references(value, &mut pending));
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

/// Removes every object that is not reachable from the trailer, and returns their ids in order.
/// lopdf's own `prune_objects` keeps its list of reachable objects in a vector, which makes it take
/// time in proportion to the square of the number of objects.
pub(crate) fn prune(doc: &mut Document) -> Vec<ObjectId> {
    let keep = reachable(doc);
    let gone: Vec<ObjectId> = doc
        .objects
        .keys()
        .filter(|id| !keep.contains(id))
        .copied()
        .collect();
    for id in &gone {
        doc.objects.remove(id);
    }
    gone
}

/// How many updates were appended to the file `bytes`, whose last cross-reference section starts
/// at `xref_start`. Counted by following the sections' /Prev links back to the first; the link from
/// a linearized file's first-page section to its main section is not an update. A start of zero
/// means the cross-reference table had to be rebuilt, which leaves nothing to follow.
fn appended_updates(bytes: &[u8], xref_start: usize) -> usize {
    let mut seen = HashSet::new();
    let mut at = xref_start;
    let mut links = 0;
    while at != 0 && seen.insert(at) {
        match previous_section(bytes, at) {
            Some(previous) if previous < bytes.len() => {
                links += 1;
                at = previous;
            }
            _ => break,
        }
    }
    let linearized = find(&bytes[..bytes.len().min(1024)], b"/Linearized").is_some();
    links - usize::from(linearized && links > 0)
}

/// The /Prev entry of the cross-reference section at `at`: in the trailer that follows a table, or
/// in the dictionary of a cross-reference stream.
fn previous_section(bytes: &[u8], at: usize) -> Option<usize> {
    let section = bytes.get(at..)?;
    let end: &[u8] = if section.trim_ascii_start().starts_with(b"xref") {
        b"startxref"
    } else {
        b"stream"
    };
    let dictionary = &section[..find(section, end)?];
    let after = &dictionary[find(dictionary, b"/Prev")? + b"/Prev".len()..];
    let digits: Vec<u8> = after
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take_while(|b| b.is_ascii_digit())
        .copied()
        .collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The text and data of one object: hashes of its strings and of its stream content.
#[derive(Clone)]
struct Fingerprint {
    strings: Vec<u64>,
    stream: Option<u64>,
}

impl Fingerprint {
    /// `None` for an object that holds neither strings nor stream data and is not an annotation.
    fn of(object: &Object) -> Option<Fingerprint> {
        fn strings(object: &Object, out: &mut Vec<u64>) {
            match object {
                Object::String(bytes, _) => out.push(hash(bytes)),
                Object::Array(items) => items.iter().for_each(|item| strings(item, out)),
                Object::Dictionary(dict) => dict.iter().for_each(|(_, value)| strings(value, out)),
                Object::Stream(stream) => stream
                    .dict
                    .iter()
                    .for_each(|(_, value)| strings(value, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        strings(object, &mut found);
        found.sort_unstable();
        let stream = object.as_stream().ok().map(|s| hash(&s.content));
        (stream.is_some() || !found.is_empty() || is_annotation(object)).then_some(Fingerprint {
            strings: found,
            stream,
        })
    }

    /// True when `object` still carries all of this content; added strings do not matter.
    fn kept_by(&self, object: &Object) -> bool {
        let Some(now) = Fingerprint::of(object) else {
            return false;
        };
        self.stream == now.stream
            && self
                .strings
                .iter()
                .all(|s| now.strings.binary_search(s).is_ok())
    }
}

/// An annotation dictionary. Ink, shapes and stamps can hold nothing but numbers and names.
fn is_annotation(object: &Object) -> bool {
    let Object::Dictionary(dict) = object else {
        return false;
    };
    dict.has_type(b"Annot")
        || (dict.get(b"Subtype").is_ok_and(|s| s.as_name().is_ok()) && dict.has(b"Rect"))
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
