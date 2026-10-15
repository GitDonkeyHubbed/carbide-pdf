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

// ---- certificate paths ---------------------------------------------------------------------

fn cert_params(cn: &str, is_ca: IsCa, usages: &[KeyUsagePurpose]) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.is_ca = is_ca;
    params.key_usages = usages.to_vec();
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    params
}

/// A CA certificate, self-signed or issued by `issuer`, adjusted by `adjust`.
fn ca(
    cn: &str,
    issuer: Option<&Issuer<'_, KeyPair>>,
    adjust: impl FnOnce(&mut CertificateParams),
) -> (Vec<u8>, Issuer<'static, KeyPair>) {
    let pair = KeyPair::generate().unwrap();
    let mut params = cert_params(
        cn,
        IsCa::Ca(BasicConstraints::Unconstrained),
        &[KeyUsagePurpose::KeyCertSign],
    );
    adjust(&mut params);
    let cert = match issuer {
        Some(issuer) => params.signed_by(&pair, issuer),
        None => params.self_signed(&pair),
    };
    (cert.unwrap().der().to_vec(), Issuer::new(params, pair))
}

/// The test signer's key in a certificate `issuer` issues, adjusted by `adjust`, with the
/// certificates in `above` embedded after it.
fn leaf(
    issuer: &Issuer<'_, KeyPair>,
    above: &[Vec<u8>],
    adjust: impl FnOnce(&mut CertificateParams),
) -> Identity {
    let pair = KeyPair::try_from(signer().pkcs8.as_slice()).unwrap();
    let mut params = cert_params(
        "Chain Signer",
        IsCa::ExplicitNoCa,
        &[KeyUsagePurpose::DigitalSignature],
    );
    adjust(&mut params);
    let mut chain = vec![params.signed_by(&pair, issuer).unwrap().der().to_vec()];
    chain.extend_from_slice(above);
    Identity::new(chain, signer().key.clone()).unwrap()
}

/// How a signature by `identity` reads when only `root` is trusted.
fn chain_outcome(identity: &Identity, root: &[u8]) -> (Validity, Detail) {
    let signed = sign(identity, SubFilter::Pkcs7Detached, None);
    let trust = TrustStore {
        certificates: vec![root.to_vec()],
    };
    outcome(&verify(&signed, &trust)[0])
}

const REJECTED: (Validity, Detail) = (Validity::ValidityUnknown, Detail::ChainRejected);
const VIA_CHAIN: (Validity, Detail) = (Validity::Valid, Detail::TrustedViaChain);

/// A critical extension of a kind nothing here checks.
fn unknown_critical_extension() -> rcgen::CustomExtension {
    let mut extension =
        rcgen::CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 55555, 1], null());
    extension.set_criticality(true);
    extension
}

#[test]
fn a_critical_extension_nothing_checks_leaves_the_identity_unconfirmed() {
    let (root, root_issuer) = ca("Root CA", None, |_| {});
    assert_eq!(
        chain_outcome(&leaf(&root_issuer, &[], |_| {}), &root),
        VIA_CHAIN
    );

    // On the signer's certificate.
    let identity = leaf(&root_issuer, &[], |p| {
        p.custom_extensions.push(unknown_critical_extension())
    });
    assert_eq!(chain_outcome(&identity, &root), REJECTED);

    // On a CA between the signer and the trusted root.
    let (intermediate, intermediate_issuer) = ca("Intermediate CA", Some(&root_issuer), |p| {
        p.custom_extensions.push(unknown_critical_extension())
    });
    let identity = leaf(&intermediate_issuer, &[intermediate], |_| {});
    assert_eq!(chain_outcome(&identity, &root), REJECTED);

    // The same extension, not marked critical, may be ignored.
    let (intermediate, intermediate_issuer) = ca("Intermediate CA", Some(&root_issuer), |p| {
        let mut extension = unknown_critical_extension();
        extension.set_criticality(false);
        p.custom_extensions.push(extension)
    });
    let identity = leaf(&intermediate_issuer, &[intermediate], |_| {});
    assert_eq!(chain_outcome(&identity, &root), VIA_CHAIN);
}

