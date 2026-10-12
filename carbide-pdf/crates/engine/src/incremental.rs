//! Incremental updates: append only what changed to an existing file.
//!
//! The bytes of the original file are never touched, which keeps earlier revisions recoverable and
//! keeps digital signatures over earlier byte ranges valid.

use lopdf::{Document, IncrementalDocument, Object, ObjectId};

use crate::error::{Error, Result};
use crate::pdf::Pdf;

/// Trailer entries an update carries over from the document.
const TRAILER_KEYS: [&[u8]; 2] = [b"Root", b"Info"];

/// What differs between a document and the file it was loaded from.
pub(crate) struct Changes {
    /// Objects that are new or differ from the file's version.
    pub objects: Vec<(ObjectId, Object)>,
    /// Trailer entries whose value changed; `None` when the document no longer has the entry.
    pub trailer: Vec<(&'static [u8], Option<Object>)>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty() && self.trailer.is_empty()
    }
}

impl Pdf {
    /// Returns `base_bytes` followed by an update section holding every object of this document
    /// that is new or differs from the version in `base_bytes`.
    ///
    /// When nothing differs, the original bytes are returned unchanged. Appending an empty update
    /// would move the end of the file and make a signature look as if the document had been edited.
    ///
    /// `base_bytes` must be the file this document was loaded from (or a later save of it).
    /// Encrypted files are not supported, including those that open without a password.
    pub fn incremental_update(&self, base_bytes: &[u8]) -> Result<Vec<u8>> {
        let base = Document::load_mem(base_bytes)?;
        if base.is_encrypted() || base.was_encrypted() || self.doc.is_encrypted() {
            return Err(Error::Unsupported("incremental updates of encrypted files are not supported".into()));
        }
        self.append_update(base_bytes, base)
    }

    /// The update itself. `base` is `base_bytes` as loaded; for an encrypted file it has to be the
    /// decrypted document, and the appended objects are then encrypted with the file's own key.
    pub(crate) fn append_update(&self, base_bytes: &[u8], base: Document) -> Result<Vec<u8>> {
        let changes = self.changes_since(&base);
        if changes.is_empty() {
            return Ok(base_bytes.to_vec());
        }

        let max_id = self.doc.max_id.max(base.max_id);
        let mut update = IncrementalDocument::create_from(base_bytes.to_vec(), base);
        for (id, object) in changes.objects {
            update.new_document.set_object(id, object);
        }
        for (key, value) in changes.trailer {
            match value {
                Some(value) => update.new_document.trailer.set(key.to_vec(), value),
                None => {
                    update.new_document.trailer.remove(key);
                }
            }
        }
        update.new_document.max_id = max_id;

        let mut out = Vec::with_capacity(base_bytes.len() + 4096);
        update.save_to(&mut out)?;
        Ok(out)
    }

    pub(crate) fn changes_since(&self, base: &Document) -> Changes {
        let objects = self
            .doc
            .objects
            .iter()
            .filter(|(id, object)| !is_file_structure(object) && base.objects.get(id) != Some(*object))
            .map(|(id, object)| (*id, object.clone()))
            .collect();
        let trailer = TRAILER_KEYS
            .into_iter()
            .filter_map(|key| {
                let now = self.doc.trailer.get(key).ok();
                (now != base.trailer.get(key).ok()).then(|| (key, now.cloned()))
            })
            .collect();
        Changes { objects, trailer }
    }
}

/// Cross-reference and object streams belong to how a file was written, not to the document: every
/// save builds its own and an update never carries them.
fn is_file_structure(object: &Object) -> bool {
    object.type_name().is_ok_and(|name| name == b"XRef" || name == b"ObjStm")
}
