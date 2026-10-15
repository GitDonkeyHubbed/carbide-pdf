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
    let parts: Vec<Vec<&str>> = contents.iter().map(|c| vec![*c]).collect();
    let parts: Vec<&[&str]> = parts.iter().map(Vec::as_slice).collect();
    split_contents_pdf(build, &parts)
}

/// Like [`shared_resources_pdf`], with each page's content split over the streams given for it.
fn split_contents_pdf(
    build: impl FnOnce(&mut Document) -> lopdf::Dictionary,
    contents: &[&[&str]],
) -> Pdf {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let resources = build(&mut doc);
    let resources_id = doc.add_object(resources);
    let mut kids = Vec::new();
    for parts in contents {
        let mut streams: Vec<Object> = parts
            .iter()
            .map(|part| {
                Object::Reference(doc.add_object(Stream::new(
                    lopdf::Dictionary::new(),
                    part.as_bytes().to_vec(),
                )))
            })
            .collect();
        let content_id = match streams.len() {
            1 => streams.remove(0),
            _ => Object::Array(streams),
        };
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

fn form(doc: &mut Document, res: Object, content: &str) -> Object {
    Object::Reference(doc.add_object(Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Form",
        "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()], "Resources" => res },
        content.as_bytes().to_vec(),
    )))
}
fn type3(doc: &mut Document, res: Object) -> lopdf::Dictionary {
    let glyph = doc.add_object(Stream::new(
        dictionary! {},
        b"0 0 d0 0 0 10 10 re f".to_vec(),
    ));
    dictionary! {"Type" => "Font", "Subtype" => "Type3", "FontBBox" => vec![0.into(),0.into(),10.into(),10.into()],
    "FontMatrix" => vec![0.1.into(),0.into(),0.into(),0.1.into(),0.into(),0.into()],
    "CharProcs" => dictionary!{"a" => glyph}, "Encoding" => dictionary!{"Differences" => vec![97.into(), Object::Name(b"a".to_vec())]},
    "FirstChar" => 97, "LastChar" => 97, "Widths" => vec![10.into()], "Resources" => res}
}

#[test]
fn deleted_page_image_does_not_survive_two_forms_down_through_a_shared_category() {
    // The kept page draws a form whose forms are listed in a dictionary of their own; the form
    // inside has resources of its own, among them an image nothing draws.
    let mut pdf = shared_resources_pdf(
        |doc| {
            let font = helvetica(doc);
            let image = secret_image(doc);
            let inner = form(
                doc,
                Object::Dictionary(dictionary! {
                    "Font" => dictionary! { "F1" => font },
                    "XObject" => dictionary! { "ImS" => image },
                }),
                "BT /F1 9 Tf (inner) Tj ET",
            );
            let listed = doc.add_object(dictionary! { "G" => inner });
            let outer = form(
                doc,
                Object::Dictionary(dictionary! { "XObject" => listed }),
                "/G Do",
            );
            dictionary! { "XObject" => dictionary! { "F" => outer } }
        },
        &["/F Do", "BT ET"],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the unused image is still in the file"
    );
    assert!(
        contains(&saved, b"Helvetica"),
        "the inner form lost its font"
    );
}

#[test]
fn unused_image_of_a_type3_font_written_into_a_shared_font_list_does_not_survive() {
    // A form's fonts are listed in a dictionary of their own, which holds a Type 3 font written
    // straight into it; the font's resources hold an image its glyphs never draw.
    let mut pdf = shared_resources_pdf(
        |doc| {
            let image = secret_image(doc);
            let font = type3(
                doc,
                Object::Dictionary(dictionary! { "XObject" => dictionary! { "ImS" => image } }),
            );
            let listed = doc.add_object(dictionary! { "T3" => font });
            let outer = form(
                doc,
                Object::Dictionary(dictionary! { "Font" => listed }),
                "BT /T3 12 Tf (a) Tj ET",
            );
            dictionary! { "XObject" => dictionary! { "F" => outer } }
        },
        &["/F Do", "BT ET"],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the unused image is still in the file"
    );
}

#[test]
fn font_chosen_across_two_content_streams_of_a_page_is_kept() {
    // The kept page's content is split over two streams, between the font's name and the `Tf`
    // that chooses it: the page's content is the two read as one.
    let mut pdf = split_contents_pdf(
        |doc| {
            let font = helvetica(doc);
            let image = secret_image(doc);
            dictionary! {
                "Font" => dictionary! { "F1" => font },
                "XObject" => dictionary! { "Im1" => image },
            }
        },
        &[
            &["BT /F1 12 ", "Tf 10 10 Td (kept) Tj ET"],
            &["q 100 0 0 100 0 0 cm /Im1 Do Q"],
        ],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        contains(&saved, b"Helvetica"),
        "the kept page lost its font"
    );
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
}

/// A document whose first page draws `levels` forms inside one another, each with resources of
/// its own; the innermost one's hold an image nothing draws.
fn nested_forms_pdf(levels: usize) -> Pdf {
    shared_resources_pdf(
        |doc| {
            let font = helvetica(doc);
            let image = secret_image(doc);
            let mut inner = form(
                doc,
                Object::Dictionary(dictionary! {
                    "Font" => dictionary! { "F1" => font },
                    "XObject" => dictionary! { "ImS" => image },
                }),
                "BT /F1 9 Tf (inner) Tj ET",
            );
            for _ in 1..levels {
                inner = form(
                    doc,
                    Object::Dictionary(dictionary! { "XObject" => dictionary! { "G" => inner } }),
                    "/G Do",
                );
            }
            dictionary! { "XObject" => dictionary! { "F" => inner } }
        },
        &["/F Do", "BT ET"],
    )
}

#[test]
fn unused_image_deep_in_nested_forms_does_not_survive() {
    let mut pdf = nested_forms_pdf(100);
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the unused image is still in the file"
    );
    assert!(
        contains(&saved, b"Helvetica"),
        "the innermost form lost its font"
    );

    // Forms nested deeper than a page may draw them are not left untrimmed.
    let mut pdf = nested_forms_pdf(140);
    assert!(matches!(
        pdf.delete_pages(&[1]),
        Err(carbide_engine::Error::TooLarge(_))
    ));
}

