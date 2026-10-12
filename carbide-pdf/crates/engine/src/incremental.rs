//! Incremental updates: append only what changed to an existing file.
//!
//! The bytes of the original file are never touched, which keeps earlier revisions recoverable and
//! keeps digital signatures over earlier byte ranges valid.

use lopdf::{Document, IncrementalDocument, Object, ObjectId};

use crate::error::{Error, Result};
use crate::pdf::Pdf;

impl Pdf {
    /// Returns `base_bytes` followed by an update section holding every object of this document
    /// that is new or differs from the version in `base_bytes`.
    ///
    /// When nothing differs, the original bytes are returned unchanged. Appending an empty update
    /// would move the end of the file and make a signature look as if the document had been edited.
    ///
    /// `base_bytes` must be the file this document was loaded from (or a later save of it).
    /// Encrypted files are not supported.
    pub fn incremental_update(&self, base_bytes: &[u8]) -> Result<Vec<u8>> {
        let base = Document::load_mem(base_bytes)?;
        if base.is_encrypted() || self.doc.is_encrypted() {
            return Err(Error::Unsupported("incremental updates of encrypted files are not supported".into()));
        }

        let mut changed: Vec<(ObjectId, Object)> = Vec::new();
        for (id, object) in &self.doc.objects {
            if base.objects.get(id) != Some(object) {
                changed.push((*id, object.clone()));
            }
        }
        let mut trailer_updates: Vec<(Vec<u8>, Object)> = Vec::new();
        for key in [&b"Root"[..], b"Info"] {
            if let Ok(value) = self.doc.trailer.get(key) {
                if base.trailer.get(key).ok() != Some(value) {
                    trailer_updates.push((key.to_vec(), value.clone()));
                }
            }
        }
        if changed.is_empty() && trailer_updates.is_empty() {
            return Ok(base_bytes.to_vec());
        }

        let mut update = IncrementalDocument::create_from(base_bytes.to_vec(), base.clone());
        for (id, object) in changed {
            update.new_document.set_object(id, object);
        }
        for (key, value) in trailer_updates {
            update.new_document.trailer.set(key, value);
        }
        update.new_document.max_id = self.doc.max_id.max(base.max_id);

        let mut out = Vec::with_capacity(base_bytes.len() + 4096);
        update.save_to(&mut out)?;
        Ok(out)
    }
}
