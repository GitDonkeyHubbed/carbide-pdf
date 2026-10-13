//! Reading CMS SignedData (RFC 5652), the structure inside PDF signatures and RFC 3161 time-stamp
//! tokens.
//!
//! Other signers often write it in BER, with indefinite lengths, so it is walked with the BER
//! reader. The signed attributes are hashed in DER, as the standard requires, whatever form they
//! arrived in.

use crate::der::{self, Element};

pub(crate) const OID_SIGNED_DATA: &str = "1.2.840.113549.1.7.2";
const OID_CONTENT_TYPE: &str = "1.2.840.113549.1.9.3";
const OID_MESSAGE_DIGEST: &str = "1.2.840.113549.1.9.4";
const OID_SIGNING_CERTIFICATE: &str = "1.2.840.113549.1.9.16.2.12";
const OID_SIGNING_CERTIFICATE_V2: &str = "1.2.840.113549.1.9.16.2.47";
const OID_SHA1: &str = "1.3.14.3.2.26";
const OID_SHA256: &str = "2.16.840.1.101.3.4.2.1";

#[derive(Debug)]
pub(crate) struct SignedData {
    /// DER certificates from the `certificates` field.
    pub certificates: Vec<Vec<u8>>,
    /// The type of the encapsulated content.
    pub content_type: String,
    /// The encapsulated content itself, unless the signature is detached.
    pub content: Option<Vec<u8>>,
    /// The first signer.
    pub signer: SignerInfo,
}

#[derive(Debug)]
pub(crate) enum SignerId {
    /// DER of the issuer's name and of the serial number INTEGER.
    IssuerAndSerial { issuer: Vec<u8>, serial: Vec<u8> },
    /// The signer certificate's subject key identifier.
    KeyIdentifier(Vec<u8>),
}

#[derive(Debug)]
pub(crate) struct SignerInfo {
    pub id: SignerId,
    pub digest_algorithm: String,
    /// Each signed attribute in DER, in the order they were sent.
    pub signed_attributes: Option<Vec<Vec<u8>>>,
    pub message_digest: Option<Vec<u8>>,
    /// The signed contentType attribute.
    pub content_type: Option<String>,
    /// The signed ESS signing-certificate attribute (v1 or v2): the hash algorithm and the hash of
    /// the certificate the signer says it used.
    pub signing_certificate: Option<(String, Vec<u8>)>,
    pub signature_algorithm: String,
    /// DER of the signature algorithm's parameters, when it has any (RSASSA-PSS carries them).
    pub signature_parameters: Option<Vec<u8>>,
    pub signature: Vec<u8>,
    /// Unsigned attributes: the type and the encoding of each value.
    pub unsigned_attributes: Vec<(String, Vec<Vec<u8>>)>,
}

impl SignerInfo {
    /// The signed attributes as a DER SET OF, which is what the signature value covers. DER sorts
    /// the members; some signers hash them in the order they sent instead, so that form follows
    /// when it differs.
    pub(crate) fn signed_attribute_encodings(&self) -> Vec<Vec<u8>> {
        let Some(attributes) = &self.signed_attributes else { return Vec::new() };
        let mut sorted = attributes.clone();
        sorted.sort();
        let canonical = der::tlv(der::SET, &sorted.concat());
        let as_sent = der::tlv(der::SET, &attributes.concat());
        if as_sent == canonical {
            vec![canonical]
        } else {
            vec![canonical, as_sent]
        }
    }
}

