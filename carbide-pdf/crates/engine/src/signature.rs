//! Digital signatures: signing (PKCS#7 detached, CAdES, document time stamps, certification) and
//! validation.
//!
//! The structure follows the original's pipeline: a signature field is created, its `/V`
//! dictionary is written with a fixed-size `/Contents` placeholder and a fixed-width `/ByteRange`,
//! the file is saved as an incremental update (so earlier signatures stay valid), the real
//! `/ByteRange` is patched in, and the CMS signature over the two byte ranges is written into the
//! placeholder. Digest is SHA-256 and the signature RSA PKCS#1 v1.5 with SHA-256.

use std::collections::BTreeSet;

use chrono::{DateTime, TimeZone, Utc};
use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream, StringFormat};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256, Sha384, Sha512};
use x509_cert::der::{Decode, Encode};
use x509_cert::Certificate;

use crate::der;
use crate::error::{Error, Result};
use crate::fonts::{literal_string, win_ansi};
use crate::geometry::{fmt, Rect};
use crate::pdf::Pdf;
use crate::revisions::{decode_text, encode_text, parse_pdf_date, pdf_date_now};

/// Verbatim strings from the original.
pub mod messages {
    pub const UNSIGNED_FIELD: &str = "Unsigned signature field (Click to sign)";
    pub const NOT_YET_VERIFIED: &str = "Signature not yet verified";
    pub const VALID: &str = "Signature is valid";
    pub const VALIDITY_UNKNOWN: &str = "Signature validity is unknown";
    pub const INVALID: &str = "Signature is invalid";
    pub const REVISION_UNALTERED: &str = "This revision of the document has not been altered";
    pub const SUBSEQUENT_CHANGES: &str = "There have been subsequence changes to the document";
    pub const NOT_MODIFIED: &str = "Document has not been modified since this signature was applied";
    pub const IDENTITY_UNKNOWN: &str = "Signer's identity is unknown because it was not included in your list of trusted certificates and none of its parent certificates are trusted certificates";
    pub const IDENTITY_EXPIRED: &str = "Signer's identity is invalid because it has expired or is not yet valid";
    pub const DOCUMENT_ALTERED: &str = "The document is corrupt or has been altered since the signature was applied.";
    pub const BAD_BYTE_RANGE: &str = "Signature Invalid: Invalid Byte Range!";
    pub const CORRUPTED: &str = "Signature Invalid: Signature Corrupted!";
    pub const FILTER_NOT_SUPPORTED: &str = "Signature Invalid: Filter Not Supported!";
    pub const SIGNED_IN_FUTURE: &str = "Signature seems to be signed in the future.";
    pub const NOTICE_NOT_VERIFIED: &str = "At least one signature requires validating.";
    pub const NOTICE_PROBLEMS: &str = "This document is signed. At least one signature has problems.";
    pub const NOTICE_CHANGED_AFTER_LAST: &str =
        "Signed and all signatures are valid, but there were changes in the document after last signature was applied.";
    pub const NOTICE_UPDATED: &str = "Signed and all signatures are valid, document was updated after signing.";
    pub const NOTICE_ALL_VALID: &str = "This document is signed. All signatures are valid.";
    pub const FIELD_ALREADY_SIGNED: &str = "Field is already signed.";
    pub const TIME_STAMPED: &str = "Signature is Time Stamped.";
}

// ---- identity ------------------------------------------------------------------------------

/// A signing identity: a certificate chain (signer first) and its RSA private key.
pub struct Identity {
    chain: Vec<Vec<u8>>,
    key: RsaPrivateKey,
}

impl Identity {
    pub fn new(chain_der: Vec<Vec<u8>>, key: RsaPrivateKey) -> Result<Identity> {
        if chain_der.is_empty() {
            return Err(Error::Other("the identity has no certificate".into()));
        }
        Certificate::from_der(&chain_der[0]).map_err(|e| Error::Other(format!("invalid certificate: {e}")))?;
        Ok(Identity { chain: chain_der, key })
    }

    /// PEM certificate(s) (signer first) and a PKCS#8 or PKCS#1 RSA private key.
    pub fn from_pem(certificates: &str, private_key: &str) -> Result<Identity> {
        let chain = pem_blocks(certificates, "CERTIFICATE");
        let key = if let Some(der) = pem_blocks(private_key, "PRIVATE KEY").into_iter().next() {
            RsaPrivateKey::from_pkcs8_der(&der).map_err(|e| Error::Other(format!("invalid private key: {e}")))?
        } else if let Some(der) = pem_blocks(private_key, "RSA PRIVATE KEY").into_iter().next() {
            RsaPrivateKey::from_pkcs1_der(&der).map_err(|e| Error::Other(format!("invalid private key: {e}")))?
        } else {
            return Err(Error::Other("no RSA private key found".into()));
        };
        Identity::new(chain, key)
    }

    /// A PKCS#12 (.p12 / .pfx) file protected by `password`.
    pub fn from_pkcs12(bytes: &[u8], password: &str) -> Result<Identity> {
        let store = p12_keystore::KeyStore::from_pkcs12(bytes, password, p12_keystore::Pkcs12ImportPolicy::Strict)
            .map_err(|e| Error::Other(format!("could not read the PKCS#12 file: {e}")))?;
        for (_, entry) in store.entries() {
            if let p12_keystore::KeyStoreEntry::PrivateKeyChain(chain) = entry {
                let key = RsaPrivateKey::from_pkcs8_der(chain.key().as_der())
                    .map_err(|e| Error::Other(format!("only RSA identities are supported: {e}")))?;
                let certificates = chain.certs().iter().map(|c| c.as_der().to_vec()).collect();
                return Identity::new(certificates, key);
            }
        }
        Err(Error::Other("the PKCS#12 file contains no private key".into()))
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.chain[0]
    }

