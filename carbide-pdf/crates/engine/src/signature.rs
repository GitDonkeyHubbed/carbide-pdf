//! Digital signatures: signing (PKCS#7 detached, CAdES, document time stamps, certification) and
//! validation.
//!
//! The pipeline: a signature field is created, its `/V`
//! dictionary is written with a fixed-size `/Contents` placeholder and a fixed-width `/ByteRange`,
//! the file is saved as an incremental update (so earlier signatures stay valid), the real
//! `/ByteRange` is patched in, and the CMS signature over the two byte ranges is written into the
//! placeholder. Digest is SHA-256 and the signature RSA PKCS#1 v1.5 with SHA-256.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, TimeZone, Utc};
use lopdf::xref::XrefEntry;
use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream, StringFormat};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};
use x509_cert::Certificate;

use crate::cms;
use crate::der;
use crate::sigcrypto::{self, Verified};
use crate::error::{Error, Result};
use crate::fonts::{literal_string, win_ansi};
use crate::geometry::{fmt, Rect};
use crate::pdf::Pdf;
use crate::revisions::{decode_text, encode_text, parse_pdf_date, pdf_date_now};

/// User-facing messages.
pub mod messages {
    pub const UNSIGNED_FIELD: &str = "Empty signature field. Click to sign.";
    pub const NOT_YET_VERIFIED: &str = "This signature has not been checked yet";
    pub const VALID: &str = "The signature is valid";
    pub const VALIDITY_UNKNOWN: &str = "The signature could not be fully validated";
    pub const INVALID: &str = "The signature is not valid";
    pub const TIMESTAMP_VALID: &str = "The time stamp is valid";
    pub const TIMESTAMP_VALIDITY_UNKNOWN: &str = "The time stamp could not be fully validated";
    pub const TIMESTAMP_INVALID: &str = "The time stamp is not valid";
    pub const REVISION_UNALTERED: &str = "The signed version of the document is unchanged";
    pub const SUBSEQUENT_CHANGES: &str = "The document was changed after it was signed";
    pub const NOT_MODIFIED: &str = "The document has not changed since it was signed";
    pub const IDENTITY_UNKNOWN: &str = "The signer's identity is not confirmed: neither the certificate nor any certificate above it is in your trusted list";
    pub const IDENTITY_EXPIRED: &str = "The signer's certificate has expired or is not valid yet, and no trusted time stamp shows it was valid when the document was signed";
    pub const CHAIN_REJECTED: &str = "The signer's certificate leads to your trusted list, but through a certificate that may not issue it or was not valid at the time";
    pub const SIGNER_NOT_PERMITTED: &str = "The signer's certificate is not allowed to sign documents";
    pub const TIMESTAMP_AUTHORITY_UNKNOWN: &str = "The time stamp authority is not confirmed: neither its certificate nor any certificate above it is in your trusted list";
    pub const TIMESTAMP_AUTHORITY_EXPIRED: &str = "The time stamp authority's certificate was not valid at the time it gives";
    pub const TIMESTAMP_CHAIN_REJECTED: &str = "The time stamp authority's certificate leads to your trusted list, but through a certificate that may not issue it or was not valid at the time";
    pub const TIMESTAMP_AUTHORITY_NOT_PERMITTED: &str = "The time stamp authority's certificate is not for time stamping";
    pub const DOCUMENT_TIME_STAMPED: &str = "A trusted time stamp authority confirms this version of the document existed at the time given.";
    pub const DOCUMENT_ALTERED: &str = "The document is damaged or was changed after signing.";
    pub const UNSUPPORTED_ALGORITHM: &str = "This signature uses an algorithm this version cannot check.";
    pub const WEAK_ALGORITHM: &str = "This signature uses SHA-1, which is no longer considered secure.";
    pub const BAD_BYTE_RANGE: &str = "Invalid signature: the signed byte range is wrong.";
    pub const CORRUPTED: &str = "Invalid signature: the signature data is damaged.";
    pub const FILTER_NOT_SUPPORTED: &str = "Invalid signature: this signature handler is not supported.";
    pub const SIGNED_IN_FUTURE: &str = "The signing time is in the future.";
    pub const TIME_STAMP_IN_FUTURE: &str = "The time stamp's time is in the future.";
    pub const NOTICE_NOT_VERIFIED: &str = "Some signatures still need to be validated.";
    pub const NOTICE_PROBLEMS: &str = "This document is signed, but at least one signature has a problem.";
    pub const NOTICE_NOT_CONFIRMED: &str =
        "This document is signed, but at least one signature's identity is not confirmed.";
    pub const NOTICE_NOT_CHECKED: &str = "This document is signed, but at least one signature could not be checked.";
    pub const NOTICE_TIMESTAMP_NOT_CONFIRMED: &str =
        "This document is signed, but at least one time stamp's authority is not confirmed.";
    pub const NOTICE_CHANGED_AFTER_LAST: &str =
        "Signed with valid signatures, but the document changed after the last signature.";
    pub const NOTICE_UPDATED: &str = "Signed with valid signatures; the document was updated after signing.";
    pub const NOTICE_ALL_VALID: &str = "This document is signed and every signature is valid.";
    pub const FIELD_ALREADY_SIGNED: &str = "This field has already been signed.";
    pub const TIME_STAMPED: &str = "The signature carries a time stamp.";
    pub const TIME_STAMP_UNCONFIRMED: &str = "The signature carries a time stamp, but its authority is not confirmed.";
}

// ---- identity ------------------------------------------------------------------------------

/// A signing identity: a certificate chain (signer first) and its RSA private key.
pub struct Identity {
    chain: Vec<Vec<u8>>,
    key: RsaPrivateKey,
}