#[test]
fn name_constraints_on_a_ca_leave_the_identity_unconfirmed() {
    let (root, root_issuer) = ca("Constrained Root CA", None, |p| {
        p.name_constraints = Some(rcgen::NameConstraints {
            permitted_subtrees: vec![rcgen::GeneralSubtree::DnsName("example.com".into())],
            excluded_subtrees: Vec::new(),
        });
    });
    assert_eq!(
        chain_outcome(&leaf(&root_issuer, &[], |_| {}), &root),
        REJECTED
    );

    let (root, root_issuer) = ca("Root CA", None, |_| {});
    let (intermediate, intermediate_issuer) = ca("Intermediate CA", Some(&root_issuer), |p| {
        p.name_constraints = Some(rcgen::NameConstraints {
            permitted_subtrees: Vec::new(),
            excluded_subtrees: vec![rcgen::GeneralSubtree::Rfc822Name("example.org".into())],
        });
    });
    let identity = leaf(&intermediate_issuer, &[intermediate], |_| {});
    assert_eq!(chain_outcome(&identity, &root), REJECTED);

    // Name constraints should be critical, but are not always marked so.
    let (root, root_issuer) = ca("Loosely Constrained Root CA", None, |p| {
        let permitted = seq(&[&ctx(0, &seq(&[&tlv(0x82, b"example.com")]))]);
        let mut extension = rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 30], permitted);
        extension.set_criticality(false);
        p.custom_extensions.push(extension);
    });
    assert_eq!(
        chain_outcome(&leaf(&root_issuer, &[], |_| {}), &root),
        REJECTED
    );
}

#[test]
fn a_ca_restricted_to_other_purposes_cannot_vouch_for_a_signer() {
    let restricted = |purposes: Vec<ExtendedKeyUsagePurpose>| {
        let (root, root_issuer) = ca("Purpose Root CA", None, |p| {
            p.extended_key_usages = purposes
        });
        chain_outcome(&leaf(&root_issuer, &[], |_| {}), &root)
    };
    assert_eq!(
        restricted(vec![ExtendedKeyUsagePurpose::ServerAuth]),
        REJECTED
    );
    assert_eq!(
        restricted(vec![ExtendedKeyUsagePurpose::TimeStamping]),
        REJECTED
    );
    // The purposes a signer's own certificate may carry are fine on its CA too.
    assert_eq!(
        restricted(vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::EmailProtection
        ]),
        VIA_CHAIN
    );
    assert_eq!(restricted(vec![ExtendedKeyUsagePurpose::Any]), VIA_CHAIN);
}

// ---- the signature value in the file -------------------------------------------------------

#[test]
fn a_hex_signature_value_with_line_breaks_and_an_odd_last_digit_verifies() {
    let mut file = sign(&signer().identity, SubFilter::Pkcs7Detached, None);
    let [_, b, c, _] = byte_range(&file);
    let slot = c - b - 2;
    let cms_hex = std::str::from_utf8(&file[b + 1..c - 1])
        .unwrap()
        .to_string();
    let used = cms_hex.trim_end_matches('0').len().div_ceil(2) * 2;
    let mut spaced = String::new();
    for (i, chunk) in cms_hex.as_bytes()[..used].chunks(64).enumerate() {
        spaced.push_str(if i % 2 == 0 { "\r\n" } else { " \t" });
        spaced.push_str(std::str::from_utf8(chunk).unwrap());
    }
    spaced.push('\n');
    while spaced.len() < slot {
        spaced.push('0');
    }
    // An odd number of digits: the last one stands for its high nibble alone.
    let digits = spaced.bytes().filter(u8::is_ascii_hexdigit).count();
    if digits % 2 == 0 {
        spaced.pop();
        spaced.push(' ');
    }
    assert_eq!(spaced.len(), slot);
    file[b + 1..c - 1].copy_from_slice(spaced.as_bytes());
    let report = verify(&file, &trust_signer()).remove(0);
    assert_eq!(outcome(&report), (Validity::Valid, Detail::TrustedDirectly));

    // Anything else between the digits is refused.
    let mut broken = file.clone();
    let at = b + 1 + spaced.find('\n').unwrap();
    broken[at] = b'G';
    let report = verify(&broken, &trust_signer()).remove(0);
    assert_eq!(report.validity, Validity::Invalid);
}