    /// The signer's common name, used for the signature's `/Name`.
    pub fn common_name(&self) -> String {
        Certificate::from_der(&self.chain[0]).map(|c| common_name(&c)).unwrap_or_default()
    }
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(cleaned).ok()
}

fn pem_blocks(text: &str, label: &str) -> Vec<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else { break };
        if let Some(der) = base64_decode(&after[..stop]) {
            out.push(der);
        }
        rest = &after[stop + end.len()..];
    }
    out
}

fn common_name(certificate: &Certificate) -> String {
    let subject = certificate.tbs_certificate.subject.to_string();
    subject
        .split(',')
        .find_map(|part| part.trim().strip_prefix("CN=").map(|s| s.to_string()))
        .unwrap_or(subject)
}

// ---- time stamps ---------------------------------------------------------------------------

/// Supplies RFC 3161 time-stamp tokens.
pub trait TimestampSource {
    /// Returns a DER `TimeStampToken` (a CMS ContentInfo) for the SHA-256 `digest`.
    fn token_for_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>>;
}

/// A time-stamp authority reached over HTTP (RFC 3161), optionally with basic authentication.
pub struct HttpTimestampSource {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

fn timestamp_request(digest: &[u8; 32]) -> Vec<u8> {
    let sha256 = der::sequence(&[&der::oid(&[2, 16, 840, 1, 101, 3, 4, 2, 1]), &der::null()]);
    let imprint = der::sequence(&[&sha256, &der::octet_string(digest)]);
    let mut nonce = [0u8; 8];
    let _ = getrandom::fill(&mut nonce);
    nonce[0] &= 0x7F;
    der::sequence(&[&der::integer_small(1), &imprint, &der::integer_unsigned(&nonce), &der::boolean(true)])
}

/// Extracts the token from a `TimeStampResp`; status 0 and 1 are "granted".
fn timestamp_token_from_response(bytes: &[u8]) -> Result<Vec<u8>> {
    let (response, _) = der::read(bytes).ok_or_else(|| Error::Other("malformed time stamp response".into()))?;
    let parts = der::children(response.content);
    let status = parts
        .first()
        .and_then(|s| der::children(s.content).into_iter().next())
        .and_then(|i| i.content.last().copied())
        .ok_or_else(|| Error::Other("malformed time stamp status".into()))?;
    if status > 1 {
        return Err(Error::Other(format!("the time stamp authority refused the request (status {status})")));
    }
    parts
        .get(1)
        .map(|t| t.raw.to_vec())
        .ok_or_else(|| Error::Other("the time stamp response has no token".into()))
}

impl TimestampSource for HttpTimestampSource {
    fn token_for_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>> {
        use base64::Engine;
        let body = timestamp_request(digest);
        let mut request = ureq::post(&self.url)
            .header("Content-Type", "application/timestamp-query")
            .header("Character-Encoding", "binary")
            .header("User-Agent", "Carbide PDF");
        if let (Some(user), Some(password)) = (&self.username, &self.password) {
            let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
            request = request.header("Authorization", format!("Basic {credentials}"));
        }
        let mut response = request
            .send(&body[..])
            .map_err(|e| Error::Other(format!("could not reach the time stamp authority: {e}")))?;
        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| Error::Other(format!("could not read the time stamp response: {e}")))?;
        timestamp_token_from_response(&bytes)
    }
}

// ---- CMS -----------------------------------------------------------------------------------

const OID_DATA: &[u32] = &[1, 2, 840, 113549, 1, 7, 1];
const OID_SIGNED_DATA: &[u32] = &[1, 2, 840, 113549, 1, 7, 2];
const OID_SHA256: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];
const OID_SHA256_RSA: &[u32] = &[1, 2, 840, 113549, 1, 1, 11];
const OID_CONTENT_TYPE: &[u32] = &[1, 2, 840, 113549, 1, 9, 3];
const OID_MESSAGE_DIGEST: &[u32] = &[1, 2, 840, 113549, 1, 9, 4];
const OID_SIGNING_CERT_V2: &[u32] = &[1, 2, 840, 113549, 1, 9, 16, 2, 47];
const OID_SIGNATURE_TIMESTAMP: &[u32] = &[1, 2, 840, 113549, 1, 9, 16, 2, 14];

fn attribute(oid: &[u32], value: &[u8]) -> Vec<u8> {
    der::sequence(&[&der::oid(oid), &der::set(&[value])])
}