/// Parses the ContentInfo holding SignedData at the front of `data`; anything after it is ignored.
pub(crate) fn parse_signed_data(data: &[u8]) -> Option<SignedData> {
    let (content_info, _) = der::read_ber(data)?;
    if content_info.tag != der::SEQUENCE {
        return None;
    }
    let parts = der::children_ber(content_info.content);
    if oid_of(parts.first()?)? != OID_SIGNED_DATA {
        return None;
    }
    let explicit = parts.get(1).filter(|e| e.tag == 0xA0)?;
    let signed_data = der::children_ber(explicit.content).into_iter().next().filter(|e| e.tag == der::SEQUENCE)?;
    // version, digestAlgorithms, encapContentInfo, [0] certificates, [1] crls, signerInfos
    let fields = der::children_ber(signed_data.content);
    let encapsulated = der::children_ber(fields.get(2).filter(|e| e.tag == der::SEQUENCE)?.content);
    let content_type = oid_of(encapsulated.first()?)?;
    let content = match encapsulated.get(1) {
        Some(explicit) if explicit.tag == 0xA0 => Some(der::string_value(der::children_ber(explicit.content).first()?)?),
        _ => None,
    };
    let mut certificates = Vec::new();
    let mut signer_infos = None;
    for field in fields.iter().skip(3) {
        match field.tag {
            0xA0 => {
                certificates = der::children_ber(field.content)
                    .iter()
                    .filter(|c| c.tag == der::SEQUENCE)
                    .map(|c| c.raw.to_vec())
                    .collect()
            }
            0x31 => signer_infos = Some(*field),
            _ => {}
        }
    }
    let signer = der::children_ber(signer_infos?.content).into_iter().next()?;
    Some(SignedData { certificates, content_type, content, signer: parse_signer_info(&signer)? })
}

/// The id-ct-TSTInfo content type carried by an RFC 3161 time-stamp token.
pub(crate) const OID_TST_INFO: &str = "1.2.840.113549.1.9.16.1.4";
/// The id-aa-signatureTimeStampToken unsigned attribute.
pub(crate) const OID_SIGNATURE_TIMESTAMP: &str = "1.2.840.113549.1.9.16.2.14";

/// The parts of a TSTInfo a verifier needs.
#[derive(Debug)]
pub(crate) struct TstInfo {
    /// The messageImprint hash algorithm OID.
    pub hash_algorithm: String,
    /// The messageImprint hashed message.
    pub hashed_message: Vec<u8>,
    /// genTime, as its raw GeneralizedTime text (e.g. `20261013000000Z`).
    pub gen_time: String,
    /// The content of the nonce INTEGER, when the token echoes one.
    pub nonce: Option<Vec<u8>>,
}

/// Parses a TSTInfo (the eContent of a time-stamp token).
pub(crate) fn parse_tst_info(data: &[u8]) -> Option<TstInfo> {
    let (tst, _) = der::read_ber(data)?;
    // version, policy, messageImprint, serialNumber, genTime, accuracy, ordering, nonce, ...
    let fields = der::children_ber(tst.content);
    let imprint = der::children_ber(fields.get(2).filter(|e| e.tag == der::SEQUENCE)?.content);
    let (hash_algorithm, _) = algorithm(imprint.first()?)?;
    let hashed_message = der::string_value(imprint.get(1).filter(|e| e.tag & !0x20 == der::OCTET_STRING)?)?;
    let gen_time = fields.get(4).filter(|e| e.tag == 0x18).map(|e| String::from_utf8_lossy(e.content).into_owned())?;
    // After genTime come the optional accuracy (a SEQUENCE) and ordering (a BOOLEAN), so the first
    // INTEGER is the nonce.
    let nonce = fields.iter().skip(5).find(|e| e.tag == der::INTEGER).map(|e| e.content.to_vec());
    Some(TstInfo { hash_algorithm, hashed_message, gen_time, nonce })
}

/// The value of the first unsigned attribute of type `oid`, if present.
pub(crate) fn unsigned_attribute<'a>(signer: &'a SignerInfo, oid: &str) -> Option<&'a [u8]> {
    signer.unsigned_attributes.iter().find(|(kind, _)| kind == oid).and_then(|(_, values)| values.first()).map(Vec::as_slice)
}