impl Identity {
    pub fn new(mut chain_der: Vec<Vec<u8>>, key: RsaPrivateKey) -> Result<Identity> {
        if chain_der.is_empty() {
            return Err(Error::Other("the identity has no certificate".into()));
        }
        for der in &chain_der {
            Certificate::from_der(der).map_err(|e| Error::Other(format!("invalid certificate: {e}")))?;
        }
        // The signer certificate is the one whose public key matches the private key. Some files
        // (a CA-first PEM bundle, say) list it after its issuer; move it to the front, and refuse
        // an identity whose key matches none of its certificates.
        let public_key = RsaPublicKey::from(&key);
        let matches = |der: &[u8]| {
            Certificate::from_der(der)
                .ok()
                .and_then(|c| c.tbs_certificate.subject_public_key_info.to_der().ok())
                .and_then(|spki| RsaPublicKey::from_public_key_der(&spki).ok())
                .is_some_and(|candidate| candidate == public_key)
        };
        let signer = chain_der
            .iter()
            .position(|der| matches(der))
            .ok_or_else(|| Error::Other("the private key does not match any of the identity's certificates".into()))?;
        chain_der.swap(0, signer);
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

    /// The signer's name as the certificate gives it, used for the signature's `/Name`: the
    /// common name, else the email address, else the whole subject.
    pub fn common_name(&self) -> String {
        Certificate::from_der(&self.chain[0]).map(|c| subject_name(&c)).unwrap_or_default()
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

const OID_COMMON_NAME: &str = "2.5.4.3";
const OID_EMAIL_ADDRESS: &str = "1.2.840.113549.1.9.1";

/// The signer's display name: common name, else email address, else the whole subject.
fn subject_name(certificate: &Certificate) -> String {
    let subject = &certificate.tbs_certificate.subject;
    name_attribute(subject, OID_COMMON_NAME)
        .or_else(|| name_attribute(subject, OID_EMAIL_ADDRESS))
        .unwrap_or_else(|| subject.to_string())
}

/// The value of the most specific `oid` attribute in `name`, read from the attribute itself
/// rather than from the escaped text form, where a comma inside a value looks like a separator.
fn name_attribute(name: &x509_cert::name::Name, oid: &str) -> Option<String> {
    name.0
        .iter()
        .rev()
        .flat_map(|rdn| rdn.0.iter())
        .find(|attribute| attribute.oid.to_string() == oid)
        .and_then(|attribute| directory_string(&attribute.value))
        .filter(|text| !text.is_empty())
}

/// The text of a DirectoryString (or IA5String) attribute value.
fn directory_string(value: &x509_cert::der::Any) -> Option<String> {
    use x509_cert::der::{Tag, Tagged};
    let bytes = value.value();
    match value.tag() {
        Tag::Utf8String | Tag::PrintableString | Tag::Ia5String | Tag::VisibleString => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        // T.61 in theory; Latin-1 in practice.
        Tag::TeletexString => Some(bytes.iter().map(|b| char::from(*b)).collect()),
        Tag::BmpString if bytes.len().is_multiple_of(2) => {
            let units: Vec<u16> = bytes.chunks(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// The certificate's subjectKeyIdentifier extension value, if it has one.
fn subject_key_id(certificate: &Certificate) -> Option<Vec<u8>> {
    let extensions = certificate.tbs_certificate.extensions.as_ref()?;
    let extension = extensions.iter().find(|e| e.extn_id.to_string() == "2.5.29.14")?;
    // The extension value is an OCTET STRING wrapping the key identifier OCTET STRING.
    der::read(extension.extn_value.as_bytes()).map(|(inner, _)| inner.content.to_vec())
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

/// A time-stamp request, and the content of the random nonce INTEGER it carries (checked against
/// the response).
fn timestamp_request(digest: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    let sha256 = der::sequence(&[&der::oid(OID_SHA256), &der::null()]);
    let imprint = der::sequence(&[&sha256, &der::octet_string(digest)]);
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).expect("system randomness");
    nonce[0] &= 0x7F; // a positive INTEGER
    let nonce = der::integer_unsigned(&nonce);
    let request = der::sequence(&[&der::integer_small(1), &imprint, &nonce, &der::boolean(true)]);
    let content = der::read(&nonce).map(|(e, _)| e.content.to_vec()).unwrap_or_default();
    (request, content)
}

/// Extracts the token from a `TimeStampResp`; status 0 and 1 are "granted".
fn timestamp_token_from_response(bytes: &[u8]) -> Result<Vec<u8>> {
    let (response, _) = der::read_ber(bytes).ok_or_else(|| Error::Other("malformed time stamp response".into()))?;
    let parts = der::children_ber(response.content);
    let status = parts
        .first()
        .and_then(|s| der::children_ber(s.content).into_iter().next())
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

/// Checks that a `TimeStampToken` the authority returned covers `digest` with SHA-256, as asked,
/// echoes `nonce` (the content of the INTEGER sent), and carries a signature that holds.
fn check_timestamp_response(token: &[u8], digest: &[u8; 32], nonce: &[u8]) -> Result<()> {
    let cms = cms::parse_signed_data(token)
        .filter(|cms| cms.content_type == cms::OID_TST_INFO)
        .ok_or_else(|| Error::Other("the time stamp authority returned a malformed token".into()))?;
    let info = cms.content.clone().ok_or_else(|| Error::Other("the time stamp token has no content".into()))?;
    let tst = cms::parse_tst_info(&info).ok_or_else(|| Error::Other("the time stamp token is malformed".into()))?;
    if tst.hash_algorithm != "2.16.840.1.101.3.4.2.1" || tst.hashed_message != digest {
        return Err(Error::Other("the time stamp token covers the wrong digest".into()));
    }
    // The token must echo the nonce we sent (RFC 3161 requires it when the request carried one).
    if tst.nonce.as_deref() != Some(nonce) {
        return Err(Error::Other("the time stamp authority returned a mismatched nonce".into()));
    }
    if timestamp_signature(&cms, &info) != Verified::Ok {
        return Err(Error::Other("the time stamp token did not verify".into()));
    }
    Ok(())
}

/// Verifies a time-stamp token's own SignerInfo (trust is checked separately): its signed
/// contentType is id-ct-TSTInfo, its messageDigest is H(eContent), its ESS signing-certificate
/// attribute, when present, names the certificate that signed, and the signature over the signed
/// attributes holds.
fn timestamp_signature(cms: &cms::SignedData, econtent: &[u8]) -> Verified {
    let signer_info = &cms.signer;
    let certificates: Vec<Certificate> = cms.certificates.iter().filter_map(|c| Certificate::from_der(c).ok()).collect();
    let Some(signer) = find_signer(&certificates, &signer_info.id) else { return Verified::Bad };
    if signer_info.signed_attributes.is_none() || signer_info.content_type.as_deref() != Some(cms::OID_TST_INFO) {
        return Verified::Bad;
    }
    let digest_oid = &signer_info.digest_algorithm;
    match sigcrypto::digest(digest_oid, &[econtent]) {
        Some(content_digest) if signer_info.message_digest.as_deref() == Some(content_digest.as_slice()) => {}
        Some(_) => return Verified::Bad,
        None => return Verified::Unsupported,
    }
    if let Some((hash_oid, hash)) = &signer_info.signing_certificate {
        let Ok(der) = signer.to_der() else { return Verified::Bad };
        match sigcrypto::digest(hash_oid, &[&der]) {
            Some(expected) if &expected == hash => {}
            Some(_) => return Verified::Bad,
            None => return Verified::Unsupported,
        }
    }
    let mut outcome = Verified::Bad;
    for set in signer_info.signed_attribute_encodings() {
        outcome = sigcrypto::verify(
            signer,
            &signer_info.signature_algorithm,
            signer_info.signature_parameters.as_deref(),
            digest_oid,
            &[&set],
            &signer_info.signature,
        );
        if outcome != Verified::Bad {
            break;
        }
    }
    outcome
}

impl TimestampSource for HttpTimestampSource {
    fn token_for_digest(&self, digest: &[u8; 32]) -> Result<Vec<u8>> {
        use base64::Engine;
        let secure = self.url.trim_start().to_ascii_lowercase().starts_with("https://");
        let authenticating = self.username.as_ref().is_some_and(|u| !u.is_empty()) || self.password.is_some();
        if authenticating && !secure {
            return Err(Error::Other(
                "refusing to send time stamp credentials over an insecure connection: use an https:// URL".into(),
            ));
        }
        let (body, nonce) = timestamp_request(digest);
        // An authority that accepts the connection and never answers must not hang the caller.
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).build().into();
        let mut request = agent
            .post(&self.url)
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
        let token = timestamp_token_from_response(&bytes)?;
        check_timestamp_response(&token, digest, &nonce)?;
        Ok(token)
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

    /// True if the document already contains a signature dictionary, or a signature field with a
    /// value (even one that no longer reads as a signature).
    pub fn has_signatures(&self) -> bool {
        self.doc.objects.values().any(|o| match o {
            Object::Dictionary(d) => is_signature_dictionary(d) || signature_field_value(d).is_some(),
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

/// Handler validity state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    Blank = 0,
    Unknown = 1,
    Invalid = 2,
    ValidityUnknown = 3,
    Valid = 4,
}

/// Detail code.
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
    UnsupportedAlgorithm = 10,
    /// Certificates lead to a trusted one by name and signature, but break the issuing rules.
    ChainRejected = 11,
    /// The certificate's key usage does not allow what it was used for.
    NotPermitted = 12,
    /// The time the signature claims is in the future.
    SignedInFuture = 13,
}

#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    /// DER certificates the user trusts.
    pub certificates: Vec<Vec<u8>>,
}

/// Reads the X.509 certificates from `bytes`, which may be PEM (one or more `CERTIFICATE` blocks)
/// or a single DER certificate. Returns their DER encodings, and an error when nothing parses as a
/// certificate, so a mistaken file (a key, a PKCS#12, random bytes) is refused rather than stored.
pub fn read_certificates(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut ders: Vec<Vec<u8>> = Vec::new();
    if let Ok(text) = std::str::from_utf8(bytes) {
        for der in pem_blocks(text, "CERTIFICATE") {
            if Certificate::from_der(&der).is_ok() {
                ders.push(der);
            }
        }
    }
    if ders.is_empty() && Certificate::from_der(bytes).is_ok() {
        ders.push(bytes.to_vec());
    }
    if ders.is_empty() {
        return Err(Error::Other("this file does not contain an X.509 certificate".into()));
    }
    Ok(ders)
}

#[derive(Debug, Clone)]
pub struct SignatureReport {
    pub field_name: String,
    pub signer: Option<String>,
    /// The signer's own `/Name`, kept only when it differs from the certificate subject.
    pub claimed_name: Option<String>,
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
    /// The signature carries a time stamp that verifies.
    pub has_embedded_timestamp: bool,
    /// ...and that time stamp's authority is trusted.
    pub embedded_timestamp_trusted: bool,
    /// The signature uses SHA-1, reported as a weak-algorithm note.
    pub weak_algorithm: bool,
    /// File offset where this signature's revision ends (for ordering).
    pub revision_end: usize,
}

/// A dictionary holding a signature value: `/ByteRange`, `/Contents` and `/SubFilter`.
fn is_signature_dictionary(d: &Dictionary) -> bool {
    d.has(b"ByteRange") && d.has(b"Contents") && d.has(b"SubFilter")
}

/// The object a signature field's `/V` names, for a field (or merged field and widget) of type
/// `/Sig`.
fn signature_field_value(d: &Dictionary) -> Option<ObjectId> {
    let is_signature = d.get(b"FT").ok().and_then(|o| o.as_name().ok()) == Some(b"Sig".as_slice());
    match d.get(b"V") {
        Ok(Object::Reference(value)) if is_signature => Some(*value),
        _ => None,
    }
}

impl SignatureReport {
    /// A report for `field_name` that has not been checked yet.
    fn new(field_name: String) -> SignatureReport {
        SignatureReport {
            field_name,
            signer: None,
            claimed_name: None,
            sub_filter: None,
            reason: None,
            location: None,
            signing_time: None,
            validity: Validity::Unknown,
            detail: Detail::NotVerified,
            modified_after_signing: false,
            certifying: false,
            document_time_stamp: false,
            has_embedded_timestamp: false,
            embedded_timestamp_trusted: false,
            weak_algorithm: false,
            revision_end: 0,
        }
    }

    /// Takes what the signature dictionary `v` says about itself: its handler, type, and the
    /// signer's claims (name, reason, location, time).
    fn describe(&mut self, v: &Dictionary) {
        self.sub_filter = v.get(b"SubFilter").ok().and_then(|o| o.as_name().ok()).map(|n| String::from_utf8_lossy(n).into_owned());
        self.claimed_name = text_of(v, b"Name");
        self.reason = text_of(v, b"Reason");
        self.location = text_of(v, b"Location");
        self.signing_time = text_of(v, b"M");
        self.document_time_stamp = v.get(b"Type").ok().and_then(|o| o.as_name().ok()) == Some(b"DocTimeStamp".as_slice())
            || self.sub_filter.as_deref() == Some("ETSI.RFC3161");
    }

    /// The panel's status line.
    pub fn status_line(&self) -> &'static str {
        let (valid, unknown, invalid) = if self.document_time_stamp {
            (messages::TIMESTAMP_VALID, messages::TIMESTAMP_VALIDITY_UNKNOWN, messages::TIMESTAMP_INVALID)
        } else {
            (messages::VALID, messages::VALIDITY_UNKNOWN, messages::INVALID)
        };
        match (self.validity, self.detail) {
            (Validity::Blank, _) => messages::UNSIGNED_FIELD,
            (Validity::Valid, _) => valid,
            (Validity::ValidityUnknown, _) | (Validity::Unknown, _) => unknown,
            (Validity::Invalid, Detail::DigestOkUntrusted) => unknown,
            (Validity::Invalid, _) => invalid,
        }
    }

    /// The panel's description lines.
    pub fn description_lines(&self) -> Vec<&'static str> {
        let mut lines = Vec::new();
        match self.validity {
            Validity::Valid => self.integrity_lines(&mut lines),
            // Nothing was checked, so nothing is said about the signed version being unchanged.
            Validity::ValidityUnknown | Validity::Unknown if self.detail == Detail::UnsupportedAlgorithm => {
                if self.modified_after_signing {
                    lines.push(messages::SUBSEQUENT_CHANGES);
                }
                lines.push(messages::UNSUPPORTED_ALGORITHM);
            }
            Validity::ValidityUnknown => {
                self.integrity_lines(&mut lines);
                lines.push(self.identity_line());
            }
            Validity::Invalid => lines.push(match self.detail {
                Detail::BadByteRange => messages::BAD_BYTE_RANGE,
                Detail::Corrupt => messages::CORRUPTED,
                Detail::UnsupportedFilter => messages::FILTER_NOT_SUPPORTED,
                _ => messages::DOCUMENT_ALTERED,
            }),
            _ => {}
        }
        if self.weak_algorithm {
            lines.push(messages::WEAK_ALGORITHM);
        }
        if self.document_time_stamp {
            if self.validity == Validity::Valid {
                lines.push(messages::DOCUMENT_TIME_STAMPED);
            }
        } else if self.has_embedded_timestamp {
            lines.push(if self.embedded_timestamp_trusted { messages::TIME_STAMPED } else { messages::TIME_STAMP_UNCONFIRMED });
        }
        lines
    }

    /// Why a signer (or time stamp authority) whose signature held is not trusted.
    fn identity_line(&self) -> &'static str {
        match (self.detail, self.document_time_stamp) {
            (Detail::Expired, false) => messages::IDENTITY_EXPIRED,
            (Detail::Expired, true) => messages::TIMESTAMP_AUTHORITY_EXPIRED,
            (Detail::ChainRejected, false) => messages::CHAIN_REJECTED,
            (Detail::ChainRejected, true) => messages::TIMESTAMP_CHAIN_REJECTED,
            (Detail::NotPermitted, false) => messages::SIGNER_NOT_PERMITTED,
            (Detail::NotPermitted, true) => messages::TIMESTAMP_AUTHORITY_NOT_PERMITTED,
            (Detail::SignedInFuture, false) => messages::SIGNED_IN_FUTURE,
            (Detail::SignedInFuture, true) => messages::TIME_STAMP_IN_FUTURE,
            (_, false) => messages::IDENTITY_UNKNOWN,
            (_, true) => messages::TIMESTAMP_AUTHORITY_UNKNOWN,
        }
    }

    /// For a signature whose digest held: the signed version is unchanged, and whether the file
    /// goes on past it.
    fn integrity_lines(&self, lines: &mut Vec<&'static str>) {
        if self.modified_after_signing {
            lines.push(messages::REVISION_UNALTERED);
            lines.push(messages::SUBSEQUENT_CHANGES);
        } else {
            lines.push(messages::NOT_MODIFIED);
        }
    }
}

/// The notice bar text for a set of reports: the most serious condition wins.
pub fn notice_text(reports: &[SignatureReport]) -> Option<&'static str> {
    let mut signed: Vec<&SignatureReport> = reports.iter().filter(|r| r.validity != Validity::Blank).collect();
    if signed.is_empty() {
        return None;
    }
    signed.sort_by_key(|r| r.revision_end);
    if signed.iter().any(|r| r.validity == Validity::Unknown && r.detail == Detail::NotVerified) {
        return Some(messages::NOTICE_NOT_VERIFIED);
    }
    // Anything that is neither cleanly valid nor merely identity-unknown (a bad digest, a damaged
    // signature) is a problem.
    if signed.iter().any(|r| !matches!(r.validity, Validity::Valid | Validity::ValidityUnknown)) {
        return Some(messages::NOTICE_PROBLEMS);
    }
    if signed.iter().any(|r| r.detail == Detail::UnsupportedAlgorithm) {
        return Some(messages::NOTICE_NOT_CHECKED);
    }
    // A signature whose digest holds but whose signer is not confirmed must not be called valid,
    // and neither may a time stamp from an authority anyone could have made.
    if signed.iter().any(|r| r.validity == Validity::ValidityUnknown && !r.document_time_stamp) {
        return Some(messages::NOTICE_NOT_CONFIRMED);
    }
    if signed.iter().any(|r| r.validity == Validity::ValidityUnknown) {
        return Some(messages::NOTICE_TIMESTAMP_NOT_CONFIRMED);
    }
    if signed.last().is_some_and(|r| r.modified_after_signing) {
        return Some(messages::NOTICE_CHANGED_AFTER_LAST);
    }
    if signed.iter().any(|r| r.modified_after_signing) {
        return Some(messages::NOTICE_UPDATED);
    }
    Some(messages::NOTICE_ALL_VALID)
}

/// Does `child` carry a valid signature from `issuer`'s key?
fn issued_by(child: &Certificate, issuer: &Certificate) -> bool {
    if child.tbs_certificate.issuer != issuer.tbs_certificate.subject {
        return false;
    }
    let Ok(tbs) = child.tbs_certificate.to_der() else { return false };
    let Some(signature) = child.signature.as_bytes() else { return false };
    let parameters = child.signature_algorithm.parameters.as_ref().and_then(|p| p.to_der().ok());
    sigcrypto::verify(
        issuer,
        &child.signature_algorithm.oid.to_string(),
        parameters.as_deref(),
        "",
        &[&tbs],
        signature,
    ) == Verified::Ok
}

/// How many certificates a chain may have above the signer.
const MAX_CHAIN: usize = 8;

/// Builds and trusts a chain from `signer` up to a certificate in `store`, trying every candidate
/// issuer (a cross-signed intermediate has several). Each issuer must be a CA (basicConstraints
/// cA=true, keyUsage keyCertSign when present), satisfy its pathLenConstraint, and be valid at
/// `at`. A non-CA leaf can never act as an issuer, so one leaf cannot vouch for another. When no
/// chain qualifies, the error says whether one reaches the store by names and signatures alone
/// (`ChainRejected`) or none does (`DigestOkUntrusted`).
fn trust_level(
    signer: &Certificate,
    embedded: &[Certificate],
    store: &TrustStore,
    at: DateTime<Utc>,
) -> std::result::Result<Detail, Detail> {
    let in_store = |cert: &Certificate| cert.to_der().ok().is_some_and(|der| store.certificates.contains(&der));
    // A certificate the user trusts outright needs nothing further: this is the "trust one
    // person's certificate" flow, where that certificate may be an ordinary end-entity one.
    if in_store(signer) {
        return Ok(Detail::TrustedDirectly);
    }
    let trusted: Vec<Certificate> = store.certificates.iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
    let mut search = ChainSearch::new(signer, trusted.iter().chain(embedded), &in_store);
    if search.reaches_store(Some(at)) {
        Ok(Detail::TrustedViaChain)
    } else if search.reaches_store(None) {
        Err(Detail::ChainRejected)
    } else {
        Err(Detail::DigestOkUntrusted)
    }
}

/// A depth-first search for a path of issuers from the signer to a stored certificate.
struct ChainSearch<'a> {
    /// The signer first, then every other distinct certificate on offer.
    certificates: Vec<&'a Certificate>,
    stored: Vec<bool>,
    /// `issued_by` by (child, issuer) index, so each certificate signature is checked once.
    issued: HashMap<(usize, usize), bool>,
    /// (certificate, intermediates below its issuer) already searched without reaching the store.
    dead_ends: HashSet<(usize, usize)>,
}

impl<'a> ChainSearch<'a> {
    fn new(
        signer: &'a Certificate,
        others: impl Iterator<Item = &'a Certificate>,
        in_store: &dyn Fn(&Certificate) -> bool,
    ) -> Self {
        let mut certificates = vec![signer];
        let mut seen = vec![signer.to_der().unwrap_or_default()];
        for cert in others {
            let der = cert.to_der().unwrap_or_default();
            if !seen.contains(&der) {
                seen.push(der);
                certificates.push(cert);
            }
        }
        let stored = certificates.iter().map(|c| in_store(c)).collect();
        ChainSearch { certificates, stored, issued: HashMap::new(), dead_ends: HashSet::new() }
    }

    /// With `rules` (the time to check validity at), every issuer must also be allowed to issue at
    /// its place in the path; without, names and signatures alone count.
    fn reaches_store(&mut self, rules: Option<DateTime<Utc>>) -> bool {
        self.dead_ends.clear();
        self.search(0, &mut vec![0], rules)
    }

    fn search(&mut self, child: usize, path: &mut Vec<usize>, rules: Option<DateTime<Utc>>) -> bool {
        // The number of CA certificates between the leaf and the issuer considered.
        let intermediates = path.len() - 1;
        if intermediates >= MAX_CHAIN || self.dead_ends.contains(&(child, intermediates)) {
            return false;
        }
        for issuer in 0..self.certificates.len() {
            if path.contains(&issuer) || rules.is_some_and(|at| !can_issue(self.certificates[issuer], intermediates, at)) {
                continue;
            }
            let certificates = &self.certificates;
            if !*self.issued.entry((child, issuer)).or_insert_with(|| issued_by(certificates[child], certificates[issuer])) {
                continue;
            }
            if self.stored[issuer] {
                return true;
            }
            path.push(issuer);
            let found = self.search(issuer, path, rules);
            path.pop();
            if found {
                return true;
            }
        }
        self.dead_ends.insert((child, intermediates));
        false
    }
}

/// Whether `cert` may issue a certificate that has `intermediates` CA certificates below it, and is
/// valid at `at`: it must be a CA (pathLen permitting), allow key-certificate signing, and be in
/// its validity period.
fn can_issue(cert: &Certificate, intermediates: usize, at: DateTime<Utc>) -> bool {
    let Some(path_len) = basic_constraints_ca(cert) else { return false };
    if path_len.is_some_and(|limit| (limit as usize) < intermediates) {
        return false;
    }
    key_usage_allows(cert, |usage| usage.key_cert_sign()) && valid_at(cert, at)
}

/// `Some(pathLenConstraint)` when `cert` has basicConstraints with cA=true, else `None`.
fn basic_constraints_ca(cert: &Certificate) -> Option<Option<u32>> {
    let extensions = cert.tbs_certificate.extensions.as_ref()?;
    let extension = extensions.iter().find(|e| e.extn_id.to_string() == "2.5.29.19")?;
    let constraints = x509_cert::ext::pkix::BasicConstraints::from_der(extension.extn_value.as_bytes()).ok()?;
    constraints.ca.then(|| constraints.path_len_constraint.map(u32::from))
}

/// True when `cert` has no keyUsage extension, or one for which `permitted` holds.
fn key_usage_allows(cert: &Certificate, permitted: impl Fn(&x509_cert::ext::pkix::KeyUsage) -> bool) -> bool {
    let Some(extensions) = cert.tbs_certificate.extensions.as_ref() else { return true };
    let Some(extension) = extensions.iter().find(|e| e.extn_id.to_string() == "2.5.29.15") else { return true };
    match x509_cert::ext::pkix::KeyUsage::from_der(extension.extn_value.as_bytes()) {
        Ok(usage) => permitted(&usage),
        Err(_) => false,
    }
}

/// True when `at` falls within `cert`'s validity period.
fn valid_at(cert: &Certificate, at: DateTime<Utc>) -> bool {
    let validity = &cert.tbs_certificate.validity;
    let seconds = |t: x509_cert::time::Time| Utc.timestamp_opt(t.to_unix_duration().as_secs() as i64, 0).single();
    match (seconds(validity.not_before), seconds(validity.not_after)) {
        (Some(nb), Some(na)) => at >= nb && at <= na,
        _ => true,
    }
}

/// `/ByteRange [0 b c d]` as offsets into `file`: the first range starts the file, the gap between
/// the ranges is a hex string, and the second range ends inside the file.
fn signed_range(v: &Dictionary, file: &[u8]) -> Option<(usize, usize, usize)> {
    let range = integer_array(v.get(b"ByteRange").ok()).filter(|r| r.len() == 4)?;
    let [a, b, c, d] = [range[0], range[1], range[2], range[3]].map(|n| usize::try_from(n).ok());
    let (b, c, d) = (b?, c?, d?);
    let end = c.checked_add(d)?;
    let fits = a == Some(0) && b > 0 && c >= b + 2 && end <= file.len();
    (fits && file[b] == b'<' && file[c - 1] == b'>').then_some((b, c, d))
}

/// Where the value of `/Contents` lies in the dictionary object `id` as stored in `file`, read
/// from the bytes at the offset the cross-reference table gives for it.
fn signature_value_span(pdf: &Pdf, id: ObjectId, file: &[u8]) -> Option<(usize, usize)> {
    let XrefEntry::Normal { offset, generation } = pdf.doc.reference_table.get(id.0)? else { return None };
    if u32::from(*generation) != u32::from(id.1) {
        return None;
    }
    let mut lexer = RawLexer { file, at: *offset as usize };
    for expected in [id.0.to_string().as_bytes(), id.1.to_string().as_bytes(), b"obj"] {
        lexer.skip_space();
        if lexer.token()? != expected {
            return None;
        }
    }
    lexer.skip_space();
    if !file.get(lexer.at..)?.starts_with(b"<<") {
        return None;
    }
    lexer.at += 2;
    let mut span = None;
    loop {
        lexer.skip_space();
        if file.get(lexer.at..)?.starts_with(b">>") {
            return span;
        }
        let key = lexer.token()?;
        lexer.skip_space();
        let start = lexer.at;
        lexer.value()?;
        if key == b"/Contents" {
            let hex = file.get(start) == Some(&b'<') && file.get(start + 1) != Some(&b'<');
            span = hex.then_some((start, lexer.at));
        }
    }
}

/// The length of the `/Contents` string as the document was parsed.
fn parsed_contents_length(v: &Dictionary) -> Option<usize> {
    v.get(b"Contents").ok()?.as_str().ok().map(<[u8]>::len)
}

/// Hex digits only, an even number of them.
fn hex_value(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    text.chunks(2).map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?)).collect()
}

/// True when the revision ending at `end` names cross-reference data of its own in the signed
/// part: its last `startxref` lies after the gap and points outside it.
fn revision_is_self_contained(file: &[u8], b: usize, c: usize, end: usize) -> bool {
    let tail = &file[c..end];
    let Some(at) = tail.windows(9).rposition(|w| w == b"startxref") else { return false };
    let mut lexer = RawLexer { file: tail, at: at + 9 };
    lexer.skip_space();
    let offset = lexer.token().and_then(|t| std::str::from_utf8(t).ok()?.parse::<usize>().ok());
    offset.is_some_and(|x| x < b || (c..end).contains(&x))
}

/// Just enough of the PDF syntax to step over the values in a dictionary as written in the file.
struct RawLexer<'a> {
    file: &'a [u8],
    at: usize,
}

impl<'a> RawLexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.file.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b'%' => {
                    while self.peek().is_some_and(|b| b != b'\r' && b != b'\n') {
                        self.at += 1;
                    }
                }
                b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' => self.at += 1,
                _ => return,
            }
        }
    }

    /// A name (with its slash), number or keyword.
    fn token(&mut self) -> Option<&'a [u8]> {
        let start = self.at;
        if self.peek() == Some(b'/') {
            self.at += 1;
        }
        while self.peek().is_some_and(|b| !b"\0\t\n\x0c\r ()<>[]{}/%".contains(&b)) {
            self.at += 1;
        }
        (self.at > start).then(|| &self.file[start..self.at])
    }

    /// Steps over one value: a string, array, dictionary, or a token (a number followed by
    /// `G R` is a reference).
    fn value(&mut self) -> Option<()> {
        match self.peek()? {
            b'(' => {
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'\\' => self.at += 1,
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                self.at += 1;
                                return Some(());
                            }
                        }
                        _ => {}
                    }
                    self.at += 1;
                }
            }
            b'<' if self.file.get(self.at + 1) == Some(&b'<') => self.container(b">>", 2),
            b'<' => {
                self.at += self.file.get(self.at..)?.iter().position(|b| *b == b'>')? + 1;
                Some(())
            }
            b'[' => self.container(b"]", 1),
            _ => {
                let token = self.token()?;
                if token.iter().all(u8::is_ascii_digit) {
                    let back = self.at;
                    self.skip_space();
                    let generation = self.token();
                    self.skip_space();
                    let keyword = self.token();
                    if !(generation.is_some_and(|g| g.iter().all(u8::is_ascii_digit)) && keyword == Some(b"R".as_slice())) {
                        self.at = back;
                    }
                }
                Some(())
            }
        }
    }

    /// Steps over everything up to and including `close`, starting at an opening of `open_len` bytes.
    fn container(&mut self, close: &[u8], open_len: usize) -> Option<()> {
        self.at += open_len;
        loop {
            self.skip_space();
            if self.file.get(self.at..)?.starts_with(close) {
                self.at += close.len();
                return Some(());
            }
            self.value()?;
        }
    }
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
    /// Certificates are checked at `now`, or at the time of a time stamp from a trusted authority;
    /// never at the signing time a signature claims for itself.
    pub fn verify_signatures(file_bytes: &[u8], trust: &TrustStore, now: DateTime<Utc>) -> Result<Vec<SignatureReport>> {
        Self::verify_signatures_with_password(file_bytes, None, trust, now)
    }

    /// As [`Pdf::verify_signatures`], for a file that needs `password` (its open password or its
    /// owner password) to be read. The signed revisions are read with it too.
    pub fn verify_signatures_with_password(
        file_bytes: &[u8],
        password: Option<&str>,
        trust: &TrustStore,
        now: DateTime<Utc>,
    ) -> Result<Vec<SignatureReport>> {
        let pdf = match password {
            Some(password) => Pdf::from_bytes_with_password(file_bytes, password)?,
            None => Pdf::from_bytes(file_bytes)?,
        };
        let file_end = file_bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map(|p| p + 1).unwrap_or(0);

        // Field name per signature dictionary id.
        let mut names: HashMap<ObjectId, String> = HashMap::new();
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
            if !is_signature_dictionary(v) {
                continue;
            }
            reports.push(verify_one(&pdf, *id, v, names.get(id).cloned(), file_bytes, password, file_end, trust, now));
        }
        // A signature field whose value is gone or no longer reads as a signature (bytes written
        // into the gap broke its dictionary, say) is still a signature, and a damaged one.
        for field in pdf.doc.objects.values() {
            let Object::Dictionary(field) = field else { continue };
            let Some(value) = signature_field_value(field) else { continue };
            match pdf.doc.objects.get(&value) {
                Some(Object::Dictionary(v)) if is_signature_dictionary(v) => {}
                Some(Object::Dictionary(v)) if v.has(b"ByteRange") => {
                    reports.push(verify_one(&pdf, value, v, names.get(&value).cloned(), file_bytes, password, file_end, trust, now));
                }
                _ => {
                    let mut report = SignatureReport::new(text_of(field, b"T").unwrap_or_else(|| format!("object {}", value.0)));
                    report.validity = Validity::Invalid;
                    report.detail = Detail::Corrupt;
                    report.revision_end = file_end;
                    reports.push(report);
                }
            }
        }
        reports.sort_by_key(|r| r.revision_end);
        Ok(reports)
    }

    /// True when this document, loaded from `file_bytes` (with `password`, for a protected file),
    /// holds a signature that an update appended to the file would keep valid. A signature whose
    /// byte range no longer fits the file, or whose digest no longer matches it (the file was
    /// rewritten since), is already broken. One that cannot be checked here (an unsupported
    /// algorithm, say) counts as intact.
    pub(crate) fn has_intact_signatures(&self, file_bytes: &[u8], password: Option<&str>) -> bool {
        let file_end = file_bytes.iter().rposition(|b| !b.is_ascii_whitespace()).map(|p| p + 1).unwrap_or(0);
        let trust = TrustStore::default();
        self.doc.objects.iter().any(|(id, object)| {
            let Object::Dictionary(v) = object else { return false };
            if !is_signature_dictionary(v) {
                return false;
            }
            let report = verify_one(self, *id, v, None, file_bytes, password, file_end, &trust, Utc::now());
            !(report.validity == Validity::Invalid && matches!(report.detail, Detail::BadByteRange | Detail::DigestMismatch))
        })
    }
}