/// Builds a detached CMS SignedData over `content_digest` (SHA-256 of the signed byte ranges).
///
/// Plain PKCS#7 carries no signed attributes (the signature is over the digest itself, as in the
/// original). CAdES adds contentType, messageDigest and signingCertificateV2.
fn build_cms(
    identity: &Identity,
    content_digest: &[u8; 32],
    cades: bool,
    timestamps: Option<&dyn TimestampSource>,
) -> Result<Vec<u8>> {
    let certificate = Certificate::from_der(identity.certificate_der()).map_err(|e| Error::Other(e.to_string()))?;
    let issuer = certificate.tbs_certificate.issuer.to_der().map_err(|e| Error::Other(e.to_string()))?;
    let serial = certificate.tbs_certificate.serial_number.to_der().map_err(|e| Error::Other(e.to_string()))?;
    let issuer_and_serial = der::sequence(&[&issuer, &serial]);

    let sha256_alg = der::sequence(&[&der::oid(OID_SHA256), &der::null()]);
    let rsa_alg = der::sequence(&[&der::oid(OID_SHA256_RSA), &der::null()]);

    let (signed_attributes, to_sign): (Option<Vec<u8>>, [u8; 32]) = if cades {
        let cert_hash = Sha256::digest(identity.certificate_der());
        let signing_cert = der::sequence(&[&der::sequence(&[&der::sequence(&[&der::octet_string(&cert_hash)])])]);
        let mut attributes = vec![
            attribute(OID_CONTENT_TYPE, &der::oid(OID_DATA)),
            attribute(OID_MESSAGE_DIGEST, &der::octet_string(content_digest)),
            attribute(OID_SIGNING_CERT_V2, &signing_cert),
        ];
        attributes.sort(); // DER SET OF ordering
        let body: Vec<u8> = attributes.concat();
        let as_set = der::tlv(der::SET, &body);
        let digest: [u8; 32] = Sha256::digest(&as_set).into();
        (Some(body), digest)
    } else {
        (None, *content_digest)
    };

    let signature = identity
        .key
        .sign(Pkcs1v15Sign::new::<Sha256>(), &to_sign)
        .map_err(|e| Error::Other(format!("signing failed: {e}")))?;

    let mut signer_info_parts: Vec<Vec<u8>> = vec![der::integer_small(1), issuer_and_serial, sha256_alg.clone()];
    if let Some(body) = &signed_attributes {
        signer_info_parts.push(der::context(0, true, body));
    }
    signer_info_parts.push(rsa_alg);
    signer_info_parts.push(der::octet_string(&signature));
    if let Some(source) = timestamps {
        let imprint: [u8; 32] = Sha256::digest(&signature).into();
        let token = source.token_for_digest(&imprint)?;
        let attr = attribute(OID_SIGNATURE_TIMESTAMP, &token);
        signer_info_parts.push(der::context(1, true, &attr));
    }
    let refs: Vec<&[u8]> = signer_info_parts.iter().map(|p| p.as_slice()).collect();
    let signer_info = der::sequence(&refs);

    let certificates: Vec<u8> = identity.chain.iter().flatten().copied().collect();
    let signed_data = der::sequence(&[
        &der::integer_small(1),
        &der::set(&[&sha256_alg]),
        &der::sequence(&[&der::oid(OID_DATA)]),
        &der::context(0, true, &certificates),
        &der::set(&[&signer_info]),
    ]);
    Ok(der::sequence(&[&der::oid(OID_SIGNED_DATA), &der::context(0, true, &signed_data)]))
}

// ---- signing -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubFilter {
    /// `adbe.pkcs7.detached` (default).
    Pkcs7Detached,
    /// `ETSI.CAdES.detached`.
    CadesDetached,
    /// `ETSI.RFC3161`: a document time stamp, no signer identity.
    DocumentTimeStamp,
}

impl SubFilter {
    pub fn name(self) -> &'static str {
        match self {
            SubFilter::Pkcs7Detached => "adbe.pkcs7.detached",
            SubFilter::CadesDetached => "ETSI.CAdES.detached",
            SubFilter::DocumentTimeStamp => "ETSI.RFC3161",
        }
    }
}

/// How much of the document a certifying signature allows to change afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertifyLevel {
    /// No changes (DocMDP P = 1).
    NoChanges = 1,
    /// Form filling and signing (P = 2).
    FormFilling = 2,
    /// Form filling, signing and commenting (P = 3).
    Commenting = 3,
}

pub struct SignOptions<'a> {
    pub identity: Option<&'a Identity>,
    pub sub_filter: SubFilter,
    pub reason: Option<String>,
    pub location: Option<String>,
    pub contact: Option<String>,
    /// Zero-based page and display-space rectangle of a visible signature; `None` is invisible.
    pub appearance: Option<(usize, Rect)>,
    pub certify: Option<CertifyLevel>,
    /// "Lock Document After Signing": records a `/Lock` on the field.
    pub lock_document: bool,
    pub timestamp: Option<&'a dyn TimestampSource>,
    /// PDF date string; defaults to the current time.
    pub signing_time: Option<String>,
}

impl Default for SignOptions<'_> {
    fn default() -> Self {
        Self {
            identity: None,
            sub_filter: SubFilter::Pkcs7Detached,
            reason: None,
            location: None,
            contact: None,
            appearance: None,
            certify: None,
            lock_document: false,
            timestamp: None,
            signing_time: None,
        }
    }
}

const BYTE_RANGE_PLACEHOLDER: &str = "9999999999 9999999999 9999999999 9999999999";

fn pdf_text(text: &str) -> Object {
    Object::String(encode_text(text), StringFormat::Literal)
}

