//! What saving keeps and what it leaves out: resources that only removed content used, and the
//! file's own bytes read back as they were.

use carbide_engine::Pdf;
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
