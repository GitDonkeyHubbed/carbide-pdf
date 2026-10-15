//! The public-key algorithms a signature or certificate can use: RSA PKCS#1 v1.5, RSA-PSS and
//! ECDSA on P-256 and P-384, with SHA-1, SHA-256, SHA-384 or SHA-512 digests.

use p256::ecdsa::signature::hazmat::PrehashVerifier;
use rsa::pkcs1::DecodeRsaPublicKey;
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
const MGF1: &str = "1.2.840.113549.1.1.8";
const SHA1_RSA: &str = "1.2.840.113549.1.1.5";
const SHA256_RSA: &str = "1.2.840.113549.1.1.11";
const SHA384_RSA: &str = "1.2.840.113549.1.1.12";
const SHA512_RSA: &str = "1.2.840.113549.1.1.13";
const EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
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

/// True when the signature algorithm hashes with SHA-1, including RSA-PSS whose parameters name
/// SHA-1 or leave the hash at its SHA-1 default. `digest_algorithm` is the SignerInfo's digest,
/// which the plain key OIDs (`rsaEncryption`, `id-ecPublicKey`) sign with.
pub(crate) fn uses_sha1(
    signature_algorithm: &str,
    parameters: Option<&[u8]>,
    digest_algorithm: &str,
) -> bool {
    signature_hash(signature_algorithm, parameters, digest_algorithm) == Some(SHA1)
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

/// The digest a signature algorithm computes over what it signs, when this build knows it.
fn signature_hash(
    signature_algorithm: &str,
    parameters: Option<&[u8]>,
    digest_algorithm: &str,
) -> Option<&'static str> {
    match signature_algorithm {
        RSA_ENCRYPTION | EC_PUBLIC_KEY => known_hash(digest_algorithm),
        SHA1_RSA | ECDSA_SHA1 => Some(SHA1),
        SHA256_RSA | ECDSA_SHA256 => Some(SHA256),
        SHA384_RSA | ECDSA_SHA384 => Some(SHA384),
        SHA512_RSA | ECDSA_SHA512 => Some(SHA512),
        RSA_PSS => pss_parameters(parameters).map(|(hash, _)| hash),
        _ => None,
    }
}

/// Verifies `signature` over `parts` with `certificate`'s public key, using `signature_algorithm`
/// (and its DER `parameters`, for PSS). `digest_algorithm` is the SignerInfo's digest, used by the
/// plain `rsaEncryption` and `id-ecPublicKey` OIDs that do not name one themselves.
pub(crate) fn verify(
    certificate: &Certificate,
    signature_algorithm: &str,
    parameters: Option<&[u8]>,
    digest_algorithm: &str,
    parts: &[&[u8]],
    signature: &[u8],
) -> Verified {
    let Some(hash) = signature_hash(signature_algorithm, parameters, digest_algorithm) else {
        return Verified::Unsupported;
    };
    match signature_algorithm {
        RSA_ENCRYPTION | SHA1_RSA | SHA256_RSA | SHA384_RSA | SHA512_RSA => {
            rsa_pkcs1(certificate, hash, parts, signature)
        }
        RSA_PSS => {
            let salt_len = pss_parameters(parameters).map_or(20, |(_, salt)| salt);
            rsa_pss(certificate, hash, salt_len, parts, signature)
        }
        _ => ecdsa(certificate, hash, parts, signature),
    }
}

/// The certificate's RSA public key. An RSASSA-PSS key (RFC 4055) carries the same RSAPublicKey
/// under its own algorithm OID, and is only accepted where `pss` allows it.
fn rsa_public_key(certificate: &Certificate, pss: bool) -> Option<RsaPublicKey> {
    let spki = &certificate.tbs_certificate.subject_public_key_info;
    match spki.algorithm.oid.to_string().as_str() {
        RSA_ENCRYPTION => RsaPublicKey::from_public_key_der(&spki.to_der().ok()?).ok(),
        RSA_PSS if pss => RsaPublicKey::from_pkcs1_der(spki.subject_public_key.as_bytes()?).ok(),
        _ => None,
    }
}

fn rsa_pkcs1(certificate: &Certificate, hash: &str, parts: &[&[u8]], signature: &[u8]) -> Verified {
    let Some(key) = rsa_public_key(certificate, false) else {
        return Verified::Unsupported;
    };
    let (scheme, Some(hashed)) = (
        match hash {
            SHA1 => Pkcs1v15Sign::new::<Sha1>(),
            SHA256 => Pkcs1v15Sign::new::<Sha256>(),
            SHA384 => Pkcs1v15Sign::new::<Sha384>(),
            SHA512 => Pkcs1v15Sign::new::<Sha512>(),
            _ => return Verified::Unsupported,
        },
        digest(hash, parts),
    ) else {
        return Verified::Unsupported;
    };
    bool_result(key.verify(scheme, &hashed, signature).is_ok())
}

