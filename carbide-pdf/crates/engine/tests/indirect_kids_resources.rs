//! Deleting a page must drop images that live on a page-tree branch reached only
//! through an indirect `/Kids` array.

use carbide_engine::Pdf;
use lopdf::{dictionary, Document, Object, Stream};

const SECRET: &[u8] = b"SECRET-PAYLOAD-7f3a";

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

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn deleting_a_page_drops_images_on_a_branch_whose_parent_kids_are_indirect() {
    // The root's /Kids is a reference to its array. Opening a sound tree leaves that reference
    // as it is. The branch under the array holds the resources both pages inherit: a font the
    // kept page draws, and an image only the deleted page draws.
    let mut doc = Document::with_version("1.7");
    let root_id = doc.new_object_id();
    let branch_id = doc.new_object_id();
    let font = helvetica(&mut doc);
    let image = secret_image(&mut doc);
    let resources = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font },
        "XObject" => dictionary! { "Im1" => image },
    });
    let kept_contents = doc.add_object(Stream::new(
        lopdf::Dictionary::new(),
        b"BT /F1 12 Tf (kept) Tj ET".to_vec(),
    ));
    let gone_contents = doc.add_object(Stream::new(
        lopdf::Dictionary::new(),
        b"q 100 0 0 100 0 0 cm /Im1 Do Q".to_vec(),
    ));
    let kept = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => branch_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        "Contents" => kept_contents,
    });
    let gone = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => branch_id,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        "Contents" => gone_contents,
    });
    doc.objects.insert(
        branch_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Parent" => root_id,
            "Kids" => vec![kept.into(), gone.into()],
            "Count" => 2, "Resources" => resources,
        }),
    );
    let kids = doc.add_object(Object::Array(vec![branch_id.into()]));
    doc.objects.insert(
        root_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => 2,
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();

    let pages_id = pdf
        .document()
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    assert!(
        matches!(
            pdf.document()
                .get_dictionary(pages_id)
                .unwrap()
                .get(b"Kids")
                .unwrap(),
            Object::Reference(_)
        ),
        "opening the file inlined /Kids, so this does not exercise the walk"
    );
    assert_eq!(pdf.page_count(), 2);

    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
    assert!(
        contains(&saved, b"Helvetica"),
        "the kept page lost the font it inherited"
    );
}
