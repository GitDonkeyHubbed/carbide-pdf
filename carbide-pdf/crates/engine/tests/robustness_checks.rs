//! Tests of how the engine copes with documents made to exhaust it, and of what a refused
//! operation leaves behind. Rendering checks use PDFium; set `CARBIDE_PDFIUM_LIB` or keep the
//! project's `tools/pdfium` directory in place.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use carbide_engine::ocr::OcrOptions;
use carbide_engine::reduce::ReduceOptions;
use carbide_engine::{Error, Pdf, Rect, Redaction, Renderer};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

// ---- fixtures ------------------------------------------------------------------------------

/// One renderer for the whole test process: PDFium is global state and tests run in parallel.
fn renderer() -> &'static Renderer {
    static RENDERER: OnceLock<Renderer> = OnceLock::new();
    RENDERER.get_or_init(|| {
        let lib = match std::env::var_os("CARBIDE_PDFIUM_LIB") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../tools/pdfium/lib/libpdfium.dylib"),
        };
        Renderer::with_library(&lib).unwrap_or_else(|e| panic!("PDFium at {}: {e}", lib.display()))
    })
}

fn ocr_models() -> PathBuf {
    match std::env::var_os("CARBIDE_OCR_MODELS") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tools/ocr-models"),
    }
}

/// One page for each entry, with that line of text on it.
fn text_pdf(pages: &[&str]) -> Pdf {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
    });
    let mut kids = Vec::new();
    for line in pages {
        let content = format!("BT\n/F1 18 Tf\n72 700 Td\n({line}) Tj\nET\n");
        let content_id = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
        kids.push(Object::Reference(page));
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

/// A form XObject, with `resources` of its own when given.
fn form(pdf: &mut Pdf, resources: Option<Dictionary>, content: &[u8]) -> ObjectId {
    let mut dict = dictionary! {
        "Type" => "XObject", "Subtype" => "Form",
        "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
    };
    if let Some(resources) = resources {
        dict.set("Resources", resources);
    }
    pdf.document_mut()
        .add_object(Stream::new(dict, content.to_vec()))
}

/// A form drawing `inner` as `/Next`, with resources of its own.
fn wrapping(pdf: &mut Pdf, inner: ObjectId) -> ObjectId {
    form(
        pdf,
        Some(dictionary! { "XObject" => dictionary! { "Next" => inner } }),
        b"/Next Do",
    )
}

/// A chain of `levels` forms, each drawing the one before, ending in an empty one.
fn chain(pdf: &mut Pdf, levels: usize) -> ObjectId {
    let mut inner = form(pdf, None, b"");
    for _ in 0..levels {
        inner = wrapping(pdf, inner);
    }
    inner
}

/// Makes the first page draw `content`, with `target` as its form `/F`.
fn page_drawing(pdf: &mut Pdf, content: &[u8], target: ObjectId) {
    let page = pdf.page_id(0).unwrap();
    let content = pdf
        .document_mut()
        .add_object(Stream::new(Dictionary::new(), content.to_vec()));
    let page = pdf.document_mut().get_dictionary_mut(page).unwrap();
    page.set("Contents", Object::Reference(content));
    page.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "F" => target } },
    );
}

fn blank_page_pdf() -> Pdf {
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((100.0, 100.0))).unwrap();
    pdf
}

/// A form that draws itself.
fn self_drawing_form(pdf: &mut Pdf) -> ObjectId {
    let id = form(pdf, None, b"/F Do");
    pdf.document_mut()
        .get_object_mut(id)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "F" => id } },
        );
    id
}

// ---- nesting of forms ----------------------------------------------------------------------

#[test]
fn forms_nested_thousands_deep_are_refused_instead_of_overflowing_the_stack() {
    // Run in a child process: overflowing the stack ends the process, not just a test.
    if std::env::var_os("CARBIDE_TEST_DEEP_FORMS").is_some() {
        let mut pdf = blank_page_pdf();
        let outermost = chain(&mut pdf, 20_000);
        page_drawing(&mut pdf, b"/F Do", outermost);
        assert!(matches!(pdf.check_drawable(0), Err(Error::TooLarge(_))));
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "forms_nested_thousands_deep_are_refused_instead_of_overflowing_the_stack",
            "--nocapture",
        ])
        .env("CARBIDE_TEST_DEEP_FORMS", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "checking the page failed: {} {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(400)
            .collect::<String>()
    );
}

#[test]
fn forms_nested_as_real_documents_do_are_drawable() {
    let mut pdf = blank_page_pdf();
    let outermost = chain(&mut pdf, 100);
    page_drawing(&mut pdf, b"/F Do", outermost);
    pdf.check_drawable(0).unwrap();
}