impl Pdf {
    /// Signs the document. `base_bytes` is the file as currently on disk; the returned bytes are
    /// that file plus one incremental update containing any pending edits and the signature.
    pub fn sign(&self, base_bytes: &[u8], options: &SignOptions<'_>) -> Result<Vec<u8>> {
        if self.doc.is_encrypted() {
            return Err(Error::Unsupported("signing encrypted documents is not supported".into()));
        }
        let identity = options.identity;
        let doc_timestamp = options.sub_filter == SubFilter::DocumentTimeStamp;
        if !doc_timestamp && identity.is_none() {
            return Err(Error::Other("a signing identity is required".into()));
        }
        if doc_timestamp && options.timestamp.is_none() {
            return Err(Error::Other("a time stamp authority is required".into()));
        }
        if options.certify.is_some() && (doc_timestamp || self.has_signatures()) {
            return Err(Error::Other(
                "A certifying signature must be the first signature in the document.".into(),
            ));
        }

        // How much room the signature needs: measure with a dummy digest, as the original does.
        let measure = [0u8; 32];
        let reserved = if doc_timestamp {
            let token = options.timestamp.expect("checked").token_for_digest(&measure)?;
            token.len() + 100
        } else {
            let identity = identity.expect("checked");
            let cades = options.sub_filter == SubFilter::CadesDetached;
            build_cms(identity, &measure, cades, options.timestamp)?.len() + 100
        };

        let mut work = self.clone();
        let signing_time = options.signing_time.clone().unwrap_or_else(pdf_date_now);

        // Marker that makes the placeholder findable in the serialized update.
        let mut marker = [0u8; 16];
        getrandom::fill(&mut marker).map_err(|e| Error::Other(e.to_string()))?;
        let mut contents = marker.to_vec();
        contents.resize(reserved.max(marker.len()), 0);

        let sig_name = identity.map(|i| i.common_name());
        let mut v = dictionary! {
            "Type" => if doc_timestamp { "DocTimeStamp" } else { "Sig" },
            "Filter" => "Adobe.PPKLite",
            "SubFilter" => options.sub_filter.name(),
            "ByteRange" => vec![
                Object::Integer(9_999_999_999), Object::Integer(9_999_999_999),
                Object::Integer(9_999_999_999), Object::Integer(9_999_999_999),
            ],
            "Contents" => Object::String(contents.clone(), StringFormat::Hexadecimal),
        };
        if !doc_timestamp {
            v.set("M", pdf_text(&signing_time));
            if let Some(name) = &sig_name {
                v.set("Name", pdf_text(name));
            }
            for (key, value) in [("Reason", &options.reason), ("Location", &options.location), ("ContactInfo", &options.contact)] {
                if let Some(text) = value.as_ref().filter(|t| !t.is_empty()) {
                    v.set(key, pdf_text(text));
                }
            }
            v.set("Prop_Build", dictionary! { "App" => dictionary! { "Name" => "Carbide PDF" } });
        }
        let mut references: Vec<Object> = Vec::new();
        if let Some(level) = options.certify {
            references.push(Object::Dictionary(dictionary! {
                "Type" => "SigRef",
                "TransformMethod" => "DocMDP",
                "TransformParams" => dictionary! { "Type" => "TransformParams", "P" => level as i64, "V" => "1.2" },
            }));
        }
        if options.lock_document && !doc_timestamp {
            references.push(Object::Dictionary(dictionary! {
                "Type" => "SigRef",
                "TransformMethod" => "FieldMDP",
                "TransformParams" => dictionary! { "Type" => "TransformParams", "Action" => "All", "V" => "1.2" },
            }));
        }
        if !references.is_empty() {
            v.set("Reference", Object::Array(references));
        }
        let v_id = work.doc.add_object(v);

        work.add_signature_field(v_id, options, &signing_time)?;
        if options.certify.is_some() {
            let catalog = work.doc.catalog_mut()?;
            let mut perms = match catalog.get(b"Perms") {
                Ok(Object::Dictionary(d)) => d.clone(),
                _ => Dictionary::new(),
            };
            perms.set("DocMDP", Object::Reference(v_id));
            catalog.set("Perms", Object::Dictionary(perms));
        }

        let mut file = work.incremental_update(base_bytes)?;

        // Locate the placeholders in the appended part.
        let appended_from = base_bytes.len();
        let hex_marker = marker.iter().map(|b| format!("{b:02X}")).collect::<String>();
        let hex_marker_lower = hex_marker.to_lowercase();
        let needle_at = |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).position(|w| w == needle);
        let appended = &file[appended_from..];
        let marker_at = needle_at(appended, hex_marker.as_bytes())
            .or_else(|| needle_at(appended, hex_marker_lower.as_bytes()))
            .ok_or_else(|| Error::Other("could not locate the signature placeholder".into()))?;
        let contents_start = appended_from + marker_at - 1; // the '<'
        let contents_len = 2 * contents.len() + 2;
        let contents_end = contents_start + contents_len;
        if file.get(contents_start) != Some(&b'<') || file.get(contents_end - 1) != Some(&b'>') {
            return Err(Error::Other("unexpected signature placeholder layout".into()));
        }
        let range_at = needle_at(&file[appended_from..], BYTE_RANGE_PLACEHOLDER.as_bytes())
            .map(|p| appended_from + p)
            .ok_or_else(|| Error::Other("could not locate the byte range placeholder".into()))?;

        let total = file.len();
        let ranges = [0usize, contents_start, contents_end, total - contents_end];
        let text = format!("{:010} {:010} {:010} {:010}", ranges[0], ranges[1], ranges[2], ranges[3]);
        debug_assert_eq!(text.len(), BYTE_RANGE_PLACEHOLDER.len());
        file[range_at..range_at + text.len()].copy_from_slice(text.as_bytes());

        // Hash both ranges (the byte range text above is itself inside the signed data).
        let mut hasher = Sha256::new();
        hasher.update(&file[..contents_start]);
        hasher.update(&file[contents_end..]);
        let digest: [u8; 32] = hasher.finalize().into();