#[test]
fn an_escaped_contents_key_is_still_the_signature_value() {
    let mut file = sign(&signer().identity, SubFilter::Pkcs7Detached, None);
    let [_, b, ..] = byte_range(&file);
    // Write the key as /#43ontents, two bytes longer, and take two of the leading zeros off the
    // byte range's first number, written before it, so nothing else moves.
    let key = file[..b]
        .windows(9)
        .rposition(|w| w == b"/Contents")
        .unwrap();
    let range = file[..b]
        .windows(10)
        .rposition(|w| w == b"/ByteRange")
        .unwrap();
    assert!(range < key, "the byte range is written before the value");
    let open = range + file[range..].iter().position(|x| *x == b'[').unwrap();
    assert_eq!(&file[open..open + 12], b"[0000000000 ");
    let mut edited = file[..open + 1].to_vec();
    edited.extend_from_slice(&file[open + 3..key]);
    edited.extend_from_slice(b"/#43ontents");
    edited.extend_from_slice(&file[key + 9..]);
    assert_eq!(edited.len(), file.len());
    file = edited;
    let value = signer_cms(&covered_bytes(&file), None, None);
    put_signature_value(&mut file, &value);
    let report = verify(&file, &trust_signer()).remove(0);
    assert_eq!(outcome(&report), (Validity::Valid, Detail::TrustedDirectly));
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

// ---- RSA-PSS keys --------------------------------------------------------------------------

fn pss_params(hash: &[u32], salt: u8) -> Vec<u8> {
    let hash = seq(&[&oid(hash), &null()]);
    seq(&[
        &ctx(0, &hash),
        &ctx(1, &seq(&[&oid(&[1, 2, 840, 113549, 1, 1, 8]), &hash])),
        &ctx(2, &[0x02, 0x01, salt]),
    ])
}

#[test]
fn an_rsa_pss_key_restricted_to_other_parameters_does_not_verify_the_signature() {
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    let key_with = |parameters: Option<Vec<u8>>| {
        let mut certificate = x509_cert::Certificate::from_der(&signer().cert).unwrap();
        certificate
            .tbs_certificate
            .subject_public_key_info
            .algorithm = x509_cert::spki::AlgorithmIdentifierOwned {
            oid: "1.2.840.113549.1.1.10".parse().unwrap(),
            parameters: parameters.map(|p| x509_cert::der::Any::from_der(&p).unwrap()),
        };
        certificate.to_der().unwrap()
    };
    // A SHA-256 PSS signature with a 32-byte salt, under a key that allows it or not.
    let with = |cert: Vec<u8>| {
        let file = signed_with(SubFilter::Pkcs7Detached, |covered| {
            let signature = rsa::pss::SigningKey::<Sha256>::new(signer().key.clone())
                .sign_with_rng(&mut rand::rngs::OsRng, covered)
                .to_vec();
            signed_data(
                &[&cert],
                &signer_id(&cert, None),
                &seq(&[&oid(DATA_OID)]),
                None,
                &seq(&[&oid(RSA_PSS_OID), &pss_params(SHA256_OID, 32)]),
                &signature,
            )
        });
        let trust = TrustStore {
            certificates: vec![cert.clone()],
        };
        verify(&file, &trust).remove(0).validity
    };
    assert_eq!(with(key_with(None)), Validity::Valid);
    assert_eq!(
        with(key_with(Some(pss_params(SHA256_OID, 32)))),
        Validity::Valid
    );
    assert_eq!(
        with(key_with(Some(pss_params(SHA256_OID, 20)))),
        Validity::Valid
    );
    assert_eq!(
        with(key_with(Some(pss_params(SHA384_OID, 48)))),
        Validity::Invalid
    );
    assert_eq!(
        with(key_with(Some(pss_params(SHA256_OID, 64)))),
        Validity::Invalid
    );
}

// ---- time stamps ---------------------------------------------------------------------------

/// An RFC 3161 authority using the test signer's key, whose tokens give `gen_time`.
struct Tsa {
    cert: Vec<u8>,
    gen_time: &'static str,
}

fn tsa_certificate() -> &'static [u8] {
    static CERT: OnceLock<Vec<u8>> = OnceLock::new();
    CERT.get_or_init(|| {
        let pair = KeyPair::try_from(signer().pkcs8.as_slice()).unwrap();
        let mut params = cert_params(
            "Check TSA",
            IsCa::ExplicitNoCa,
            &[KeyUsagePurpose::DigitalSignature],
        );
        let mut purpose = rcgen::CustomExtension::from_oid_content(
            &[2, 5, 29, 37],
            seq(&[&oid(&[1, 3, 6, 1, 5, 5, 7, 3, 8])]),
        );
        purpose.set_criticality(true);
        params.custom_extensions.push(purpose);
        params.self_signed(&pair).unwrap().der().to_vec()
    })
}

