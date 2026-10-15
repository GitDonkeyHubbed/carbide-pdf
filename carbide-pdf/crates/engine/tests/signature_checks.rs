//! How signatures are validated: the certificate path to a trusted certificate, where the
//! signature value sits in the file, and what the CMS signed attributes must say.

use std::sync::OnceLock;

use carbide_engine::signature::{
    Detail, Identity, SignOptions, SignatureReport, SubFilter, TimestampSource, TrustStore,
    Validity,
};
use carbide_engine::Pdf;
use lopdf::{dictionary, Document, Object, Stream};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};

// ---- DER -----------------------------------------------------------------------------------

fn len(n: usize) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    let bytes = n.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    let mut out = vec![0x80 | (bytes.len() - skip) as u8];
    out.extend_from_slice(&bytes[skip..]);
    out
}

fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(len(body.len()));
    out.extend_from_slice(body);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn set(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x31, &parts.concat())
}

fn ctx(n: u8, body: &[u8]) -> Vec<u8> {
    tlv(0xA0 | n, body)
}

fn oid(arcs: &[u32]) -> Vec<u8> {
    let mut content = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut stack = vec![(arc & 0x7F) as u8];
        let mut rest = arc >> 7;
        while rest > 0 {
            stack.push(0x80 | (rest & 0x7F) as u8);
            rest >>= 7;
        }
        stack.reverse();
        content.extend(stack);
    }
    tlv(0x06, &content)
}

fn null() -> Vec<u8> {
    vec![0x05, 0x00]
}

const SHA256_OID: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];
const SHA384_OID: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 2];
const SHA256_RSA_OID: &[u32] = &[1, 2, 840, 113549, 1, 1, 11];
const RSA_PSS_OID: &[u32] = &[1, 2, 840, 113549, 1, 1, 10];
const DATA_OID: &[u32] = &[1, 2, 840, 113549, 1, 7, 1];
const SIGNED_DATA_OID: &[u32] = &[1, 2, 840, 113549, 1, 7, 2];
const TST_INFO_OID: &[u32] = &[1, 2, 840, 113549, 1, 9, 16, 1, 4];

fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

// ---- CMS -----------------------------------------------------------------------------------

fn attribute(kind: &[u32], value: &[u8]) -> Vec<u8> {
    seq(&[&oid(kind), &set(&[value])])
}

fn content_type(kind: &[u32]) -> Vec<u8> {
    attribute(&[1, 2, 840, 113549, 1, 9, 3], &oid(kind))
}

fn message_digest(digest: &[u8]) -> Vec<u8> {
    attribute(&[1, 2, 840, 113549, 1, 9, 4], &tlv(0x04, digest))
}

/// A signingCertificateV2 attribute naming a certificate by its SHA-256 `hash`, and by issuer and
/// serial number (an encoded IssuerSerial) when given.
fn signing_certificate_v2(hash: &[u8], issuer_serial: Option<Vec<u8>>) -> Vec<u8> {
    let mut cert_id = vec![tlv(0x04, hash)];
    cert_id.extend(issuer_serial);
    let refs: Vec<&[u8]> = cert_id.iter().map(Vec::as_slice).collect();
    attribute(
        &[1, 2, 840, 113549, 1, 9, 16, 2, 47],
        &seq(&[&seq(&[&seq(&refs)])]),
    )
}

/// An IssuerSerial: the issuer as a directoryName, and the serial number INTEGER `serial`.
fn issuer_serial(cert_der: &[u8], serial: Option<&[u8]>) -> Vec<u8> {
    let cert = x509_cert::Certificate::from_der(cert_der).unwrap();
    let issuer = cert.tbs_certificate.issuer.to_der().unwrap();
    let serial = serial
        .map(|s| tlv(0x02, s))
        .unwrap_or_else(|| cert.tbs_certificate.serial_number.to_der().unwrap());
    seq(&[&seq(&[&ctx(4, &issuer)]), &serial])
}

/// An IssuerAndSerialNumber naming `cert_der`, or another serial number under its issuer.
fn signer_id(cert_der: &[u8], serial: Option<&[u8]>) -> Vec<u8> {
    let cert = x509_cert::Certificate::from_der(cert_der).unwrap();
    let issuer = cert.tbs_certificate.issuer.to_der().unwrap();
    let serial = serial
        .map(|s| tlv(0x02, s))
        .unwrap_or_else(|| cert.tbs_certificate.serial_number.to_der().unwrap());
    seq(&[&issuer, &serial])
}

/// The signed attributes as the DER SET OF a signature covers.
fn attribute_set(attributes: &[Vec<u8>]) -> Vec<u8> {
    let mut sorted = attributes.to_vec();
    sorted.sort();
    tlv(0x31, &sorted.concat())
}