        let signature = if doc_timestamp {
            options.timestamp.expect("checked").token_for_digest(&digest)?
        } else {
            let cades = options.sub_filter == SubFilter::CadesDetached;
            build_cms(identity.expect("checked"), &digest, cades, options.timestamp)?
        };
        if signature.len() > contents.len() {
            return Err(Error::Other("the signature does not fit the space reserved for it".into()));
        }
        let hex: String = signature.iter().map(|b| format!("{b:02X}")).collect();
        let slot = &mut file[contents_start + 1..contents_end - 1];
        slot.fill(b'0');
        slot[..hex.len()].copy_from_slice(hex.as_bytes());
        Ok(file)
    }

    /// True if the document already contains a signature dictionary.
    pub fn has_signatures(&self) -> bool {
        self.doc.objects.values().any(|o| match o {
            Object::Dictionary(d) => d.has(b"ByteRange") && d.has(b"Contents") && d.has(b"SubFilter"),
            _ => false,
        })
    }

    fn add_signature_field(&mut self, v_id: ObjectId, options: &SignOptions<'_>, signing_time: &str) -> Result<()> {
        let page_index = options.appearance.map(|(p, _)| p).unwrap_or(0);
        let page_id = self.page_id(page_index)?;
        let geometry = self.page_geometry(page_index)?;

        // A unique field name, "Signature1", "Signature2", ...
        let existing = self.field_names();
        let name = (1..)
            .map(|n| format!("Signature{n}"))
            .find(|candidate| !existing.contains(candidate))
            .expect("unbounded range");

        let (rect, visible) = match options.appearance {
            Some((_, r)) if !r.is_empty() => {
                let m = geometry.map_rect(r);
                ([m.x, m.y, m.max_x(), m.max_y()], true)
            }
            _ => ([0.0, 0.0, 0.0, 0.0], false),
        };

        let mut widget = dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Sig",
            "T" => pdf_text(&name),
            "V" => v_id,
            "P" => page_id,
            "F" => 132, // Print + Locked
            "Rect" => rect.iter().map(|v| Object::Real(*v)).collect::<Vec<_>>(),
        };
        if options.lock_document {
            widget.set("Lock", dictionary! { "Type" => "SigFieldLock", "Action" => "All", "P" => 1 });
        }
        if visible {
            let (w, h) = (rect[2] - rect[0], rect[3] - rect[1]);
            let [a, b, c, d, _, _] = geometry.display_to_user();
            let font = self.doc.add_object(dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
            });
            let (dw, dh) = match options.appearance {
                Some((_, r)) => (r.width, r.height),
                None => (w, h),
            };
            let lines = appearance_lines(self_name(options), signing_time, options);
            let size = (dh / (lines.len().max(1) as f32 + 1.0)).clamp(6.0, 11.0);
            let mut content = format!("q 0.2 w 0 0 {} {} re S Q\nBT /F1 {} Tf 0 g 3 {} Td {} TL\n", fmt(dw), fmt(dh), fmt(size), fmt(dh - size - 2.0), fmt(size * 1.15))
                .into_bytes();
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    content.extend_from_slice(b"T*\n");
                }
                content.extend_from_slice(&literal_string(&win_ansi(line)));
                content.extend_from_slice(b" Tj\n");
            }
            content.extend_from_slice(b"ET\n");
            let appearance = self.doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), Object::Real(dw), Object::Real(dh)],
                    "Matrix" => vec![Object::Real(a), Object::Real(b), Object::Real(c), Object::Real(d), 0.into(), 0.into()],
                    "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
                },
                content,
            ));
            widget.set("AP", dictionary! { "N" => appearance });
        }
        let widget_id = self.doc.add_object(widget);

        // Page annotations.
        let page = self.doc.get_dictionary_mut(page_id)?;
        match page.get_mut(b"Annots") {
            Ok(Object::Array(items)) => items.push(Object::Reference(widget_id)),
            _ => page.set("Annots", Object::Array(vec![Object::Reference(widget_id)])),
        }

        // AcroForm: fields and SigFlags (SignaturesExist | AppendOnly).
        let acroform = match self.doc.catalog()?.get(b"AcroForm") {
            Ok(Object::Reference(id)) => Some(*id),
            _ => None,
        };
        match acroform {
            Some(id) => {
                let form = self.doc.get_dictionary_mut(id)?;
                match form.get_mut(b"Fields") {
                    Ok(Object::Array(items)) => items.push(Object::Reference(widget_id)),
                    _ => form.set("Fields", Object::Array(vec![Object::Reference(widget_id)])),
                }
                form.set("SigFlags", 3);
            }
            None => {
                let existing = match self.doc.catalog()?.get(b"AcroForm") {
                    Ok(Object::Dictionary(d)) => Some(d.clone()),
                    _ => None,
                };
                let mut form = existing.unwrap_or_default();
                match form.get_mut(b"Fields") {
                    Ok(Object::Array(items)) => items.push(Object::Reference(widget_id)),
                    _ => form.set("Fields", Object::Array(vec![Object::Reference(widget_id)])),
                }
                form.set("SigFlags", 3);
                self.doc.catalog_mut()?.set("AcroForm", Object::Dictionary(form));
            }
        }
        Ok(())
    }

    fn field_names(&self) -> BTreeSet<String> {
        self.doc
            .objects
            .values()
            .filter_map(|o| match o {
                Object::Dictionary(d) if d.has(b"FT") => d.get(b"T").ok()?.as_str().ok().map(decode_text),
                _ => None,
            })
            .collect()
    }
}

fn self_name(options: &SignOptions<'_>) -> String {
    options.identity.map(|i| i.common_name()).unwrap_or_default()
}

fn appearance_lines(name: String, signing_time: &str, options: &SignOptions<'_>) -> Vec<String> {
    let mut lines = vec![format!("Digitally signed by {name}")];
    lines.push(format!("Date: {}", crate::revisions::pdf_date_to_local(signing_time)));
    if let Some(reason) = options.reason.as_ref().filter(|r| !r.is_empty()) {
        lines.push(format!("Reason: {reason}"));
    }
    if let Some(location) = options.location.as_ref().filter(|l| !l.is_empty()) {
        lines.push(format!("Location: {location}"));
    }
    lines
}

// ---- validation ----------------------------------------------------------------------------

/// Handler validity state (the original's `A`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    Blank = 0,
    Unknown = 1,
    Invalid = 2,
    ValidityUnknown = 3,
    Valid = 4,
}

/// Detail code (the original's `B`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    NotVerified = 0,
    Unknown = 1,
    BadByteRange = 2,
    DigestMismatch = 3,
    Corrupt = 4,
    UnsupportedFilter = 5,
    Expired = 6,
    TrustedViaChain = 7,
    TrustedDirectly = 8,
    DigestOkUntrusted = 9,
}

#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    /// DER certificates the user trusts.
    pub certificates: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct SignatureReport {
    pub field_name: String,
    pub signer: Option<String>,
    pub sub_filter: Option<String>,
    pub reason: Option<String>,
    pub location: Option<String>,
    pub signing_time: Option<String>,
    pub validity: Validity,
    pub detail: Detail,
    /// The file continues past the end of this signature's byte range.
    pub modified_after_signing: bool,
    pub certifying: bool,
    pub document_time_stamp: bool,
    pub has_embedded_timestamp: bool,
    /// File offset where this signature's revision ends (for ordering).
    pub revision_end: usize,
}

