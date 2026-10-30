//! A signature widget's rectangle decides whether deleting its page keeps the signature.

use carbide_engine::Pdf;
use lopdf::{dictionary, Document, Object, ObjectId, Stream};

const VISIBLE_RECT: &[u8] = b"SIG-VISIBLE-RECT-7f3a";
const VISIBLE_CORNERS: &[u8] = b"SIG-VISIBLE-CORNERS-7f3a";
const INVISIBLE: &[u8] = b"SIG-INVISIBLE-7f3a";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// A two-page document. `widgets` are signature fields on the second page.
fn signed_pdf(widgets: impl FnOnce(&mut Document, ObjectId) -> Vec<Object>) -> Pdf {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();
    for _ in 0..2 {
        let content = doc.add_object(Stream::new(lopdf::Dictionary::new(), Vec::new()));
        kids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            "Contents" => content,
        })));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids.clone(),
            "Count" => 2,
        }),
    );
    let page = kids[1].as_reference().unwrap();
    let fields = widgets(&mut doc, page);
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", fields.clone());
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "AcroForm" => dictionary! { "Fields" => fields },
    });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

fn signature(doc: &mut Document, page: ObjectId, rect: Object, marker: &[u8]) -> Object {
    let value = doc.add_object(dictionary! {
        "Type" => "Sig",
        "Filter" => "Adobe.PPKLite",
        "SubFilter" => "adbe.pkcs7.detached",
        "Reason" => Object::string_literal(marker.to_vec()),
    });
    let widget = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Sig",
        "T" => Object::string_literal(marker.to_vec()),
        "V" => value,
        "P" => page,
    });
    doc.get_dictionary_mut(widget).unwrap().set("Rect", rect);
    Object::Reference(widget)
}

#[test]
fn deleting_a_page_drops_a_visible_signature_whose_rectangle_is_indirect() {
    // The rectangle is an array in its own object, and one whose corners are each in their own
    // object. Both cover the page. An invisible signature stores a zero rectangle the same way.
    let mut pdf = signed_pdf(|doc, page| {
        let visible = doc.add_object(vec![10.into(), 20.into(), 180.into(), 70.into()]);
        let corners = Object::Array(vec![
            Object::Reference(doc.add_object(Object::Integer(12))),
            Object::Reference(doc.add_object(Object::Integer(24))),
            Object::Reference(doc.add_object(Object::Integer(160))),
            Object::Reference(doc.add_object(Object::Integer(90))),
        ]);
        let invisible = doc.add_object(vec![0.into(), 0.into(), 0.into(), 0.into()]);
        vec![
            signature(doc, page, Object::Reference(visible), VISIBLE_RECT),
            signature(doc, page, corners, VISIBLE_CORNERS),
            signature(doc, page, Object::Reference(invisible), INVISIBLE),
        ]
    });
    assert_eq!(pdf.page_count(), 2);
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, VISIBLE_RECT),
        "a signature whose rectangle is an indirect array is still in the file"
    );
    assert!(
        !contains(&saved, VISIBLE_CORNERS),
        "a signature whose corners are indirect numbers is still in the file"
    );
    assert!(
        contains(&saved, INVISIBLE),
        "an invisible signature was removed with the page"
    );
}