impl TimestampSource for Tsa {
    fn token_for_digest(&self, digest: &[u8; 32]) -> carbide_engine::Result<Vec<u8>> {
        let tst_info = seq(&[
            &tlv(0x02, &[1]),
            &oid(&[1, 2, 3, 4]),
            &seq(&[&seq(&[&oid(SHA256_OID), &null()]), &tlv(0x04, digest)]),
            &tlv(0x02, &[1]),
            &tlv(0x18, self.gen_time.as_bytes()),
        ]);
        let attributes = [
            content_type(TST_INFO_OID),
            message_digest(&sha256(&tst_info)),
            signing_certificate_v2(&sha256(&self.cert), None),
        ];
        Ok(signed_data(
            &[&self.cert],
            &signer_id(&self.cert, None),
            &seq(&[&oid(TST_INFO_OID), &ctx(0, &tlv(0x04, &tst_info))]),
            Some(&attributes),
            &seq(&[&oid(SHA256_RSA_OID), &null()]),
            &rsa_sign(&signer().key, &attribute_set(&attributes)),
        ))
    }
}

#[test]
fn a_time_stamp_time_must_be_a_plain_utc_generalized_time() {
    let stamped = |gen_time: &'static str| {
        let tsa = Tsa {
            cert: tsa_certificate().to_vec(),
            gen_time,
        };
        let file = sign(&signer().identity, SubFilter::Pkcs7Detached, Some(&tsa));
        let trust = TrustStore {
            certificates: vec![signer().cert.clone(), tsa.cert.clone()],
        };
        let report = verify(&file, &trust).remove(0);
        assert_ne!(
            report.has_embedded_timestamp,
            report.embedded_timestamp_failed
        );
        report.has_embedded_timestamp
    };
    assert!(stamped("20260101000000Z"));
    assert!(stamped("20260101000000.25Z"));
    for bad in [
        "20260101000000+0100",
        "20260101000000-0500",
        "20260101000000",
        "2026010100000099Z",
        "20260101000000Zjunk",
        "20260101000000.Z",
        "20260101000000.250Z",
        "20260101000000,5Z",
        "202601010000Z",
    ] {
        assert!(!stamped(bad), "{bad} was taken as a time");
    }
}

// ---- signature fields ----------------------------------------------------------------------

#[test]
fn a_signature_field_typed_by_its_parent_is_still_checked() {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
        }),
    );
    // The value is no longer a signature dictionary: it has lost its byte range and contents.
    let value = doc.add_object(dictionary! { "Type" => "Sig" });
    let grandparent = doc.new_object_id();
    let parent = doc.new_object_id();
    let kid = doc.add_object(dictionary! {
        "T" => Object::string_literal("Approval"),
        "Parent" => parent,
        "V" => value,
    });
    doc.objects.insert(
        parent,
        Object::Dictionary(dictionary! {
            "T" => Object::string_literal("Section"),
            "Parent" => grandparent,
            "Kids" => vec![kid.into()],
        }),
    );
    doc.objects.insert(
        grandparent,
        Object::Dictionary(dictionary! {
            "T" => Object::string_literal("Form"),
            "FT" => "Sig",
            "Kids" => vec![parent.into()],
        }),
    );
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "AcroForm" => dictionary! { "Fields" => vec![grandparent.into()] },
    });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();

    let reports = verify(&bytes, &TrustStore::default());
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].field_name, "Approval");
    assert_eq!(outcome(&reports[0]), (Validity::Invalid, Detail::Corrupt));
}