impl SignatureReport {
    /// The panel's status line.
    pub fn status_line(&self) -> &'static str {
        match (self.validity, self.detail) {
            (Validity::Blank, _) => messages::UNSIGNED_FIELD,
            (Validity::Valid, _) => messages::VALID,
            (Validity::ValidityUnknown, _) | (Validity::Unknown, _) => messages::VALIDITY_UNKNOWN,
            (Validity::Invalid, Detail::DigestOkUntrusted) => messages::VALIDITY_UNKNOWN,
            (Validity::Invalid, _) => messages::INVALID,
        }
    }

    /// The panel's description lines, in the original's wording.
    pub fn description_lines(&self) -> Vec<&'static str> {
        let mut lines = Vec::new();
        let modification = if self.modified_after_signing {
            messages::SUBSEQUENT_CHANGES
        } else {
            messages::NOT_MODIFIED
        };
        match self.validity {
            Validity::Valid => {
                lines.push(if self.modified_after_signing { messages::REVISION_UNALTERED } else { modification });
                if self.modified_after_signing {
                    lines.push(messages::SUBSEQUENT_CHANGES);
                }
            }
            Validity::ValidityUnknown => {
                if self.detail == Detail::Expired {
                    lines.push(messages::IDENTITY_EXPIRED);
                } else {
                    lines.push(modification);
                    lines.push(messages::IDENTITY_UNKNOWN);
                }
            }
            Validity::Invalid => lines.push(match self.detail {
                Detail::BadByteRange => messages::BAD_BYTE_RANGE,
                Detail::Corrupt => messages::CORRUPTED,
                Detail::UnsupportedFilter => messages::FILTER_NOT_SUPPORTED,
                _ => messages::DOCUMENT_ALTERED,
            }),
            _ => {}
        }
        if self.has_embedded_timestamp || self.document_time_stamp {
            lines.push(messages::TIME_STAMPED);
        }
        lines
    }
}

/// The notice bar text for a set of reports (the original's aggregation order).
pub fn notice_text(reports: &[SignatureReport]) -> Option<&'static str> {
    let mut signed: Vec<&SignatureReport> = reports.iter().filter(|r| r.validity != Validity::Blank).collect();
    if signed.is_empty() {
        return None;
    }
    signed.sort_by_key(|r| r.revision_end);
    if signed.iter().any(|r| r.validity == Validity::Unknown && r.detail == Detail::NotVerified) {
        return Some(messages::NOTICE_NOT_VERIFIED);
    }
    if signed.iter().any(|r| !matches!(r.validity, Validity::Valid | Validity::ValidityUnknown)) {
        return Some(messages::NOTICE_PROBLEMS);
    }
    if signed.last().is_some_and(|r| r.modified_after_signing) {
        return Some(messages::NOTICE_CHANGED_AFTER_LAST);
    }
    if signed.iter().any(|r| r.modified_after_signing) {
        return Some(messages::NOTICE_UPDATED);
    }
    Some(messages::NOTICE_ALL_VALID)
}

#[derive(Debug)]
struct ParsedCms {
    certificates: Vec<Vec<u8>>,
    digest_oid: String,
    signed_attributes: Option<Vec<u8>>,
    message_digest: Option<Vec<u8>>,
    signature: Vec<u8>,
    sid_issuer: Vec<u8>,
    sid_serial: Vec<u8>,
    has_timestamp: bool,
}

fn parse_cms(data: &[u8]) -> Option<ParsedCms> {
    let (content_info, _) = der::read(data)?;
    if content_info.tag != der::SEQUENCE {
        return None;
    }
    let parts = der::children(content_info.content);
    let explicit = parts.get(1)?;
    let (signed_data, _) = der::read(explicit.content)?;
    let fields = der::children(signed_data.content);
    let mut certificates = Vec::new();
    let mut signer_infos = None;
    for field in fields.iter().skip(3) {
        match field.tag {
            0xA0 => certificates = der::children(field.content).iter().map(|c| c.raw.to_vec()).collect(),
            0x31 => signer_infos = Some(field),
            _ => {}
        }
    }
    let signer = der::children(signer_infos?.content).into_iter().next()?;
    let parts = der::children(signer.content);
    let sid = parts.get(1)?;
    let sid_parts = der::children(sid.content);
    let (sid_issuer, sid_serial) = (sid_parts.first()?.raw.to_vec(), sid_parts.get(1)?.raw.to_vec());
    let digest_alg = der::children(parts.get(2)?.content);
    let digest_oid = der::oid_to_string(digest_alg.first()?.content);

    let mut index = 3;
    let mut signed_attributes = None;
    let mut message_digest = None;
    if parts.get(index)?.tag == 0xA0 {
        let attrs = parts[index];
        for attr in der::children(attrs.content) {
            let kids = der::children(attr.content);
            if kids.first().map(|o| der::oid_to_string(o.content)).as_deref() == Some("1.2.840.113549.1.9.4") {
                if let Some(values) = kids.get(1) {
                    message_digest = der::children(values.content).first().map(|v| v.content.to_vec());
                }
            }
        }
        signed_attributes = Some(attrs.content.to_vec());
        index += 1;
    }
    index += 1; // signatureAlgorithm
    let signature = parts.get(index)?.content.to_vec();
    let has_timestamp = parts.iter().skip(index + 1).any(|p| p.tag == 0xA1);
    Some(ParsedCms { certificates, digest_oid, signed_attributes, message_digest, signature, sid_issuer, sid_serial, has_timestamp })
}

fn digest_by_oid(oid: &str, parts: &[&[u8]]) -> Option<Vec<u8>> {
    fn run<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
        let mut h = D::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().to_vec()
    }
    match oid {
        "2.16.840.1.101.3.4.2.1" => Some(run::<Sha256>(parts)),
        "2.16.840.1.101.3.4.2.2" => Some(run::<Sha384>(parts)),
        "2.16.840.1.101.3.4.2.3" => Some(run::<Sha512>(parts)),
        _ => None,
    }
}

