//! The public-key algorithms a signature or certificate can use: RSA PKCS#1 v1.5, RSA-PSS and
//! ECDSA on P-256 and P-384, with SHA-1, SHA-256, SHA-384 or SHA-512 digests.

use p256::ecdsa::signature::hazmat::PrehashVerifier;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Sign, Pss, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use x509_cert::der::Encode;
use x509_cert::Certificate;

use crate::cms;
use crate::der;

// Digest OIDs.
const SHA1: &str = "1.3.14.3.2.26";
const SHA256: &str = "2.16.840.1.101.3.4.2.1";
const SHA384: &str = "2.16.840.1.101.3.4.2.2";
const SHA512: &str = "2.16.840.1.101.3.4.2.3";

// Signature-algorithm OIDs.
const RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
const RSA_PSS: &str = "1.2.840.113549.1.1.10";
const SHA1_RSA: &str = "1.2.840.113549.1.1.5";
const SHA256_RSA: &str = "1.2.840.113549.1.1.11";
const SHA384_RSA: &str = "1.2.840.113549.1.1.12";
const SHA512_RSA: &str = "1.2.840.113549.1.1.13";
const ECDSA_SHA1: &str = "1.2.840.10045.4.1";
const ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
const ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
const ECDSA_SHA512: &str = "1.2.840.10045.4.3.4";

/// The result of checking a signature value against the key that should have produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verified {
    /// The signature is good.
    Ok,
    /// The key and algorithm are supported, but the signature did not verify.
    Bad,
    /// The algorithm or key type is one this build cannot check.
    Unsupported,
}

/// True when the signature algorithm `oid` hashes with SHA-1.
pub(crate) fn uses_sha1(oid: &str) -> bool {
    matches!(oid, SHA1_RSA | ECDSA_SHA1)
}

/// True when `oid` names SHA-1, which is reported with a weak-algorithm note.
pub(crate) fn is_weak_digest(oid: &str) -> bool {
    oid == SHA1
}

/// The digest `oid` over the concatenation of `parts`, or `None` for an unknown digest.
pub(crate) fn digest(oid: &str, parts: &[&[u8]]) -> Option<Vec<u8>> {
    fn run<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
        let mut hasher = D::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize().to_vec()
    }
    match oid {
        SHA1 => Some(run::<Sha1>(parts)),
        SHA256 => Some(run::<Sha256>(parts)),
        SHA384 => Some(run::<Sha384>(parts)),
        SHA512 => Some(run::<Sha512>(parts)),
        _ => None,
    }
}

/// Verifies `signature` over `parts` with `certificate`'s public key, using `signature_algorithm`
/// (and its DER `parameters`, for PSS). `digest_algorithm` is the SignerInfo's digest, used by the
/// plain `rsaEncryption` OID that does not name one itself.
pub(crate) fn verify(
    certificate: &Certificate,
    signature_algorithm: &str,
    parameters: Option<&[u8]>,
    digest_algorithm: &str,
    parts: &[&[u8]],
    signature: &[u8],
) -> Verified {
    let Ok(spki) = certificate.tbs_certificate.subject_public_key_info.to_der() else {
        return Verified::Unsupported;
    };
    match signature_algorithm {
        RSA_ENCRYPTION | SHA1_RSA | SHA256_RSA | SHA384_RSA | SHA512_RSA => {
            let hash = rsa_hash(signature_algorithm).unwrap_or(digest_algorithm);
            rsa_pkcs1(&spki, hash, parts, signature)
        }
        RSA_PSS => rsa_pss(&spki, parameters, parts, signature),
        ECDSA_SHA1 => ecdsa(&spki, SHA1, parts, signature),
        ECDSA_SHA256 => ecdsa(&spki, SHA256, parts, signature),
        ECDSA_SHA384 => ecdsa(&spki, SHA384, parts, signature),
        ECDSA_SHA512 => ecdsa(&spki, SHA512, parts, signature),
        _ => Verified::Unsupported,
    }
}