/// RSA PKCS#1 v1.5 with SHA-256 over `data`.
fn rsa_sign(key: &rsa::RsaPrivateKey, data: &[u8]) -> Vec<u8> {
    key.sign(rsa::Pkcs1v15Sign::new::<Sha256>(), &sha256(data))
        .unwrap()
}

/// A SignedData ContentInfo with one SHA-256 signer named by `sid`.
fn signed_data(
    certificates: &[&[u8]],
    sid: &[u8],
    encapsulated: &[u8],
    attributes: Option<&[Vec<u8>]>,
    signature_algorithm: &[u8],
    signature: &[u8],
) -> Vec<u8> {
    let sha256_alg = seq(&[&oid(SHA256_OID), &null()]);
    let mut info = vec![tlv(0x02, &[1]), sid.to_vec(), sha256_alg.clone()];
    if let Some(attributes) = attributes {
        let mut sorted = attributes.to_vec();
        sorted.sort();
        info.push(ctx(0, &sorted.concat()));
    }
    info.push(signature_algorithm.to_vec());
    info.push(tlv(0x04, signature));
    let refs: Vec<&[u8]> = info.iter().map(Vec::as_slice).collect();
    let body = seq(&[
        &tlv(0x02, &[1]),
        &set(&[&sha256_alg]),
        encapsulated,
        &ctx(0, &certificates.concat()),
        &set(&[&seq(&refs)]),
    ]);
    seq(&[&oid(SIGNED_DATA_OID), &ctx(0, &body)])
}

/// A detached CMS from the test signer over `covered`: with `attributes`, the signature covers
/// them; without, the covered bytes directly. `sid` defaults to the signer's own certificate.
fn signer_cms(covered: &[u8], attributes: Option<&[Vec<u8>]>, sid: Option<Vec<u8>>) -> Vec<u8> {
    let signer = signer();
    let signature = match attributes {
        Some(attributes) => rsa_sign(&signer.key, &attribute_set(attributes)),
        None => rsa_sign(&signer.key, covered),
    };
    signed_data(
        &[&signer.cert],
        &sid.unwrap_or_else(|| signer_id(&signer.cert, None)),
        &seq(&[&oid(DATA_OID)]),
        attributes,
        &seq(&[&oid(SHA256_RSA_OID), &null()]),
        &signature,
    )
}

// ---- documents -----------------------------------------------------------------------------

struct Signer {
    key: rsa::RsaPrivateKey,
    pkcs8: Vec<u8>,
    cert: Vec<u8>,
    identity: Identity,
}

/// One RSA key for every test (generating them is slow), with a self-signed certificate.
fn signer() -> &'static Signer {
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        let pkcs8 = key.to_pkcs8_der().unwrap().as_bytes().to_vec();
        let pair = KeyPair::try_from(pkcs8.as_slice()).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "Check Signer");
        let cert = params.self_signed(&pair).unwrap().der().to_vec();
        Signer {
            identity: Identity::new(vec![cert.clone()], key.clone()).unwrap(),
            key,
            pkcs8,
            cert,
        }
    })
}

fn trust_signer() -> TrustStore {
    TrustStore {
        certificates: vec![signer().cert.clone()],
    }
}