fn rsa_verify(public_key: &RsaPublicKey, digest_oid: &str, digest: &[u8], signature: &[u8]) -> bool {
    let scheme = match digest_oid {
        "2.16.840.1.101.3.4.2.1" => Pkcs1v15Sign::new::<Sha256>(),
        "2.16.840.1.101.3.4.2.2" => Pkcs1v15Sign::new::<Sha384>(),
        "2.16.840.1.101.3.4.2.3" => Pkcs1v15Sign::new::<Sha512>(),
        _ => return false,
    };
    public_key.verify(scheme, digest, signature).is_ok()
}

fn public_key_of(certificate: &Certificate) -> Option<RsaPublicKey> {
    let spki = certificate.tbs_certificate.subject_public_key_info.to_der().ok()?;
    RsaPublicKey::from_public_key_der(&spki).ok()
}

/// Does `child` carry a valid signature from `issuer`'s key?
fn issued_by(child: &Certificate, issuer: &Certificate) -> bool {
    if child.tbs_certificate.issuer != issuer.tbs_certificate.subject {
        return false;
    }
    let (Some(key), Ok(tbs)) = (public_key_of(issuer), child.tbs_certificate.to_der()) else { return false };
    let Some(signature) = child.signature.as_bytes() else { return false };
    let algorithm = child.signature_algorithm.oid.to_string();
    let digest_oid = match algorithm.as_str() {
        "1.2.840.113549.1.1.11" => "2.16.840.1.101.3.4.2.1",
        "1.2.840.113549.1.1.12" => "2.16.840.1.101.3.4.2.2",
        "1.2.840.113549.1.1.13" => "2.16.840.1.101.3.4.2.3",
        _ => return false,
    };
    digest_by_oid(digest_oid, &[&tbs]).is_some_and(|d| rsa_verify(&key, digest_oid, &d, signature))
}

fn trust_level(signer: &Certificate, embedded: &[Certificate], store: &TrustStore) -> Option<Detail> {
    let trusted: Vec<Certificate> = store.certificates.iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
    let signer_der = signer.to_der().ok()?;
    if store.certificates.iter().any(|d| *d == signer_der) {
        return Some(Detail::TrustedDirectly);
    }
    let mut current = signer.clone();
    for _ in 0..8 {
        let der = current.to_der().ok()?;
        if store.certificates.iter().any(|d| *d == der) {
            return Some(Detail::TrustedViaChain);
        }
        let next = trusted
            .iter()
            .chain(embedded.iter())
            .find(|candidate| issued_by(&current, candidate) && candidate.to_der().ok() != Some(der.clone()));
        match next {
            Some(parent) => {
                if let Ok(parent_der) = parent.to_der() {
                    if store.certificates.iter().any(|d| *d == parent_der) {
                        return Some(Detail::TrustedViaChain);
                    }
                }
                current = parent.clone();
            }
            None => return None,
        }
    }
    None
}

fn text_of(dict: &Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key).ok()?.as_str().ok().map(decode_text)
}

fn integer_array(object: Option<&Object>) -> Option<Vec<i64>> {
    match object? {
        Object::Array(items) => items.iter().map(|o| o.as_i64().ok()).collect(),
        _ => None,
    }
}