/// Checks the signature dictionary `v` (object `id` of `pdf`, loaded from `file` with `password`).
#[allow(clippy::too_many_arguments)]
fn verify_one(
    pdf: &Pdf,
    id: ObjectId,
    v: &Dictionary,
    name: Option<String>,
    file: &[u8],
    password: Option<&str>,
    file_end: usize,
    trust: &TrustStore,
    now: DateTime<Utc>,
) -> SignatureReport {
    let mut report = SignatureReport::new(name.unwrap_or_else(|| format!("object {}", id.0)));
    report.describe(v);

    let invalid = |report: &mut SignatureReport, detail: Detail| {
        report.validity = Validity::Invalid;
        report.detail = detail;
    };

    // 1. Sub-filter.
    const SUPPORTED: [&str; 3] = ["adbe.pkcs7.detached", "ETSI.CAdES.detached", "ETSI.RFC3161"];
    let supported = |report: &SignatureReport| report.sub_filter.as_deref().is_some_and(|f| SUPPORTED.contains(&f));
    if !supported(&report) {
        invalid(&mut report, Detail::UnsupportedFilter);
        return report;
    }

    // 2. The byte range, bound to the signature value: the gap holds exactly this dictionary's
    //    /Contents, the dictionary lies in the signed part, and so does the revision's own
    //    cross-reference data.
    let Some((b, c, d)) = signed_range(v, file) else {
        invalid(&mut report, Detail::BadByteRange);
        return report;
    };
    report.revision_end = c + d;
    report.modified_after_signing = report.revision_end < file_end;
    if !revision_is_self_contained(file, b, c, c + d) {
        invalid(&mut report, Detail::BadByteRange);
        return report;
    }
    // The dictionary itself is read again from the signed revision alone. A later update can file
    // another copy under the same object number (an entry whose bytes say "N 0 obj"), and that
    // copy, outside the signed bytes, must not supply the date, reason or names.
    let reloaded;
    let revision = if report.modified_after_signing {
        let signed_part = &file[..c + d];
        match password.map_or_else(|| Pdf::from_bytes(signed_part), |p| Pdf::from_bytes_with_password(signed_part, p)) {
            Ok(pdf) => {
                reloaded = pdf;
                &reloaded
            }
            Err(_) => {
                invalid(&mut report, Detail::BadByteRange);
                return report;
            }
        }
    } else {
        pdf
    };
    let signed = match revision.doc.objects.get(&id) {
        Some(Object::Dictionary(signed)) if signed_range(signed, file) == Some((b, c, d)) => signed,
        _ => {
            invalid(&mut report, Detail::BadByteRange);
            return report;
        }
    };
    let bound = signature_value_span(revision, id, file) == Some((b, c))
        && (revision.doc.encryption_state.is_some()
            || parsed_contents_length(signed).is_some_and(|len| 2 * len + 2 == c - b));
    let Some(contents) = bound.then(|| hex_value(&file[b + 1..c - 1])).flatten() else {
        invalid(&mut report, Detail::BadByteRange);
        return report;
    };
    // From here on, what the report says about the signature comes from the signed dictionary.
    report.describe(signed);
    report.certifying = revision.is_certifying_signature(id);
    if !supported(&report) {
        invalid(&mut report, Detail::UnsupportedFilter);
        return report;
    }

    // 3. Parse the CMS (or time-stamp token). Only zero padding may follow it.
    if der::read_ber(&contents).is_none_or(|(_, rest)| rest.iter().any(|byte| *byte != 0)) {
        invalid(&mut report, Detail::Corrupt);
        return report;
    }

    // A document time stamp is an RFC 3161 token, not an ordinary signature.
    if report.document_time_stamp {
        return verify_document_timestamp(report, &contents, file, b, c, d, trust, now);
    }

    let Some(cms) = cms::parse_signed_data(&contents) else {
        invalid(&mut report, Detail::Corrupt);
        return report;
    };
    let signer_info = &cms.signer;

    let unknown = |report: &mut SignatureReport, detail: Detail| {
        report.validity = Validity::ValidityUnknown;
        report.detail = detail;
    };

    // Signer certificate: match issuer and serial, or subject key identifier, else the first
    // embedded certificate.
    let certificates: Vec<Certificate> = cms.certificates.iter().filter_map(|d| Certificate::from_der(d).ok()).collect();
    let Some(signer) = find_signer(&certificates, &signer_info.id) else {
        invalid(&mut report, Detail::Corrupt);
        return report;
    };
    report.signer = Some(subject_name(signer));
    if report.claimed_name.as_deref() == Some(report.signer.as_deref().unwrap_or_default()) {
        report.claimed_name = None;
    }

    let digest_oid = &signer_info.digest_algorithm;
    report.weak_algorithm = sigcrypto::is_weak_digest(digest_oid)
        || sigcrypto::uses_sha1(&signer_info.signature_algorithm, signer_info.signature_parameters.as_deref(), digest_oid);

    // 4. Integrity: message digest and signature value. The signed input is the DER SET OF signed
    //    attributes, or, when there are none, the covered bytes directly.
    let covered: [&[u8]; 2] = [&file[..b], &file[c..c + d]];
    let inputs: Vec<Vec<u8>> = match &signer_info.signed_attributes {
        Some(_) => {
            let Some(content_digest) = sigcrypto::digest(digest_oid, &covered) else {
                unknown(&mut report, Detail::UnsupportedAlgorithm);
                return report;
            };
            if signer_info.message_digest.as_deref() != Some(content_digest.as_slice()) {
                invalid(&mut report, Detail::DigestMismatch);
                return report;
            }
            signer_info.signed_attribute_encodings()
        }
        None => vec![covered.concat()],
    };
    let mut outcome = Verified::Bad;
    for input in &inputs {
        outcome = sigcrypto::verify(
            signer,
            &signer_info.signature_algorithm,
            signer_info.signature_parameters.as_deref(),
            digest_oid,
            &[input],
            &signer_info.signature,
        );
        if outcome != Verified::Bad {
            break;
        }
    }
    match outcome {
        Verified::Ok => {}
        Verified::Unsupported => {
            unknown(&mut report, Detail::UnsupportedAlgorithm);
            return report;
        }
        // No signed attributes means no message digest to compare against, so a failing signature
        // is the only sign the covered bytes changed.
        Verified::Bad if signer_info.signed_attributes.is_none() => {
            invalid(&mut report, Detail::DigestMismatch);
            return report;
        }
        Verified::Bad => {
            invalid(&mut report, Detail::Corrupt);
            return report;
        }
    }

    // An embedded signature time stamp counts only when it is present and itself verifies: its
    // message imprint is over the signature value, and its own token signature holds. Only the
    // time of one whose authority is trusted shows when the signature existed: anyone can add an
    // unsigned attribute, with any time in it.
    let embedded = cms::unsigned_attribute(signer_info, cms::OID_SIGNATURE_TIMESTAMP)
        .map(|token| check_timestamp_token(token, &[&signer_info.signature], trust, now));
    let mut timestamp_time = None;
    if let Some(TokenResult::Verified { time, standing, .. }) = embedded {
        report.has_embedded_timestamp = true;
        report.embedded_timestamp_trusted = is_trusted(standing);
        timestamp_time = report.embedded_timestamp_trusted.then_some(time);
    }

    // 5. Trust and validity period, at the time a trusted time stamp gives, otherwise now. The
    //    signing time in /M is whatever the signer chose to write: a key that has expired could
    //    otherwise sign today with a date from when it was valid. A /M in the future is still
    //    named, as nothing that claims it can be taken at its word.
    let claimed_time = report.signing_time.as_deref().and_then(parse_pdf_date);
    let reference_time = timestamp_time.unwrap_or(now);
    let future = |time: DateTime<Utc>| time > now + chrono::Duration::minutes(5);
    if future(reference_time) || (timestamp_time.is_none() && claimed_time.is_some_and(future)) {
        unknown(&mut report, Detail::SignedInFuture);
        return report;
    }
    if !valid_at(signer, reference_time) {
        unknown(&mut report, Detail::Expired);
        return report;
    }
    // The signer certificate, when it declares a key usage, must allow signing.
    if !key_usage_allows(signer, |usage| usage.digital_signature() || usage.non_repudiation()) {
        unknown(&mut report, Detail::NotPermitted);
        return report;
    }
    match trust_level(signer, &certificates, trust, reference_time) {
        Ok(level) => {
            report.validity = Validity::Valid;
            report.detail = level;
        }
        Err(detail) => unknown(&mut report, detail),
    }
    report
}