/// A one-page document and its bytes.
fn document() -> (Pdf, Vec<u8>) {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let content = doc.add_object(Stream::new(dictionary! {}, b"0 0 m 10 10 l S".to_vec()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();
    let base = pdf.to_bytes().unwrap();
    (pdf, base)
}

fn sign(
    identity: &Identity,
    sub_filter: SubFilter,
    timestamp: Option<&dyn TimestampSource>,
) -> Vec<u8> {
    let (doc, base) = document();
    let options = SignOptions {
        identity: Some(identity),
        sub_filter,
        signing_time: Some("D:20261001000000Z".into()),
        timestamp,
        ..Default::default()
    };
    doc.sign(&base, &options).unwrap()
}

fn verify(bytes: &[u8], trust: &TrustStore) -> Vec<SignatureReport> {
    Pdf::verify_signatures(bytes, trust, chrono::Utc::now()).unwrap()
}

fn outcome(report: &SignatureReport) -> (Validity, Detail) {
    (report.validity, report.detail)
}

/// The last `/ByteRange` in `file`.
fn byte_range(file: &[u8]) -> [usize; 4] {
    let at = file.windows(10).rposition(|w| w == b"/ByteRange").unwrap();
    let open = at + file[at..].iter().position(|b| *b == b'[').unwrap();
    let close = open + file[open..].iter().position(|b| *b == b']').unwrap();
    let numbers: Vec<usize> = std::str::from_utf8(&file[open + 1..close])
        .unwrap()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    numbers.try_into().unwrap()
}

/// The bytes a signature over `file` covers.
fn covered_bytes(file: &[u8]) -> Vec<u8> {
    let [_, b, c, d] = byte_range(file);
    [&file[..b], &file[c..c + d]].concat()
}

/// Writes `cms` into the last signature's `/Contents`, zero-padded to the reserved size.
fn put_signature_value(file: &mut [u8], cms: &[u8]) {
    let [_, b, c, _] = byte_range(file);
    let hex: String = cms.iter().map(|b| format!("{b:02X}")).collect();
    assert!(hex.len() <= c - b - 2, "the signature does not fit");
    let slot = &mut file[b + 1..c - 1];
    slot.fill(b'0');
    slot[..hex.len()].copy_from_slice(hex.as_bytes());
}

/// A file signed with `sub_filter` by the test signer, whose signature value is then replaced by
/// what `cms` makes from the covered bytes. CAdES reserves the most room, and both sub-filter names
/// are the same length.
fn signed_with(sub_filter: SubFilter, cms: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut file = sign(&signer().identity, SubFilter::CadesDetached, None);
    if sub_filter != SubFilter::CadesDetached {
        let at = file
            .windows(19)
            .rposition(|w| w == b"ETSI.CAdES.detached")
            .unwrap();
        file[at..at + 19].copy_from_slice(sub_filter.name().as_bytes());
    }
    let value = cms(&covered_bytes(&file));
    put_signature_value(&mut file, &value);
    file
}

// ---- signed attributes ---------------------------------------------------------------------

#[test]
fn signed_attributes_must_name_the_content_type() {
    let with = |attributes: &dyn Fn(&[u8]) -> Vec<Vec<u8>>| {
        let file = signed_with(SubFilter::Pkcs7Detached, |covered| {
            signer_cms(covered, Some(&attributes(covered)), None)
        });
        outcome(&verify(&file, &trust_signer())[0])
    };
    assert_eq!(
        with(&|covered| vec![content_type(DATA_OID), message_digest(&sha256(covered))]),
        (Validity::Valid, Detail::TrustedDirectly)
    );
    assert_eq!(
        with(&|covered| vec![message_digest(&sha256(covered))]),
        (Validity::Invalid, Detail::Corrupt)
    );
    assert_eq!(
        with(&|covered| vec![
            content_type(SIGNED_DATA_OID),
            message_digest(&sha256(covered))
        ]),
        (Validity::Invalid, Detail::Corrupt)
    );
}

#[test]
fn the_signing_certificate_attribute_must_name_the_signer() {
    let cert = &signer().cert;
    let with = |sub_filter: SubFilter, extra: Option<Vec<u8>>| {
        let file = signed_with(sub_filter, |covered| {
            let mut attributes = vec![content_type(DATA_OID), message_digest(&sha256(covered))];
            attributes.extend(extra.clone());
            signer_cms(covered, Some(&attributes), None)
        });
        outcome(&verify(&file, &trust_signer())[0])
    };
    let good = (Validity::Valid, Detail::TrustedDirectly);
    let bad = (Validity::Invalid, Detail::Corrupt);

    let right = signing_certificate_v2(&sha256(cert), None);
    assert_eq!(with(SubFilter::Pkcs7Detached, Some(right.clone())), good);
    assert_eq!(with(SubFilter::CadesDetached, Some(right)), good);
    let named = signing_certificate_v2(&sha256(cert), Some(issuer_serial(cert, None)));
    assert_eq!(with(SubFilter::CadesDetached, Some(named)), good);

    // Another certificate's hash.
    let other = signing_certificate_v2(&sha256(b"another certificate"), None);
    assert_eq!(with(SubFilter::Pkcs7Detached, Some(other.clone())), bad);
    assert_eq!(with(SubFilter::CadesDetached, Some(other)), bad);
    // The right hash, with another serial number.
    let misnamed = signing_certificate_v2(&sha256(cert), Some(issuer_serial(cert, Some(&[0x7B]))));
    assert_eq!(with(SubFilter::Pkcs7Detached, Some(misnamed)), bad);
    // CAdES requires the attribute; PKCS#7 does not.
    assert_eq!(with(SubFilter::Pkcs7Detached, None), good);
    assert_eq!(with(SubFilter::CadesDetached, None), bad);
}

#[test]
fn a_signer_identifier_that_names_no_embedded_certificate_is_not_valid() {
    let file = signed_with(SubFilter::Pkcs7Detached, |covered| {
        signer_cms(
            covered,
            None,
            Some(signer_id(&signer().cert, Some(&[0x7B]))),
        )
    });
    let report = verify(&file, &trust_signer()).remove(0);
    assert_eq!(outcome(&report), (Validity::Invalid, Detail::Corrupt));
    assert_eq!(report.signer, None);
}