impl Pdf {
    /// Validates every signature in `file_bytes` (the file this document was loaded from).
    /// `now` is the reference time for certificate validity when a signature has no usable date.
    pub fn verify_signatures(file_bytes: &[u8], trust: &TrustStore, now: DateTime<Utc>) -> Result<Vec<SignatureReport>> {
        let pdf = Pdf::from_bytes(file_bytes)?;
        let file_end = file_bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map(|p| p + 1).unwrap_or(0);

        // Field name per signature dictionary id.
        let mut names: std::collections::HashMap<ObjectId, String> = std::collections::HashMap::new();
        for object in pdf.doc.objects.values() {
            if let Object::Dictionary(d) = object {
                if let (Ok(Object::Reference(v)), Some(name)) = (d.get(b"V"), text_of(d, b"T")) {
                    names.insert(*v, name);
                }
            }
        }

        let mut reports = Vec::new();
        for (id, object) in &pdf.doc.objects {
            let Object::Dictionary(v) = object else { continue };
            if !(v.has(b"ByteRange") && v.has(b"Contents") && v.has(b"SubFilter")) {
                continue;
            }
            reports.push(verify_one(&pdf, *id, v, names.get(id).cloned(), file_bytes, file_end, trust, now));
        }
        reports.sort_by_key(|r| r.revision_end);
        Ok(reports)
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_one(
    pdf: &Pdf,
    id: ObjectId,
    v: &Dictionary,
    name: Option<String>,
    file: &[u8],
    file_end: usize,
    trust: &TrustStore,
    now: DateTime<Utc>,
) -> SignatureReport {
    let sub_filter = v.get(b"SubFilter").ok().and_then(|o| o.as_name().ok()).map(|n| String::from_utf8_lossy(n).into_owned());
    let mut report = SignatureReport {
        field_name: name.unwrap_or_else(|| format!("object {}", id.0)),
        signer: text_of(v, b"Name"),
        sub_filter: sub_filter.clone(),
        reason: text_of(v, b"Reason"),
        location: text_of(v, b"Location"),
        signing_time: text_of(v, b"M"),
        validity: Validity::Unknown,
        detail: Detail::NotVerified,
        modified_after_signing: false,
        certifying: pdf.is_certifying_signature(id),
        document_time_stamp: v.get(b"Type").ok().and_then(|o| o.as_name().ok()) == Some(b"DocTimeStamp"),
        has_embedded_timestamp: false,
        revision_end: 0,
    };

    let invalid = |report: &mut SignatureReport, detail: Detail| {
        report.validity = Validity::Invalid;
        report.detail = detail;
    };

    // 1. Sub-filter.
    const SUPPORTED: [&str; 3] = ["adbe.pkcs7.detached", "ETSI.CAdES.detached", "ETSI.RFC3161"];
    if !sub_filter.as_deref().is_some_and(|f| SUPPORTED.contains(&f)) {
        invalid(&mut report, Detail::UnsupportedFilter);
        return report;
    }

    // 2. Byte range sanity.
    let Some(range) = integer_array(v.get(b"ByteRange").ok()).filter(|r| r.len() == 4) else {
        invalid(&mut report, Detail::Corrupt);
        return report;
    };
    let (a, b, c, d) = (range[0], range[1], range[2], range[3]);
    let consistent = a == 0
        && b >= 0
        && b <= c
        && c >= 0
        && d >= 0
        && (c + d) as usize <= file.len()
        && file.get(b as usize) == Some(&b'<')
        && c >= 1
        && file.get(c as usize - 1) == Some(&b'>');
    if !consistent {
        invalid(&mut report, Detail::BadByteRange);
        return report;
    }
    report.revision_end = (c + d) as usize;
    report.modified_after_signing = report.revision_end < file_end;

    // 3. Parse the CMS (or time-stamp token).
    let Some(contents) = v.get(b"Contents").ok().and_then(|o| o.as_str().ok()).map(|s| s.to_vec()) else {
        invalid(&mut report, Detail::Corrupt);
        return report;
    };
    let Some(cms) = parse_cms(&contents) else {
        if report.document_time_stamp && der::read(&contents).is_some() {
            // A time-stamp token: the digest check is done below; the TSA signature is not.
            return verify_document_timestamp(report, &contents, file, b as usize, c as usize);
        }
        invalid(&mut report, Detail::Corrupt);
        return report;
    };
    report.has_embedded_timestamp = cms.has_timestamp;

    let covered: [&[u8]; 2] = [&file[..b as usize], &file[c as usize..(c + d) as usize]];
    let Some(content_digest) = digest_by_oid(&cms.digest_oid, &covered) else {
        invalid(&mut report, Detail::UnsupportedFilter);
        return report;
    };

    // Signer certificate: match issuer and serial, else the first embedded certificate.
    let certificates: Vec<Certificate> = cms.certificates.iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
    let signer = certificates
        .iter()
        .find(|c| {
            c.tbs_certificate.issuer.to_der().ok().as_deref() == Some(cms.sid_issuer.as_slice())
                && c.tbs_certificate.serial_number.to_der().ok().as_deref() == Some(cms.sid_serial.as_slice())
        })
        .or_else(|| certificates.first());
    let Some(signer) = signer else {
        invalid(&mut report, Detail::Corrupt);
        return report;
    };
    let Some(public_key) = public_key_of(signer) else {
        invalid(&mut report, Detail::UnsupportedFilter);
        return report;
    };
    if report.signer.is_none() {
        report.signer = Some(common_name(signer));
    }

    // 4. Integrity: message digest and signature value.
    let signature_ok = match &cms.signed_attributes {
        Some(attrs) => {
            if cms.message_digest.as_deref() != Some(content_digest.as_slice()) {
                invalid(&mut report, Detail::DigestMismatch);
                return report;
            }
            let as_set = der::tlv(der::SET, attrs);
            match digest_by_oid(&cms.digest_oid, &[&as_set]) {
                Some(d) => rsa_verify(&public_key, &cms.digest_oid, &d, &cms.signature),
                None => false,
            }
        }
        None => {
            if !rsa_verify(&public_key, &cms.digest_oid, &content_digest, &cms.signature) {
                // No message digest to compare: a failing signature means altered content.
                invalid(&mut report, Detail::DigestMismatch);
                return report;
            }
            true
        }
    };
    if !signature_ok {
        invalid(&mut report, Detail::Corrupt);
        return report;
    }

    // 5. Trust and validity period.
    let reference_time = report
        .signing_time
        .as_deref()
        .and_then(parse_pdf_date)
        .unwrap_or(now);
    if reference_time > now + chrono::Duration::minutes(5) {
        report.validity = Validity::ValidityUnknown;
        report.detail = Detail::DigestOkUntrusted;
        return report;
    }
    let validity = &signer.tbs_certificate.validity;
    let (not_before, not_after) = (
        Utc.timestamp_opt(validity.not_before.to_unix_duration().as_secs() as i64, 0).single(),
        Utc.timestamp_opt(validity.not_after.to_unix_duration().as_secs() as i64, 0).single(),
    );
    if let (Some(nb), Some(na)) = (not_before, not_after) {
        if reference_time < nb || reference_time > na {
            report.validity = Validity::ValidityUnknown;
            report.detail = Detail::Expired;
            return report;
        }
    }
    match trust_level(signer, &certificates, trust) {
        Some(level) => {
            report.validity = Validity::Valid;
            report.detail = level;
        }
        None => {
            report.validity = Validity::ValidityUnknown;
            report.detail = Detail::DigestOkUntrusted;
        }
    }
    report
}

fn verify_document_timestamp(mut report: SignatureReport, token: &[u8], file: &[u8], b: usize, c: usize) -> SignatureReport {
    // The token's message imprint must equal the SHA-256 of the covered ranges.
    let digest: [u8; 32] = {
        let mut h = Sha256::new();
        h.update(&file[..b]);
        h.update(&file[c..]);
        h.finalize().into()
    };
    let found = token.windows(32).any(|w| w == digest);
    if found {
        report.validity = Validity::ValidityUnknown;
        report.detail = Detail::DigestOkUntrusted;
    } else {
        report.validity = Validity::Invalid;
        report.detail = Detail::DigestMismatch;
    }
    report
}

impl Pdf {
    fn is_certifying_signature(&self, id: ObjectId) -> bool {
        self.doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Perms").ok())
            .map(|p| self.resolve(p))
            .and_then(|p| p.as_dict().ok().and_then(|d| d.get(b"DocMDP").ok().cloned()))
            .is_some_and(|o| matches!(o, Object::Reference(r) if r == id))
    }
}
