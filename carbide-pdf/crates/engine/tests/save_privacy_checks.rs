//! What saving keeps and what it leaves out: resources that only removed content used, and the
//! file's own bytes read back as they were.

use carbide_engine::{Pdf, SavePlan, SavedFile};
use lopdf::{dictionary, Document, Object, Stream};

const SECRET: &[u8] = b"SECRET-PAYLOAD-7f3a";

/// An image XObject whose data is `SECRET`, stored unfiltered so it shows in the saved bytes.
fn secret_image(doc: &mut Document) -> Object {
    let mut data = SECRET.to_vec();
    data.resize(3 * 8 * 8, 0);
    Object::Reference(doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 8, "Height" => 8,
            "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        },
        data,
    )))
}

fn helvetica(doc: &mut Document) -> Object {
    Object::Reference(doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    }))
}

/// A document with one page per entry of `contents`, all sharing `resources`.
fn shared_resources_pdf(
    build: impl FnOnce(&mut Document) -> lopdf::Dictionary,
    contents: &[&str],
) -> Pdf {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let resources = build(&mut doc);
    let resources_id = doc.add_object(resources);
    let mut kids = Vec::new();
    for content in contents {
        let content_id = doc.add_object(Stream::new(
            lopdf::Dictionary::new(),
            content.as_bytes().to_vec(),
        ));
        kids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            "Contents" => content_id, "Resources" => resources_id,
        })));
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn deleted_page_image_does_not_survive_through_a_font_of_the_same_name() {
    // The kept page draws the font /R1; the deleted page drew the image /R1.
    let mut pdf = shared_resources_pdf(
        |doc| {
            let font = helvetica(doc);
            let image = secret_image(doc);
            dictionary! {
                "Font" => dictionary! { "R1" => font },
                "XObject" => dictionary! { "R1" => image },
            }
        },
        &[
            "BT /R1 12 Tf 10 10 Td (kept) Tj ET",
            "q 100 0 0 100 0 0 cm /R1 Do Q",
        ],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
    assert!(
        contains(&saved, b"Helvetica"),
        "the kept page lost its font"
    );
}

#[test]
fn deleted_page_image_does_not_survive_in_a_form_with_resources_of_its_own() {
    // Each page draws a form; the two forms share one resource dictionary, which holds the image
    // only the deleted page's form drew.
    let mut pdf = shared_resources_pdf(
        |doc| {
            let font = helvetica(doc);
            let image = secret_image(doc);
            let shared = doc.add_object(dictionary! {
                "Font" => dictionary! { "F1" => font },
                "XObject" => dictionary! { "Im1" => image },
            });
            let form = |doc: &mut Document, content: &str| {
                Object::Reference(doc.add_object(Stream::new(
                    dictionary! {
                        "Type" => "XObject", "Subtype" => "Form",
                        "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                        "Resources" => shared,
                    },
                    content.as_bytes().to_vec(),
                )))
            };
            let kept = form(doc, "BT /F1 12 Tf (kept) Tj ET");
            let gone = form(doc, "q 100 0 0 100 0 0 cm /Im1 Do Q");
            dictionary! { "XObject" => dictionary! { "Fm1" => kept, "Fm2" => gone } }
        },
        &["/Fm1 Do", "/Fm2 Do"],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
    assert!(
        contains(&saved, b"Helvetica"),
        "the kept form lost its font"
    );
}

// ---- legacy passwords ----------------------------------------------------------------------

const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut state: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|byte| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(state[i as usize]);
            state.swap(i as usize, j as usize);
            byte ^ state[state[i as usize].wrapping_add(state[j as usize]) as usize]
        })
        .collect()
}