#[test]
fn default_colour_spaces_stay_with_content_that_paints_in_device_colours() {
    // `rg`, `g` and `k` paint in the device colour spaces, which the page's /DefaultRGB,
    // /DefaultGray and /DefaultCMYK stand in for without the content naming them.
    let mut pdf = shared_resources_pdf(
        |doc| {
            let image = secret_image(doc);
            let profile = |doc: &mut Document, marker: &str, n: i64| {
                let stream = doc.add_object(Stream::new(
                    dictionary! { "N" => n },
                    marker.as_bytes().to_vec(),
                ));
                Object::Array(vec![Object::Name(b"ICCBased".to_vec()), stream.into()])
            };
            let rgb = profile(doc, "PROFILE-RGB", 3);
            let gray = profile(doc, "PROFILE-GRAY", 1);
            let cmyk = profile(doc, "PROFILE-CMYK", 4);
            dictionary! {
                "ColorSpace" => dictionary! {
                    "DefaultRGB" => rgb, "DefaultGray" => gray, "DefaultCMYK" => cmyk,
                },
                "XObject" => dictionary! { "Im1" => image },
            }
        },
        &[
            "1 0 0 rg 0 0 10 10 re f 0.5 g 0 10 10 10 re f 0 0 0 1 k 10 0 10 10 re f",
            "q 100 0 0 100 0 0 cm /Im1 Do Q",
        ],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    for marker in [&b"PROFILE-RGB"[..], b"PROFILE-GRAY", b"PROFILE-CMYK"] {
        assert!(
            contains(&saved, marker),
            "the kept page lost a default colour space"
        );
    }
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
}

/// The allocator, noting the largest block each thread asks for.
struct Largest;

thread_local! {
    static LARGEST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn note(size: usize) {
    let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
}

unsafe impl std::alloc::GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        note(layout.size());
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Largest = Largest;

#[test]
fn names_repeated_before_an_operator_take_no_memory_each() {
    // Four million names before one operator: 8 MB of content.
    let content = format!("{} Do", "/A".repeat(4 << 20));
    let mut pdf = shared_resources_pdf(
        |doc| {
            let image = secret_image(doc);
            dictionary! { "XObject" => dictionary! { "Im1" => image } }
        },
        &[&content, "q 100 0 0 100 0 0 cm /Im1 Do Q"],
    );
    LARGEST.with(|largest| largest.set(0));
    pdf.delete_pages(&[1]).unwrap();
    let largest = LARGEST.with(std::cell::Cell::get);
    assert!(
        largest < 4 * content.len(),
        "reading the page's names took a block of {largest} bytes"
    );
}

#[test]
fn deleted_page_image_does_not_survive_in_the_appearance_of_a_kept_annotation() {
    // The kept page has an annotation whose appearances, one for a state and a down one, use the
    // resource dictionary the pages share; only the deleted page drew its image.
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font = helvetica(&mut doc);
    let image = secret_image(&mut doc);
    let shared = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font },
        "XObject" => dictionary! { "Im1" => image },
    });
    let on = form(&mut doc, shared.into(), "BT /F1 9 Tf (on) Tj ET");
    let down = form(&mut doc, shared.into(), "BT /F1 9 Tf (down) Tj ET");
    let annotation = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Stamp",
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "AP" => dictionary! { "N" => dictionary! { "On" => on }, "D" => down },
        "AS" => "On",
    });
    let mut kids = Vec::new();
    for (content, annots) in [
        ("BT ET", vec![annotation.into()]),
        ("q 100 0 0 100 0 0 cm /Im1 Do Q", vec![]),
    ] {
        let contents = doc.add_object(Stream::new(
            lopdf::Dictionary::new(),
            content.as_bytes().to_vec(),
        ));
        kids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            "Contents" => contents, "Resources" => shared, "Annots" => annots,
        })));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 2 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();

    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
    assert!(
        contains(&saved, b"Helvetica"),
        "the annotation's appearances lost their font"
    );
}