#[test]
fn a_form_found_again_is_as_deep_as_it_was_where_it_was_first_drawn() {
    // A form with 100 forms inside it is drawn first on its own, then 40 forms down, which makes
    // 140 forms nested, though the page is only walked into the second place as far as the first.
    let mut pdf = blank_page_pdf();
    let deep = chain(&mut pdf, 100);
    let mut below = deep;
    for _ in 0..40 {
        below = wrapping(&mut pdf, below);
    }
    let page = pdf.page_id(0).unwrap();
    let content = pdf.document_mut().add_object(Stream::new(
        Dictionary::new(),
        b"/Deep Do /Below Do".to_vec(),
    ));
    let page = pdf.document_mut().get_dictionary_mut(page).unwrap();
    page.set("Contents", Object::Reference(content));
    page.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "Deep" => deep, "Below" => below } },
    );
    assert!(matches!(pdf.check_drawable(0), Err(Error::TooLarge(_))));
}

// ---- inline images -------------------------------------------------------------------------

#[test]
fn a_stray_id_does_not_hide_the_forms_drawn_after_it() {
    let mut pdf = blank_page_pdf();
    let f = self_drawing_form(&mut pdf);
    page_drawing(&mut pdf, b"ID /F Do", f);
    assert!(
        matches!(pdf.check_drawable(0), Err(Error::TooLarge(_))),
        "a form drawing itself went unnoticed after a stray ID"
    );
}

#[test]
fn what_follows_a_real_inline_image_is_still_read() {
    let mut pdf = blank_page_pdf();
    let f = self_drawing_form(&mut pdf);
    page_drawing(
        &mut pdf,
        b"q BI /W 1 /H 1 /CS /G /BPC 8 ID \x00 EI Q /F Do",
        f,
    );
    assert!(matches!(pdf.check_drawable(0), Err(Error::TooLarge(_))));
    // Drawing something that looks like a form inside the image data is not drawing it.
    let mut pdf = blank_page_pdf();
    let f = self_drawing_form(&mut pdf);
    page_drawing(
        &mut pdf,
        b"BI /W 2 /H 1 /CS /G /BPC 8 ID /F Do EI",
        f,
    );
    pdf.check_drawable(0).unwrap();
}

// ---- deleting pages ------------------------------------------------------------------------

/// Zlib data that decodes to `megabytes` MB of zeros.
fn zeros_deflated(megabytes: usize) -> Vec<u8> {
    let mut encoder =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let block = vec![0u8; 1 << 20];
    for _ in 0..megabytes {
        encoder.write_all(&block).unwrap();
    }
    encoder.finish().unwrap()
}

#[test]
fn deleting_pages_that_cannot_be_finished_leaves_the_document_as_it_was() {
    let mut pdf = text_pdf(&["first", "second"]);
    // The page that stays has content too large to read, so the resources cannot be trimmed.
    let ids = pdf.page_ids();
    let bomb = pdf.document_mut().add_object(Stream::new(
        dictionary! { "Filter" => "FlateDecode" },
        zeros_deflated(300),
    ));
    pdf.document_mut()
        .get_dictionary_mut(ids[1])
        .unwrap()
        .set("Contents", Object::Reference(bomb));
    let before = pdf.document().objects.clone();

    let refused = pdf.delete_pages(&[0]);
    assert!(matches!(refused, Err(Error::TooLarge(_))), "{refused:?}");
    assert_eq!(pdf.page_count(), 2, "a page was deleted though it failed");
    assert_eq!(
        pdf.document().objects,
        before,
        "a refused deletion changed the document"
    );
}

// ---- areas ---------------------------------------------------------------------------------

#[test]
fn an_area_that_overflows_once_normalized_is_not_finite() {
    assert!(Rect::new(1.0, 2.0, 3.0, 4.0).is_finite());
    assert!(Rect::new(1.0, 2.0, -3.0, -4.0).is_finite());
    assert!(!Rect::new(f32::MAX, 0.0, f32::MAX, 10.0).is_finite());
    assert!(!Rect::new(-f32::MAX, 0.0, -f32::MAX, 10.0).is_finite());
    assert!(!Rect::new(0.0, f32::MAX, 10.0, f32::MAX).is_finite());
    assert!(!Rect::new(f32::NAN, 0.0, 1.0, 1.0).is_finite());
}

#[test]
fn redaction_refuses_an_area_whose_far_corner_overflows() {
    let mut pdf = text_pdf(&["SECRET"]);
    let before = pdf.document().objects.clone();
    for rect in [
        Rect::new(f32::MAX, 0.0, f32::MAX, 10.0),
        Rect::new(0.0, -f32::MAX, 10.0, -f32::MAX),
    ] {
        let refused = pdf.apply_redactions(renderer(), &[Redaction { page: 0, rect }], 150.0);
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.to_string().contains("redaction area")),
            "{refused:?}"
        );
    }
    assert_eq!(pdf.document().objects, before);
}