/// A one-page RC4 (revision 2) file whose open password is `user`, as raw bytes.
fn rc4_file(user: &[u8]) -> Vec<u8> {
    use md5::{Digest, Md5};
    let pad = |password: &[u8]| {
        let mut padded = password[..password.len().min(32)].to_vec();
        padded.extend_from_slice(&PADDING[..32 - padded.len()]);
        padded
    };
    let id = b"0123456789abcdef".to_vec();
    let permissions: i32 = -4;
    let owner_key: [u8; 16] = Md5::digest(pad(b"owner")).into();
    let o = rc4(&owner_key[..5], &pad(user));
    let file_key: [u8; 16] = Md5::new()
        .chain_update(pad(user))
        .chain_update(&o)
        .chain_update(permissions.to_le_bytes())
        .chain_update(&id)
        .finalize()
        .into();
    let u = rc4(&file_key[..5], &PADDING);

    let mut doc = Document::with_version("1.4");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    let encrypt = doc.add_object(dictionary! {
        "Filter" => "Standard", "V" => 1, "R" => 2, "P" => permissions,
        "O" => Object::String(o, lopdf::StringFormat::Hexadecimal),
        "U" => Object::String(u, lopdf::StringFormat::Hexadecimal),
    });
    doc.trailer.set("Root", catalog);
    doc.trailer.set("Encrypt", encrypt);
    let id = Object::String(id, lopdf::StringFormat::Hexadecimal);
    doc.trailer.set("ID", vec![id.clone(), id]);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn legacy_password_with_a_tab_and_an_accent_opens_the_file() {
    // "é<tab>a" in PDFDocEncoding: é is 0xE9, the tab stays 0x09.
    let file = rc4_file(&[0xe9, 0x09, b'a']);
    let pdf = Pdf::from_bytes_with_password(&file, "é\ta").expect("the password is right");
    assert_eq!(pdf.page_count(), 1);
}

// ---- linearization -------------------------------------------------------------------------

#[test]
fn text_reading_linearized_near_the_start_does_not_hide_an_update() {
    // The first object is the document information, whose title reads "/Linearized".
    let mut doc = Document::with_version("1.7");
    let info = doc.add_object(dictionary! {
        "Title" => Object::string_literal("/Linearized"),
    });
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    doc.trailer.set("Info", info);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    assert!(contains(&bytes[..1024.min(bytes.len())], b"/Linearized"));

    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert_eq!(saved.appended_updates(), 0);
    let catalog = pdf
        .document()
        .trailer
        .get(b"Root")
        .unwrap()
        .as_reference()
        .unwrap();
    pdf.document_mut()
        .get_dictionary_mut(catalog)
        .unwrap()
        .set("Lang", Object::string_literal("en"));
    let updated = pdf.save_bytes(SavePlan::Incremental, Some(&saved)).unwrap();
    let (_, saved) = SavedFile::open(updated, None).unwrap();
    assert_eq!(saved.appended_updates(), 1);
}

// ---- comparing protections -----------------------------------------------------------------

/// The encryption dictionary of an AES-128 (revision 4) file with an empty open password.
fn r4_encryption() -> lopdf::Dictionary {
    use lopdf::encryption::crypt_filters::Aes128CryptFilter;
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    let mut doc = Document::with_version("1.6");
    doc.trailer.set("ID", file_id());
    let state = EncryptionState::try_from(EncryptionVersion::V4 {
        document: &doc,
        encrypt_metadata: true,
        crypt_filters: [(
            b"StdCF".to_vec(),
            std::sync::Arc::new(Aes128CryptFilter) as std::sync::Arc<_>,
        )]
        .into_iter()
        .collect(),
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: "owner",
        user_password: "",
        permissions: Permissions::all(),
    })
    .unwrap();
    state.encode().unwrap()
}

fn file_id() -> Vec<Object> {
    let id = Object::String(
        b"0123456789abcdef".to_vec(),
        lopdf::StringFormat::Hexadecimal,
    );
    vec![id.clone(), id]
}

/// A one-page file encrypted with `encryption`; it holds no strings or streams to encrypt.
fn encrypted_file(encryption: lopdf::Dictionary) -> Pdf {
    let mut doc = Document::with_version("1.6");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    let encrypt = doc.add_object(encryption);
    doc.trailer.set("Root", catalog);
    doc.trailer.set("Encrypt", encrypt);
    doc.trailer.set("ID", file_id());
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

#[test]
fn protections_that_encrypt_strings_differently_are_not_the_same() {
    // Same passwords, file key and permissions; only one of them encrypts strings.
    let encryption = r4_encryption();
    let sealed = encrypted_file(encryption.clone());
    let mut without = encryption;
    without.set("StrF", Object::Name(b"Identity".to_vec()));
    let clear = encrypted_file(without);
    let (a, b) = (sealed.protection().unwrap(), clear.protection().unwrap());
    assert!(
        !a.same_as(&b),
        "a merge would write the strings of one file in clear"
    );
    assert!(Pdf::shared_protection(&[&sealed, &clear]).is_err());
    assert!(a.same_as(&sealed.protection().unwrap()));
}

// ---- the report of Remove Hidden Information -------------------------------------------------

/// A one-page document; `edit` adds to it, given the catalog's and the page's ids.
fn one_page(edit: impl FnOnce(&mut Document, lopdf::ObjectId, lopdf::ObjectId)) -> Pdf {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    edit(&mut doc, catalog, page);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

#[test]
fn removals_that_were_the_only_hidden_information_are_reported() {
    use carbide_engine::HiddenItem;

    let mut pdf = one_page(|doc, _, page| {
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("LastModified", Object::string_literal("D:20240101000000Z"));
    });
    let report = pdf
        .remove_hidden_information(&[HiddenItem::PrivateData])
        .unwrap();
    assert_eq!(report.count(HiddenItem::PrivateData), 1);

    let mut pdf = one_page(|doc, catalog, _| {
        doc.get_dictionary_mut(catalog)
            .unwrap()
            .set("Collection", dictionary! { "View" => "D" });
    });
    let report = pdf
        .remove_hidden_information(&[HiddenItem::Attachments])
        .unwrap();
    assert_eq!(report.count(HiddenItem::Attachments), 1);

    // A check box shown checked and a text field showing text, neither holding a value.
    let mut pdf = one_page(|doc, catalog, page| {
        let shown = doc.add_object(Stream::new(
            dictionary! { "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()] },
            b"BT (typed) Tj ET".to_vec(),
        ));
        let check = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Btn", "T" => Object::string_literal("c"),
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()], "P" => page, "AS" => "Yes",
        });
        let text = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => Object::string_literal("t"),
            "Rect" => vec![0.into(), 20.into(), 10.into(), 30.into()], "P" => page,
            "AP" => dictionary! { "N" => shown },
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![check.into(), text.into()]);
        doc.get_dictionary_mut(catalog).unwrap().set(
            "AcroForm",
            dictionary! { "Fields" => vec![check.into(), text.into()] },
        );
    });
    let report = pdf
        .remove_hidden_information(&[HiddenItem::FormData])
        .unwrap();
    assert_eq!(report.count(HiddenItem::FormData), 2);
}

