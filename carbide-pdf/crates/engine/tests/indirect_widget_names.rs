//! Form fields whose widget marker or field type is an object of its own.
//!
//! A name may be stored indirectly. Deleting the page that shows the field has to
//! follow that reference, or the field's value stays in the saved file. An invisible
//! signature is the exception: it stays with the document, so its type has to be
//! read the same way or the deletion treats it as an ordinary field and drops it.

use carbide_engine::Pdf;
use lopdf::{dictionary, Document, Object, ObjectId};

fn two_page_file(widget_on: usize, widget: ObjectId, doc: &mut Document) -> Vec<u8> {
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();
    for index in 0..2 {
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        });
        if index == widget_on {
            doc.get_dictionary_mut(page)
                .unwrap()
                .set("Annots", vec![widget.into()]);
            doc.get_dictionary_mut(widget)
                .unwrap()
                .set("P", Object::Reference(page));
        }
        kids.push(Object::Reference(page));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => 2,
        }),
    );
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "AcroForm" => dictionary! { "Fields" => vec![widget.into()] },
    });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn deleting_a_page_drops_a_widget_whose_subtype_is_indirect() {
    let mut doc = Document::with_version("1.7");
    let subtype = doc.add_object(Object::Name(b"Widget".to_vec()));
    let widget = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => subtype,
        "FT" => "Tx",
        "T" => "secret",
        "V" => Object::string_literal("SECRET-INDIRECT-WIDGET"),
        "Rect" => vec![10.into(), 20.into(), 180.into(), 40.into()],
    });
    let bytes = two_page_file(1, widget, &mut doc);
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !saved
            .windows(b"SECRET-INDIRECT-WIDGET".len())
            .any(|w| w == b"SECRET-INDIRECT-WIDGET"),
        "the deleted page's field value is still in the file"
    );
}

#[test]
fn deleting_a_page_keeps_an_invisible_signature_whose_type_is_indirect() {
    let mut doc = Document::with_version("1.7");
    let field_type = doc.add_object(Object::Name(b"Sig".to_vec()));
    let widget = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => field_type,
        "T" => "approval",
        "V" => Object::string_literal("SIG-INDIRECT-TYPE"),
        "Rect" => vec![0.into(), 0.into(), 0.into(), 0.into()],
    });
    let bytes = two_page_file(0, widget, &mut doc);
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();
    pdf.delete_pages(&[0]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        saved
            .windows(b"SIG-INDIRECT-TYPE".len())
            .any(|w| w == b"SIG-INDIRECT-TYPE"),
        "an invisible signature was dropped because its type was not read"
    );
}
