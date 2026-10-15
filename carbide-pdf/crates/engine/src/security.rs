//! Password protection (AES-256) and permissions.

use std::collections::BTreeMap;
use std::sync::Arc;

use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
use lopdf::{EncryptionState, EncryptionVersion, Permissions};

use crate::error::{Error, Result};
use crate::pdf::Pdf;

#[derive(Debug, Clone)]
pub struct ProtectionOptions {
    /// Password needed to open the file. Empty means anyone can open it (permissions still apply).
    pub user_password: String,
    /// Password that lifts the permission restrictions. Required.
    pub owner_password: String,
    pub allow_printing: bool,
    pub allow_copying: bool,
    pub allow_modifying: bool,
    pub allow_annotating: bool,
}

impl ProtectionOptions {
    pub fn new(user_password: impl Into<String>, owner_password: impl Into<String>) -> Self {
        Self {
            user_password: user_password.into(),
            owner_password: owner_password.into(),
            allow_printing: true,
            allow_copying: true,
            allow_modifying: false,
            allow_annotating: true,
        }
    }

    fn permissions(&self) -> Permissions {
        let mut p = Permissions::ASSEMBLABLE | Permissions::COPYABLE_FOR_ACCESSIBILITY;
        if self.allow_printing {
            p |= Permissions::PRINTABLE | Permissions::PRINTABLE_IN_HIGH_QUALITY;
        }
        if self.allow_copying {
            p |= Permissions::COPYABLE;
        }
        if self.allow_modifying {
            p |= Permissions::MODIFIABLE;
        }
        if self.allow_annotating {
            p |= Permissions::ANNOTABLE | Permissions::FILLABLE;
        }
        p
    }
}

impl Pdf {
    /// Serializes an encrypted copy. The in-memory document stays unencrypted.
    pub fn to_encrypted_bytes(&mut self, options: &ProtectionOptions) -> Result<Vec<u8>> {
        if options.owner_password.is_empty() {
            return Err(Error::Other("an owner password is required".into()));
        }
        crate::save::prune(&mut self.doc);
        let mut copy = self.doc.clone();

        let mut key = [0u8; 32];
        getrandom::fill(&mut key)
            .map_err(|e| Error::Other(format!("no randomness available: {e}")))?;

        let mut filters: BTreeMap<Vec<u8>, Arc<dyn CryptFilter>> = BTreeMap::new();
        filters.insert(b"StdCF".to_vec(), Arc::new(Aes256CryptFilter));

        let version = EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: filters,
            file_encryption_key: &key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: &options.owner_password,
            user_password: &options.user_password,
            permissions: options.permissions(),
        };
        let state = EncryptionState::try_from(version)
            .map_err(|e| Error::Other(format!("could not set up encryption: {e}")))?;
        copy.encrypt(&state)
            .map_err(|e| Error::Other(format!("could not encrypt: {e}")))?;
        // Classic cross-reference table and no object streams: every consumer can read this, and
        // lopdf itself cannot expand object streams of an encrypted file.
        let options = lopdf::SaveOptions::builder()
            .use_object_streams(false)
            .use_xref_streams(false)
            .build();
        let mut out = Vec::new();
        copy.save_with_options(&mut out, options)?;
        Ok(out)
    }

    pub fn save_encrypted(
        &mut self,
        path: impl AsRef<std::path::Path>,
        options: &ProtectionOptions,
    ) -> Result<()> {
        let bytes = self.to_encrypted_bytes(options)?;
        crate::fs::write_replacing(path.as_ref(), &bytes)?;
        Ok(())
    }
}