// ---- changes to annotations ----------------------------------------------------------------

#[test]
fn ink_points_taken_out_in_place_count_as_removed() {
    let ink = std::cell::Cell::new((0, 0));
    let bytes = {
        let pdf = one_page(|doc, _, page| {
            let id = doc.add_object(dictionary! {
                "Type" => "Annot", "Subtype" => "Ink", "P" => page,
                "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "InkList" => vec![Object::Array(
                    [10, 10, 50, 50, 90, 10].into_iter().map(Object::from).collect(),
                )],
            });
            doc.get_dictionary_mut(page)
                .unwrap()
                .set("Annots", vec![id.into()]);
            ink.set(id);
        });
        let mut copy = pdf.document().clone();
        let mut bytes = Vec::new();
        copy.save_to(&mut bytes).unwrap();
        bytes
    };
    let (mut pdf, saved) = SavedFile::open(bytes, None).unwrap();
    assert!(!saved.removed_from(&pdf));
    pdf.document_mut()
        .get_dictionary_mut(ink.get())
        .unwrap()
        .set(
            "InkList",
            vec![Object::Array(
                [10, 10, 50, 50].into_iter().map(Object::from).collect(),
            )],
        );
    assert!(
        saved.removed_from(&pdf),
        "the removed stroke points went unnoticed"
    );
}

// ---- reading the file as it is -------------------------------------------------------------

#[test]
fn names_and_text_that_read_like_object_streams_come_back_unchanged() {
    let pdf = one_page(|doc, catalog, _| {
        let info =
            doc.add_object(dictionary! { "Title" => Object::string_literal("/Cb-ObS notes") });
        doc.trailer.set("Info", info);
        doc.get_dictionary_mut(catalog)
            .unwrap()
            .set("ObjStm", Object::Name(b"Custom".to_vec()));
    });
    let doc = pdf.document();
    let catalog = doc.catalog().unwrap();
    assert!(catalog.has(b"ObjStm"), "a key named /ObjStm was renamed");
    assert!(!catalog.has(b"Cb-ObS"));
    let info = doc
        .get_dictionary(doc.trailer.get(b"Info").unwrap().as_reference().unwrap())
        .unwrap();
    assert_eq!(
        info.get(b"Title").unwrap().as_str().unwrap(),
        b"/Cb-ObS notes",
        "text in the file was changed"
    );
}