/// A document whose first page draws `names` different forms, none of them there, and whose
/// second page draws the secret image.
fn many_names_pdf(names: usize) -> Pdf {
    let content: String = (0..names).map(|i| format!("/N{i} Do ")).collect();
    shared_resources_pdf(
        |doc| {
            let image = secret_image(doc);
            dictionary! { "XObject" => dictionary! { "Im1" => image } }
        },
        &[&content, "q 100 0 0 100 0 0 cm /Im1 Do Q"],
    )
}

#[test]
fn a_page_naming_too_many_different_resources_is_refused() {
    // Each name is kept while the page is read: 200,000 different ones are refused rather than
    // held, however small the content that names them.
    let mut pdf = many_names_pdf(200_000);
    LARGEST.with(|largest| largest.set(0));
    assert!(matches!(
        pdf.delete_pages(&[1]),
        Err(carbide_engine::Error::TooLarge(_))
    ));
    let largest = LARGEST.with(std::cell::Cell::get);
    assert!(
        largest < 8 << 20,
        "reading the page's names took a block of {largest} bytes"
    );

    // A page naming a few thousand is read as before.
    let mut pdf = many_names_pdf(5_000);
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
}

#[test]
fn colour_spaces_a_transparency_group_blends_in_stay_with_it() {
    // The form and the page each blend in a colour space their own resources name, which their
    // content never selects.
    let blend = |white: f32| {
        Object::Array(vec![
            Object::Name(b"CalRGB".to_vec()),
            Object::Dictionary(dictionary! {
                "WhitePoint" => vec![white.into(), 1.into(), 1.0889.into()],
            }),
        ])
    };
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let grouped = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Group" => dictionary! { "S" => "Transparency", "CS" => "FormBlend" },
            "Resources" => dictionary! {
                "ColorSpace" => dictionary! { "FormBlend" => blend(0.9505) },
            },
        },
        b"0 0 1 rg 0 0 10 10 re f".to_vec(),
    ));
    let resources = doc.add_object(dictionary! {
        "XObject" => dictionary! { "Fm1" => grouped },
        "ColorSpace" => dictionary! { "PageBlend" => blend(0.9642) },
    });
    let mut kids = Vec::new();
    for content in ["/Fm1 Do", "BT ET"] {
        let contents = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        kids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            "Contents" => contents, "Resources" => resources,
            "Group" => dictionary! { "S" => "Transparency", "CS" => "PageBlend" },
        })));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 2 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        contains(&saved, b"0.9505"),
        "the form's blending space was dropped"
    );
    assert!(
        contains(&saved, b"0.9642"),
        "the page's blending space was dropped"
    );
}

#[test]
fn deleted_page_image_does_not_survive_in_a_soft_mask_form() {
    // The kept page paints through a graphics state whose soft mask draws a form; the form's
    // resources are shared with the image only the deleted page drew.
    let mut pdf = shared_resources_pdf(
        |doc| {
            let font = helvetica(doc);
            let image = secret_image(doc);
            let shared = doc.add_object(dictionary! {
                "Font" => dictionary! { "F1" => font },
                "XObject" => dictionary! { "Im1" => image.clone() },
            });
            let mask = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form",
                    "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                    "Group" => dictionary! { "S" => "Transparency", "CS" => "DeviceGray" },
                    "Resources" => shared,
                },
                b"BT /F1 12 Tf (mask) Tj ET".to_vec(),
            ));
            dictionary! {
                "ExtGState" => dictionary! {
                    "GS1" => dictionary! {
                        "Type" => "ExtGState",
                        "SMask" => dictionary! { "Type" => "Mask", "S" => "Luminosity", "G" => mask },
                    },
                },
                "XObject" => dictionary! { "Im1" => image.clone() },
            }
        },
        &["/GS1 gs 0 0 10 10 re f", "q 100 0 0 100 0 0 cm /Im1 Do Q"],
    );
    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
    assert!(contains(&saved, b"Helvetica"), "the mask lost its font");
}