// ---- issuer names --------------------------------------------------------------------------

/// A self-signed certificate for the test key whose name is a PrintableString "Check Signer".
fn printable_signer_cert() -> &'static [u8] {
    static CERT: OnceLock<Vec<u8>> = OnceLock::new();
    CERT.get_or_init(|| {
        let pair = KeyPair::try_from(signer().pkcs8.as_slice()).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(
            DnType::CommonName,
            rcgen::DnValue::PrintableString("Check Signer".try_into().unwrap()),
        );
        params.self_signed(&pair).unwrap().der().to_vec()
    })
}

/// A Name with one common name, its value encoded with `tag`.
fn common_name(tag: u8, value: &[u8]) -> Vec<u8> {
    seq(&[&set(&[&seq(&[&oid(&[2, 5, 4, 3]), &tlv(tag, value)])])])
}

fn utf16be(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// What verifying a signature by the printable-name certificate gives when the SignerInfo names
/// its issuer as `issuer` (and its serial number as the certificate's own), with an ESS
/// signing-certificate attribute naming the same issuer.
fn outcome_naming_issuer(issuer: &[u8]) -> (Validity, Detail) {
    let cert = printable_signer_cert();
    let serial = x509_cert::Certificate::from_der(cert)
        .unwrap()
        .tbs_certificate
        .serial_number
        .to_der()
        .unwrap();
    let named = seq(&[&seq(&[&ctx(4, issuer)]), &serial]);
    let file = signed_with(SubFilter::CadesDetached, |covered| {
        let attributes = vec![
            content_type(DATA_OID),
            message_digest(&sha256(covered)),
            signing_certificate_v2(&sha256(cert), Some(named.clone())),
        ];
        signed_data(
            &[cert],
            &seq(&[issuer, &serial]),
            &seq(&[&oid(DATA_OID)]),
            Some(&attributes),
            &seq(&[&oid(SHA256_RSA_OID), &null()]),
            &rsa_sign(&signer().key, &attribute_set(&attributes)),
        )
    });
    let trust = TrustStore {
        certificates: vec![cert.to_vec()],
    };
    outcome(&verify(&file, &trust)[0])
}

#[test]
fn a_signer_issuer_spelled_another_way_still_names_the_certificate() {
    let good = (Validity::Valid, Detail::TrustedDirectly);
    // The certificate's own spelling.
    assert_eq!(
        outcome_naming_issuer(&common_name(0x13, b"Check Signer")),
        good
    );
    // Another string type, upper case, stray and repeated white space.
    assert_eq!(
        outcome_naming_issuer(&common_name(0x0C, b"CHECK SIGNER")),
        good
    );
    assert_eq!(
        outcome_naming_issuer(&common_name(0x0C, b"  check \t signer ")),
        good
    );
    assert_eq!(
        outcome_naming_issuer(&common_name(0x1E, &utf16be("Check Signer"))),
        good
    );
    assert_eq!(
        outcome_naming_issuer(&common_name(0x14, b"CHECK SIGNER")),
        good
    );
}

#[test]
fn a_different_issuer_with_the_same_serial_is_not_the_signer() {
    let bad = (Validity::Invalid, Detail::Corrupt);
    assert_eq!(
        outcome_naming_issuer(&common_name(0x0C, b"Other Signer")),
        bad
    );
    // The same words joined, or another attribute type holding the same text.
    assert_eq!(
        outcome_naming_issuer(&common_name(0x0C, b"CheckSigner")),
        bad
    );
    let organization = seq(&[&set(&[&seq(&[
        &oid(&[2, 5, 4, 10]),
        &tlv(0x0C, b"Check Signer"),
    ])])]);
    assert_eq!(outcome_naming_issuer(&organization), bad);
    // An extra name component.
    let two = seq(&[
        &set(&[&seq(&[&oid(&[2, 5, 4, 3]), &tlv(0x0C, b"Check Signer")])]),
        &set(&[&seq(&[&oid(&[2, 5, 4, 10]), &tlv(0x0C, b"Extra")])]),
    ]);
    assert_eq!(outcome_naming_issuer(&two), bad);
}