/// The digest a `shaNWithRSAEncryption` OID names; `None` for plain `rsaEncryption`.
fn rsa_hash(oid: &str) -> Option<&'static str> {
    match oid {
        SHA1_RSA => Some(SHA1),
        SHA256_RSA => Some(SHA256),
        SHA384_RSA => Some(SHA384),
        SHA512_RSA => Some(SHA512),
        _ => None,
    }
}

fn rsa_pkcs1(spki: &[u8], hash: &str, parts: &[&[u8]], signature: &[u8]) -> Verified {
    let Ok(key) = RsaPublicKey::from_public_key_der(spki) else { return Verified::Unsupported };
    let (scheme, Some(hashed)) = (match hash {
        SHA1 => Pkcs1v15Sign::new::<Sha1>(),
        SHA256 => Pkcs1v15Sign::new::<Sha256>(),
        SHA384 => Pkcs1v15Sign::new::<Sha384>(),
        SHA512 => Pkcs1v15Sign::new::<Sha512>(),
        _ => return Verified::Unsupported,
    }, digest(hash, parts)) else {
        return Verified::Unsupported;
    };
    bool_result(key.verify(scheme, &hashed, signature).is_ok())
}

fn rsa_pss(spki: &[u8], parameters: Option<&[u8]>, parts: &[&[u8]], signature: &[u8]) -> Verified {
    let Ok(key) = RsaPublicKey::from_public_key_der(spki) else { return Verified::Unsupported };
    let (hash, salt_len) = pss_parameters(parameters);
    let Some(hashed) = digest(hash, parts) else { return Verified::Unsupported };
    let scheme = match hash {
        SHA1 => Pss::new_with_salt::<Sha1>(salt_len),
        SHA256 => Pss::new_with_salt::<Sha256>(salt_len),
        SHA384 => Pss::new_with_salt::<Sha384>(salt_len),
        SHA512 => Pss::new_with_salt::<Sha512>(salt_len),
        _ => return Verified::Unsupported,
    };
    bool_result(key.verify(scheme, &hashed, signature).is_ok())
}

/// `RSASSA-PSS-params`: the hash algorithm (default SHA-1) and salt length (default 20).
fn pss_parameters(parameters: Option<&[u8]>) -> (&'static str, usize) {
    let mut hash = SHA1;
    let mut salt_len = 20;
    if let Some((params, _)) = parameters.and_then(der::read_ber) {
        for field in der::children_ber(params.content) {
            match field.tag {
                0xA0 => {
                    if let Some((oid, _)) = der::children_ber(field.content).first().and_then(cms::algorithm) {
                        hash = canonical_hash(&oid);
                    }
                }
                0xA2 => {
                    if let Some(len) = der::children_ber(field.content).first().and_then(|e| der::small_unsigned(e.content)) {
                        salt_len = len as usize;
                    }
                }
                _ => {}
            }
        }
    }
    (hash, salt_len)
}

fn canonical_hash(oid: &str) -> &'static str {
    match oid {
        SHA256 => SHA256,
        SHA384 => SHA384,
        SHA512 => SHA512,
        _ => SHA1,
    }
}

fn ecdsa(spki: &[u8], hash: &str, parts: &[&[u8]], signature: &[u8]) -> Verified {
    let Some(hashed) = digest(hash, parts) else { return Verified::Unsupported };
    if let Ok(key) = p256::ecdsa::VerifyingKey::from_public_key_der(spki) {
        let Ok(sig) = p256::ecdsa::Signature::from_der(signature) else { return Verified::Bad };
        return bool_result(key.verify_prehash(&hashed, &sig).is_ok());
    }
    if let Ok(key) = p384::ecdsa::VerifyingKey::from_public_key_der(spki) {
        let Ok(sig) = p384::ecdsa::Signature::from_der(signature) else { return Verified::Bad };
        return bool_result(key.verify_prehash(&hashed, &sig).is_ok());
    }
    Verified::Unsupported
}

fn bool_result(ok: bool) -> Verified {
    if ok {
        Verified::Ok
    } else {
        Verified::Bad
    }
}