#[test]
fn deleting_a_page_does_not_copy_the_images_of_the_pages_that_stay() {
    // The page that stays draws a 16 MB image; deleting the other one need not copy it, as
    // copying the document to put it back on failure would.
    let size = 16 << 20;
    let mut pdf = shared_resources_pdf(
        |doc| {
            let image = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Image", "Width" => 2048, "Height" => 2048,
                    "ColorSpace" => "DeviceCMYK", "BitsPerComponent" => 8,
                },
                vec![0x5a; size],
            ));
            dictionary! { "XObject" => dictionary! { "Im1" => image } }
        },
        &["q 100 0 0 100 0 0 cm /Im1 Do Q", "BT ET"],
    );
    LARGEST.with(|largest| largest.set(0));
    pdf.delete_pages(&[1]).unwrap();
    let largest = LARGEST.with(std::cell::Cell::get);
    assert!(
        largest < size / 4,
        "deleting a page took a block of {largest} bytes"
    );
    assert_eq!(pdf.page_count(), 1);
    assert!(
        pdf.to_bytes().unwrap().len() > size,
        "the page that stays lost its image"
    );
}

/// Deletes the second page of a document whose first page has an annotation appearance that draws
/// a logo found only in the page's resources, and checks the logo stays. The appearance's
/// `/Resources` entry is whatever `entry` makes: none at all, or one that is no dictionary.
fn appearance_keeps_what_it_draws_from_the_page(
    entry: impl FnOnce(&mut Document) -> Option<Object>,
) {
    const LOGO: &[u8] = b"LOGO-PAYLOAD-91c2";
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut data = LOGO.to_vec();
    data.resize(3 * 8 * 8, 0);
    let logo = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 8, "Height" => 8,
            "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        },
        data,
    ));
    let image = secret_image(&mut doc);
    let shared = doc.add_object(dictionary! {
        "XObject" => dictionary! { "Logo" => logo, "Im1" => image },
    });
    let mut form = dictionary! {
        "Type" => "XObject", "Subtype" => "Form",
        "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
    };
    if let Some(resources) = entry(&mut doc) {
        form.set("Resources", resources);
    }
    let appearance = doc.add_object(Stream::new(
        form,
        b"q 100 0 0 100 0 0 cm /Logo Do Q".to_vec(),
    ));
    let annotation = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Stamp",
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "AP" => dictionary! { "N" => appearance },
    });
    let mut kids = Vec::new();
    for (content, annots) in [
        ("BT ET", vec![annotation.into()]),
        ("q 100 0 0 100 0 0 cm /Im1 Do Q", vec![]),
    ] {
        let contents = doc.add_object(Stream::new(
            lopdf::Dictionary::new(),
            content.as_bytes().to_vec(),
        ));
        kids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
            "Contents" => contents, "Resources" => shared, "Annots" => annots,
        })));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 2 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();

    pdf.delete_pages(&[1]).unwrap();
    let saved = pdf.to_bytes().unwrap();
    assert!(
        contains(&saved, LOGO),
        "the annotation's appearance lost the logo it draws"
    );
    assert!(
        !contains(&saved, SECRET),
        "the deleted page's image is still in the file"
    );
}

#[test]
fn an_appearance_without_resources_keeps_what_it_draws_from_the_page() {
    // The kept page's annotation has an appearance with no resources of its own, so the logo it
    // draws is looked up in the page's resources; the page's content never draws it.
    appearance_keeps_what_it_draws_from_the_page(|_| None);
}

#[test]
fn an_appearance_with_null_resources_keeps_what_it_draws_from_the_page() {
    // A null entry, or a reference to nothing, is the same as none.
    appearance_keeps_what_it_draws_from_the_page(|_| Some(Object::Null));
    appearance_keeps_what_it_draws_from_the_page(|doc| {
        Some(Object::Reference(doc.add_object(Object::Null)))
    });
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

#[test]
fn a_check_box_whose_state_is_a_reference_to_a_name_counts_as_cleared() {
    use carbide_engine::HiddenItem;
    // The state is stored as a reference to a name object, and the field holds no value.
    let mut pdf = one_page(|doc, catalog, page| {
        let state = doc.add_object(Object::Name(b"Yes".to_vec()));
        let check = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Btn", "T" => Object::string_literal("c"),
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()], "P" => page, "AS" => state,
        });
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Annots", vec![check.into()]);
        doc.get_dictionary_mut(catalog)
            .unwrap()
            .set("AcroForm", dictionary! { "Fields" => vec![check.into()] });
    });
    let report = pdf
        .remove_hidden_information(&[HiddenItem::FormData])
        .unwrap();
    assert_eq!(report.count(HiddenItem::FormData), 1);
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