#[allow(clippy::too_many_arguments)]
fn verify_document_timestamp(
    mut report: SignatureReport,
    token: &[u8],
    file: &[u8],
    b: usize,
    c: usize,
    d: usize,
    trust: &TrustStore,
    now: DateTime<Utc>,
) -> SignatureReport {
    let covered: [&[u8]; 2] = [&file[..b], &file[c..c + d]];
    let (validity, detail) = match check_timestamp_token(token, &covered, trust, now) {
        TokenResult::NotAToken | TokenResult::BadSignature => (Validity::Invalid, Detail::Corrupt),
        TokenResult::ImprintMismatch => (Validity::Invalid, Detail::DigestMismatch),
        TokenResult::Unsupported => (Validity::ValidityUnknown, Detail::UnsupportedAlgorithm),
        TokenResult::Verified { time, signer, standing } => {
            report.signer = signer;
            report.signing_time = Some(time.format("D:%Y%m%d%H%M%SZ").to_string());
            (if is_trusted(standing) { Validity::Valid } else { Validity::ValidityUnknown }, standing)
        }
    };
    report.validity = validity;
    report.detail = detail;
    report
}

/// True for the details that mean a certificate is trusted.
fn is_trusted(detail: Detail) -> bool {
    matches!(detail, Detail::TrustedDirectly | Detail::TrustedViaChain)
}