fn parse_signer_info(signer: &Element<'_>) -> Option<SignerInfo> {
    // version, sid, digestAlgorithm, [0] signedAttrs, signatureAlgorithm, signature, [1] unsignedAttrs
    let parts = der::children_ber(signer.content);
    let sid = parts.get(1)?;
    let id = match sid.tag {
        der::SEQUENCE => {
            let kids = der::children_ber(sid.content);
            SignerId::IssuerAndSerial { issuer: der::normalize(kids.first()?)?, serial: der::normalize(kids.get(1)?)? }
        }
        0x80 | 0xA0 => SignerId::KeyIdentifier(der::string_value(sid)?),
        _ => return None,
    };
    let (digest_algorithm, _) = algorithm(parts.get(2)?)?;
    let mut index = 3;
    let mut signed_attributes = None;
    let mut message_digest = None;
    let mut content_type = None;
    let mut signing_certificate = None;
    if parts.get(index)?.tag == 0xA0 {
        let mut attributes = Vec::new();
        for attribute in der::children_ber(parts[index].content) {
            let (kind, values) = attribute_parts(&attribute)?;
            match kind.as_str() {
                OID_MESSAGE_DIGEST => message_digest = Some(der::string_value(values.first()?)?),
                OID_CONTENT_TYPE => content_type = Some(oid_of(values.first()?)?),
                OID_SIGNING_CERTIFICATE => signing_certificate = Some(ess_cert_id(values.first()?, OID_SHA1)?),
                OID_SIGNING_CERTIFICATE_V2 => signing_certificate = Some(ess_cert_id(values.first()?, OID_SHA256)?),
                _ => {}
            }
            attributes.push(der::normalize(&attribute)?);
        }
        signed_attributes = Some(attributes);
        index += 1;
    }
    let (signature_algorithm, signature_parameters) = algorithm(parts.get(index)?)?;
    let signature = parts.get(index + 1).filter(|e| e.tag & !0x20 == der::OCTET_STRING)?;
    let signature = der::string_value(signature)?;
    let mut unsigned_attributes = Vec::new();
    if let Some(unsigned) = parts.get(index + 2).filter(|e| e.tag == 0xA1) {
        for attribute in der::children_ber(unsigned.content) {
            let (kind, values) = attribute_parts(&attribute)?;
            unsigned_attributes.push((kind, values.iter().map(|v| v.raw.to_vec()).collect()));
        }
    }
    Some(SignerInfo {
        id,
        digest_algorithm,
        signed_attributes,
        message_digest,
        content_type,
        signing_certificate,
        signature_algorithm,
        signature_parameters,
        signature,
        unsigned_attributes,
    })
}

/// The first ESSCertID(v2) of a SigningCertificate(V2) value: its hash algorithm (`default_hash`
/// when the field is left out, as v2 allows and v1 always does) and the certificate hash.
fn ess_cert_id(value: &Element<'_>, default_hash: &str) -> Option<(String, Vec<u8>)> {
    let certs = der::children_ber(value.content).into_iter().next().filter(|e| e.tag == der::SEQUENCE)?;
    let first = der::children_ber(certs.content).into_iter().next().filter(|e| e.tag == der::SEQUENCE)?;
    let fields = der::children_ber(first.content);
    match fields.first()? {
        algorithm_id if algorithm_id.tag == der::SEQUENCE => {
            let hash = fields.get(1).filter(|e| e.tag & !0x20 == der::OCTET_STRING)?;
            Some((algorithm(algorithm_id)?.0, der::string_value(hash)?))
        }
        hash if hash.tag & !0x20 == der::OCTET_STRING => Some((default_hash.to_string(), der::string_value(hash)?)),
        _ => None,
    }
}

/// An attribute's type and values.
fn attribute_parts<'a>(attribute: &Element<'a>) -> Option<(String, Vec<Element<'a>>)> {
    let kids = der::children_ber(attribute.content);
    let kind = oid_of(kids.first()?)?;
    let values = der::children_ber(kids.get(1).filter(|v| v.tag == der::SET)?.content);
    Some((kind, values))
}

pub(crate) fn oid_of(element: &Element<'_>) -> Option<String> {
    (element.tag == der::OID).then(|| der::oid_to_string(element.content))
}

/// An AlgorithmIdentifier: the algorithm and the DER of its parameters (absent or NULL gives none).
pub(crate) fn algorithm(element: &Element<'_>) -> Option<(String, Option<Vec<u8>>)> {
    if element.tag != der::SEQUENCE {
        return None;
    }
    let kids = der::children_ber(element.content);
    let oid = oid_of(kids.first()?)?;
    let parameters = match kids.get(1) {
        Some(p) if p.tag != der::NULL => Some(der::normalize(p)?),
        _ => None,
    };
    Some((oid, parameters))
}