fn rsa_pss(
    certificate: &Certificate,
    hash: &str,
    salt_len: usize,
    parts: &[&[u8]],
    signature: &[u8],
) -> Verified {
    let Some(key) = rsa_public_key(certificate, true) else {
        return Verified::Unsupported;
    };
    match key_pss_restrictions(certificate) {
        None => return Verified::Unsupported,
        Some(Some((key_hash, least_salt))) if key_hash != hash || salt_len < least_salt => {
            return Verified::Bad;
        }
        Some(_) => {}
    }
    let Some(hashed) = digest(hash, parts) else {
        return Verified::Unsupported;
    };
    let scheme = match hash {
        SHA1 => Pss::new_with_salt::<Sha1>(salt_len),
        SHA256 => Pss::new_with_salt::<Sha256>(salt_len),
        SHA384 => Pss::new_with_salt::<Sha384>(salt_len),
        SHA512 => Pss::new_with_salt::<Sha512>(salt_len),
        _ => return Verified::Unsupported,
    };
    bool_result(key.verify(scheme, &hashed, signature).is_ok())
}

/// What an RSASSA-PSS key's own parameters allow (RFC 4055, 3.1): the one hash, for the message
/// and for MGF1, its signatures may use, and the least salt length. `Some(None)` for a key that
/// sets none (or an rsaEncryption key), `None` for parameters this build cannot read.
fn key_pss_restrictions(certificate: &Certificate) -> Option<Option<(&'static str, usize)>> {
    let algorithm = &certificate
        .tbs_certificate
        .subject_public_key_info
        .algorithm;
    if algorithm.oid.to_string() != RSA_PSS {
        return Some(None);
    }
    let Some(parameters) = algorithm.parameters.as_ref() else {
        return Some(None);
    };
    let parameters = parameters.to_der().ok()?;
    if parameters == der::null() {
        return Some(None);
    }
    pss_parameters(Some(&parameters)).map(Some)
}

/// `RSASSA-PSS-params` this build can check: the hash (default SHA-1) and salt length (default
/// 20). `None` for a hash it does not know, a mask generation other than MGF1 over that same hash
/// (the RSA code uses one hash for both), or a trailer field other than 1.
fn pss_parameters(parameters: Option<&[u8]>) -> Option<(&'static str, usize)> {
    let (mut hash, mut mask_hash, mut salt_len) = (SHA1, SHA1, 20);
    if let Some(bytes) = parameters {
        let (params, _) = der::read_ber(bytes).filter(|(p, _)| p.tag == der::SEQUENCE)?;
        for field in der::children_ber(params.content) {
            let inner = der::children_ber(field.content);
            let value = inner.first()?;
            match field.tag {
                0xA0 => hash = known_hash(&cms::algorithm(value)?.0)?,
                0xA1 => {
                    let (mask, mask_params) = cms::algorithm(value)?;
                    let (mask_params, _) = der::read_ber(mask_params.as_deref()?)?;
                    if mask != MGF1 {
                        return None;
                    }
                    mask_hash = known_hash(&cms::algorithm(&mask_params)?.0)?;
                }
                0xA2 => salt_len = usize::try_from(der::small_unsigned(value.content)?).ok()?,
                0xA3 if der::small_unsigned(value.content)? == 1 => {}
                _ => return None,
            }
        }
    }
    (hash == mask_hash).then_some((hash, salt_len))
}

/// `oid` when it names a digest this build computes.
fn known_hash(oid: &str) -> Option<&'static str> {
    [SHA1, SHA256, SHA384, SHA512]
        .into_iter()
        .find(|known| *known == oid)
}

fn ecdsa(certificate: &Certificate, hash: &str, parts: &[&[u8]], signature: &[u8]) -> Verified {
    let Ok(spki) = certificate.tbs_certificate.subject_public_key_info.to_der() else {
        return Verified::Unsupported;
    };
    let Some(mut hashed) = digest(hash, parts) else {
        return Verified::Unsupported;
    };
    // A digest shorter than half the curve's field (SHA-1 on P-384) is refused by the curve code;
    // zeros on the left give the same integer, which is what ECDSA signs.
    if hashed.len() < 24 {
        hashed.splice(0..0, std::iter::repeat_n(0, 24 - hashed.len()));
    }
    if let Ok(key) = p256::ecdsa::VerifyingKey::from_public_key_der(&spki) {
        let Ok(sig) = p256::ecdsa::Signature::from_der(signature) else {
            return Verified::Bad;
        };
        return bool_result(key.verify_prehash(&hashed, &sig).is_ok());
    }
    if let Ok(key) = p384::ecdsa::VerifyingKey::from_public_key_der(&spki) {
        let Ok(sig) = p384::ecdsa::Signature::from_der(signature) else {
            return Verified::Bad;
        };
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