/// The outcome of checking an RFC 3161 time-stamp token.
enum TokenResult {
    /// Not a parseable RFC 3161 token (used where one is required).
    NotAToken,
    /// The token's message imprint does not cover the expected data.
    ImprintMismatch,
    /// The token's own signature does not hold.
    BadSignature,
    /// The token uses an algorithm this build cannot check.
    Unsupported,
    /// The token verifies. `standing` is its authority's: `TrustedDirectly` or `TrustedViaChain`
    /// when trusted, else why not.
    Verified { time: DateTime<Utc>, signer: Option<String>, standing: Detail },
}

/// Verifies an RFC 3161 time-stamp token: its message imprint must cover `imprint` and its own
/// signature must hold. For a trusted result its authority certificate must also be valid at the
/// token's time (which may not be in the future), be for time stamping alone, and chain to a
/// trusted certificate.
fn check_timestamp_token(token: &[u8], imprint: &[&[u8]], trust: &TrustStore, now: DateTime<Utc>) -> TokenResult {
    let Some(cms) = cms::parse_signed_data(token) else { return TokenResult::NotAToken };
    if cms.content_type != cms::OID_TST_INFO {
        return TokenResult::NotAToken;
    }
    let Some(econtent) = cms.content.as_deref() else { return TokenResult::NotAToken };
    let Some(info) = cms::parse_tst_info(econtent) else { return TokenResult::NotAToken };
    let Some(time) = gentime_to_utc(&info.gen_time) else { return TokenResult::NotAToken };

    // The message imprint must hash exactly the data this token is supposed to cover.
    match sigcrypto::digest(&info.hash_algorithm, imprint) {
        Some(expected) if expected == info.hashed_message => {}
        Some(_) => return TokenResult::ImprintMismatch,
        None => return TokenResult::Unsupported,
    }

    // The token's own signature, over its signed attributes with messageDigest = H(TSTInfo).
    match timestamp_signature(&cms, econtent) {
        Verified::Ok => {}
        Verified::Bad => return TokenResult::BadSignature,
        Verified::Unsupported => return TokenResult::Unsupported,
    }

    let certificates: Vec<Certificate> = cms.certificates.iter().filter_map(|c| Certificate::from_der(c).ok()).collect();
    let Some(signer) = find_signer(&certificates, &cms.signer.id) else { return TokenResult::BadSignature };
    let standing = if time > now + chrono::Duration::minutes(5) {
        Detail::SignedInFuture
    } else if !valid_at(signer, time) {
        Detail::Expired
    } else if !(has_timestamping_eku(signer) && key_usage_allows(signer, |u| u.digital_signature() || u.non_repudiation())) {
        Detail::NotPermitted
    } else {
        trust_level(signer, &certificates, trust, time).unwrap_or_else(|detail| detail)
    };
    TokenResult::Verified { time, signer: Some(subject_name(signer)), standing }
}

/// Finds the certificate a SignerInfo names, by issuer and serial or by subject key identifier.
fn find_signer<'a>(certificates: &'a [Certificate], id: &cms::SignerId) -> Option<&'a Certificate> {
    certificates
        .iter()
        .find(|c| match id {
            cms::SignerId::IssuerAndSerial { issuer, serial } => {
                c.tbs_certificate.issuer.to_der().ok().as_ref() == Some(issuer)
                    && c.tbs_certificate.serial_number.to_der().ok().as_ref() == Some(serial)
            }
            cms::SignerId::KeyIdentifier(key_id) => subject_key_id(c).as_deref() == Some(key_id),
        })
        .or_else(|| certificates.first())
}

/// True when `certificate` is for time stamping as RFC 3161 asks: a critical extended key usage
/// whose only purpose is id-kp-timeStamping.
fn has_timestamping_eku(certificate: &Certificate) -> bool {
    let Some(extensions) = certificate.tbs_certificate.extensions.as_ref() else { return false };
    let Some(extension) = extensions.iter().find(|e| e.extn_id.to_string() == "2.5.29.37") else { return false };
    let Ok(usage) = x509_cert::ext::pkix::ExtendedKeyUsage::from_der(extension.extn_value.as_bytes()) else {
        return false;
    };
    extension.critical && usage.0.len() == 1 && usage.0[0].to_string() == "1.3.6.1.5.5.7.3.8"
}

/// A GeneralizedTime (`YYYYMMDDHHMMSS[.fff]Z`) as a UTC instant.
fn gentime_to_utc(text: &str) -> Option<DateTime<Utc>> {
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 14 {
        return None;
    }
    let field = |from: usize, len: usize| digits[from..from + len].parse::<u32>().ok();
    let year = field(0, 4)? as i32;
    let (month, day) = (field(4, 2)?, field(6, 2)?);
    let (hour, minute, second) = (field(8, 2)?, field(10, 2)?, field(12, 2)?);
    Utc.with_ymd_and_hms(year, month, day, hour, minute, second).single()
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
