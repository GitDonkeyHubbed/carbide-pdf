//! Behavioural tests for the engine. Rendering checks use PDFium; set `CARBIDE_PDFIUM_LIB` or keep
//! the project's `tools/pdfium` directory in place.

use std::path::PathBuf;
use std::sync::OnceLock;

use carbide_engine::autosave::{AutoSaveStore, RowKind};
use carbide_engine::imposition::{ImpositionOptions, Margins, Media};
use carbide_engine::ocr::OcrOptions;
use carbide_engine::signature::{
    messages as sig_messages, notice_text, CertifyLevel, Detail, Identity, SignOptions, SubFilter, TimestampSource,
    TrustStore, Validity,
};
use carbide_engine::reduce::{messages as reduce_messages, reduce_files, OutputOptions, ReduceOptions};
use carbide_engine::imposition_plan::{Binding, LayoutKind, PlanOptions, PrintingOrder, Style};
use carbide_engine::{
    compare_documents, export_images, export_text, open_revision_bytes, pdf_date_now, BatesOptions, Error,
    ImageFormat, MarkupStyle, PageAnchor, Pdf, ProtectionOptions, Rect, Redaction, Renderer, WatermarkOptions,
};
use image::RgbaImage;
use lopdf::{dictionary, Document, Object, Stream};

// ---- fixtures ------------------------------------------------------------------------------

/// One renderer for the whole test process: PDFium is global state and tests run in parallel.
fn renderer() -> &'static Renderer {
    static RENDERER: OnceLock<Renderer> = OnceLock::new();
    RENDERER.get_or_init(|| {
        let lib = match std::env::var_os("CARBIDE_PDFIUM_LIB") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tools/pdfium/lib/libpdfium.dylib"),
        };
        Renderer::with_library(&lib).unwrap_or_else(|e| panic!("PDFium at {}: {e}", lib.display()))
    })
}

/// One page per entry; each entry is a list of text lines in 18 pt Helvetica.
fn text_pdf(pages: &[&[&str]]) -> Pdf {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
    });
    let mut kids = Vec::new();
    for lines in pages {
        let mut content = String::from("BT\n/F1 18 Tf\n72 700 Td\n30 TL\n");
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                content.push_str("T*\n");
            }
            content.push_str(&format!("({line}) Tj\n"));
        }
        content.push_str("ET\n");
        let content_id = doc.add_object(Stream::new(lopdf::Dictionary::new(), content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
        kids.push(Object::Reference(page_id));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids.clone(), "Count" => kids.len() as i64,
        }),
    );
    let outlines = doc.add_object(dictionary! { "Type" => "Outlines", "Count" => 0 });
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id, "Outlines" => outlines });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

fn render(r: &Renderer, pdf: &mut Pdf, page: usize) -> RgbaImage {
    let bytes = pdf.to_bytes().unwrap();
    r.render_page(&bytes, page, 72.0).unwrap()
}

/// Mean brightness (0..255) of a display-space rect of a 72 dpi render.
fn brightness(image: &RgbaImage, rect: Rect) -> f32 {
    let h = image.height() as f32;
    let (x0, x1) = (rect.x.max(0.0) as u32, (rect.max_x() as u32).min(image.width()));
    let (y0, y1) = (((h - rect.max_y()).max(0.0)) as u32, ((h - rect.y) as u32).min(image.height()));
    let mut total = 0.0f64;
    let mut n = 0.0f64;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = image.get_pixel(x, y);
            total += (p[0] as f64 + p[1] as f64 + p[2] as f64) / 3.0;
            n += 1.0;
        }
    }
    (total / n.max(1.0)) as f32
}

/// Bounding box (x0, y0, x1, y1 in image rows) of pixels that differ noticeably between two renders.
fn changed_bounds(a: &RgbaImage, b: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for y in 0..a.height() {
        for x in 0..a.width() {
            let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
            let diff = (0..3).map(|i| (p[i] as i32 - q[i] as i32).abs()).sum::<i32>();
            if diff > 60 {
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    bounds
}

fn page_text(r: &Renderer, pdf: &mut Pdf, page: usize) -> String {
    let bytes = pdf.to_bytes().unwrap();
    r.page_text(&bytes, page).unwrap()
}

// ---- page operations -----------------------------------------------------------------------

#[test]
fn merge_extract_split_delete() {
    let r = renderer();
    let a = text_pdf(&[&["A1"], &["A2"]]);
    let b = text_pdf(&[&["B1"]]);
    let mut merged = Pdf::merge(&[&a, &b]).unwrap();
    assert_eq!(merged.page_count(), 3);
    assert!(page_text(&r, &mut merged, 2).contains("B1"));
    assert!(page_text(&r, &mut merged, 0).contains("A1"));

    let parts = merged.split(&[0..=0, 1..=2]).unwrap();
    assert_eq!(parts.iter().map(Pdf::page_count).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(merged.split_every_page().unwrap().len(), 3);

    let mut picked = merged.extract_pages(&[2, 0]).unwrap();
    assert!(page_text(&r, &mut picked, 0).contains("B1"));
    assert!(page_text(&r, &mut picked, 1).contains("A1"));

    merged.delete_pages(&[0, 2]).unwrap();
    assert_eq!(merged.page_count(), 1);
    assert!(page_text(&r, &mut merged, 0).contains("A2"));
    assert!(merged.delete_pages(&[0]).is_err(), "must keep at least one page");
}

#[test]
fn rotate_insert_move() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["one"], &["two"]]);
    pdf.rotate_pages(&[0], 90).unwrap();
    pdf.rotate_pages(&[0], -180).unwrap();
    assert_eq!(pdf.page_rotation(0).unwrap(), 270);

    pdf.insert_blank_page(1, None).unwrap();
    assert_eq!(pdf.page_count(), 3);
    assert_eq!(page_text(&r, &mut pdf, 1).trim(), "");
    assert!(page_text(&r, &mut pdf, 2).contains("two"));

    pdf.move_page(2, 0).unwrap();
    let texts: Vec<String> = (0..3).map(|i| page_text(&r, &mut pdf, i)).collect();
    assert!(texts[0].contains("two"), "page order after move: {texts:?}");
    assert!(texts[1].contains("one"), "page order after move: {texts:?}");
    assert_eq!(pdf.page_rotation(1).unwrap(), 270, "rotation must travel with the page");
    assert!(pdf.move_page(5, 0).is_err());
}

#[test]
fn page_ranges_parse_and_validate() {
    assert_eq!(Pdf::parse_ranges("1-3, 5", 10).unwrap(), vec![0..=2, 4..=4]);
    for bad in ["0-2", "3-1", "1-11", "abc", "", "1-2-3"] {
        assert!(Pdf::parse_ranges(bad, 10).is_err(), "{bad:?} should be rejected");
    }
}

// ---- stamping ------------------------------------------------------------------------------

#[test]
fn watermark_is_visible_and_keeps_structure() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["Hello world"], &["Second page"]]);
    let before = render(&r, &mut pdf, 0);

    let mut options = WatermarkOptions::new("CONFIDENTIAL");
    options.pages = Some(vec![0]);
    pdf.apply_watermark(&options).unwrap();

    assert!(page_text(&r, &mut pdf, 0).contains("CONFIDENTIAL"));
    assert!(page_text(&r, &mut pdf, 0).contains("Hello world"));
    assert!(!page_text(&r, &mut pdf, 1).contains("CONFIDENTIAL"));

    let after = render(&r, &mut pdf, 0);
    let (x0, y0, x1, y1) = changed_bounds(&before, &after).expect("watermark changes pixels");
    // Diagonal watermark centered on the page: its bounds straddle the page center.
    assert!(x0 < 306 && x1 > 306 && y0 < 396 && y1 > 396, "bounds {x0},{y0} {x1},{y1}");

    // The catalog's outline entry must still be there: stamping edits objects in place.
    let catalog = pdf.document().catalog().unwrap();
    assert!(catalog.has(b"Outlines"));
}

#[test]
fn bates_numbers_are_sequential_and_positioned() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["x"], &["y"], &["z"]]);
    let options = BatesOptions { prefix: "ABC-".into(), start_number: 41, digits: 5, ..Default::default() };
    assert_eq!(options.label(0), "ABC-00041");
    pdf.apply_bates(&options).unwrap();
    for (i, expected) in ["ABC-00041", "ABC-00042", "ABC-00043"].iter().enumerate() {
        assert!(page_text(&r, &mut pdf, i).contains(expected), "page {i}");
    }
}

#[test]
fn bates_lands_in_the_displayed_bottom_right_even_on_rotated_pages() {
    let r = renderer();
    for rotation in [0, 90, 180, 270] {
        let mut pdf = text_pdf(&[&[]]);
        pdf.rotate_pages(&[0], rotation).unwrap();
        let before = render(&r, &mut pdf, 0);
        let options = BatesOptions { anchor: PageAnchor::BottomRight, font_size: 14.0, ..Default::default() };
        pdf.apply_bates(&options).unwrap();
        let after = render(&r, &mut pdf, 0);
        let (x0, y0, x1, y1) = changed_bounds(&before, &after).expect("label drawn");
        let (w, h) = (after.width(), after.height());
        assert!(x0 > w / 2 && y0 > h / 2, "rotation {rotation}: label at ({x0},{y0})-({x1},{y1}) of {w}x{h}");
        assert!(x1 <= w - 10 && y1 <= h - 10);
    }
}

#[test]
fn rotated_page_keeps_size_and_text() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["upright"]]);
    pdf.rotate_pages(&[0], 90).unwrap();
    pdf.apply_watermark(&WatermarkOptions::new("W")).unwrap();
    let image = render(&r, &mut pdf, 0);
    assert_eq!((image.width(), image.height()), (792, 612));
    assert!(page_text(&r, &mut pdf, 0).contains("upright"));
}

// ---- redaction -----------------------------------------------------------------------------

#[test]
fn redaction_removes_content_from_the_file() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["TOP SECRET 12345", "public line"], &["page two stays"]]);
    // "TOP SECRET 12345" sits on the first baseline (y=700), 18 pt tall.
    let rect = Rect::new(60.0, 692.0, 300.0, 28.0);
    pdf.apply_redactions(&r, &[Redaction { page: 0, rect }], 150.0).unwrap();

    let text = page_text(&r, &mut pdf, 0);
    assert!(!text.contains("SECRET") && !text.contains("12345"), "extractable text remains: {text:?}");

    // Nothing anywhere in the saved file may still carry the secret, decoded or not.
    let bytes = pdf.to_bytes().unwrap();
    let reloaded = Document::load_mem(&bytes).unwrap();
    for (_, object) in reloaded.objects.iter() {
        if let Object::Stream(stream) = object {
            let data = stream.get_plain_content().unwrap_or_else(|_| stream.content.clone());
            assert!(!String::from_utf8_lossy(&data).contains("SECRET"));
        }
    }
    assert!(!String::from_utf8_lossy(&bytes).contains("SECRET"));

    let image = render(&r, &mut pdf, 0);
    assert!(brightness(&image, rect.inset(4.0)) < 10.0, "redaction box should be black");
    // The unredacted line is still visible (page keeps its non-white pixels outside the box).
    assert!(brightness(&image, Rect::new(60.0, 662.0, 200.0, 28.0)) < 250.0);
    assert!(page_text(&r, &mut pdf, 1).contains("page two stays"));
}

// ---- compare, export, security -------------------------------------------------------------

#[test]
fn compare_reports_line_changes() {
    let r = renderer();
    let mut old = text_pdf(&[&["alpha", "beta"], &["same"]]);
    let mut new = text_pdf(&[&["alpha", "gamma"], &["same"], &["extra page"]]);
    let diffs = compare_documents(&r, &mut old, &mut new).unwrap();
    assert_eq!(diffs.len(), 3);
    assert_eq!(diffs[0].removed, vec!["beta"]);
    assert_eq!(diffs[0].added, vec!["gamma"]);
    assert!(diffs[1].is_identical());
    assert_eq!(diffs[2].added, vec!["extra page"]);
}

#[test]
fn export_images_and_text() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["page one"], &["page two"]]);
    let dir = std::env::temp_dir().join(format!("carbide-export-{}", std::process::id()));
    let paths = export_images(&r, &mut pdf, &dir, "doc", ImageFormat::Png, 72.0).unwrap();
    let names: Vec<_> = paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["doc-001.png", "doc-002.png"]);
    assert_eq!(image::open(&paths[0]).unwrap().width(), 612);
    assert!(export_text(&r, &mut pdf).unwrap().contains("page two"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn encryption_requires_the_password() {
    let mut pdf = text_pdf(&[&["private"]]);
    let encrypted = pdf.to_encrypted_bytes(&ProtectionOptions::new("open-sesame", "owner-pw")).unwrap();
    assert!(!String::from_utf8_lossy(&encrypted).contains("private"));
    assert!(matches!(Pdf::from_bytes(&encrypted), Err(Error::PasswordRequired)));
    assert!(matches!(Pdf::from_bytes_with_password(&encrypted, "wrong"), Err(Error::WrongPassword)));
    let mut opened = Pdf::from_bytes_with_password(&encrypted, "open-sesame").unwrap();
    assert!(page_text(&renderer(), &mut opened, 0).contains("private"));
}

#[test]
fn saving_a_protected_document_does_not_write_the_plaintext() {
    let mut pdf = text_pdf(&[&["private"]]);
    let encrypted = pdf.to_encrypted_bytes(&ProtectionOptions::new("open-sesame", "owner-pw")).unwrap();
    assert!(!encrypted.windows(7).any(|w| w == b"private"));
    let mut opened = Pdf::from_bytes_with_password(&encrypted, "open-sesame").unwrap();
    let leaked = opened.to_bytes().unwrap();
    assert!(leaked.windows(7).any(|w| w == b"private"), "the decrypted document serializes as plaintext");
    let saved = opened.bytes_for_save(Some(&encrypted), Some("open-sesame")).unwrap();
    assert!(!saved.windows(7).any(|w| w == b"private"));
    assert!(matches!(Pdf::from_bytes(&saved), Err(Error::PasswordRequired)));
    let mut again = Pdf::from_bytes_with_password(&saved, "open-sesame").unwrap();
    assert!(again.to_bytes().unwrap().windows(7).any(|w| w == b"private"));
    assert!(matches!(Pdf::from_bytes_with_password(&saved, "owner-pw"), Err(Error::WrongPassword) | Err(Error::PasswordRequired)));
}

// ---- annotations ---------------------------------------------------------------------------

#[test]
fn annotations_render_and_survive_a_round_trip() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["Highlight me please"]]);
    let line = Rect::new(72.0, 696.0, 190.0, 22.0);
    let before = render(&r, &mut pdf, 0);

    pdf.add_markup(0, &[line], MarkupStyle::Highlight, None).unwrap();
    pdf.add_note(0, 400.0, 600.0, "a note").unwrap();
    pdf.add_text_box(0, 100.0, 500.0, "Typed text", 14.0).unwrap();
    assert_eq!(pdf.annotations(0).unwrap().len(), 3);

    let mut reopened = Pdf::from_bytes(&pdf.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.annotations(0).unwrap().len(), 3);
    let after = render(&r, &mut reopened, 0);

    // Yellow highlight: blue channel drops well below red/green in the covered area.
    let probe = (line.x as u32 + 150, (792.0 - line.y - 3.0) as u32);
    let (a, b) = (before.get_pixel(probe.0, probe.1), after.get_pixel(probe.0, probe.1));
    assert!(b[2] < a[2].saturating_sub(60), "highlight not visible: {a:?} -> {b:?}");
    assert!(changed_bounds(&before, &after).is_some());
    // The typed text is stored on the annotation, and its box is drawn near the requested spot.
    let contents: Vec<String> = reopened
        .annotations(0)
        .unwrap()
        .iter()
        .filter_map(|a| a.get(b"Contents").ok()?.as_str().ok().map(|s| String::from_utf8_lossy(s).into_owned()))
        .collect();
    assert!(contents.iter().any(|c| c == "Typed text"), "contents: {contents:?}");
    let text_region = Rect::new(100.0, 470.0, 120.0, 32.0);
    assert!(brightness(&after, text_region) < brightness(&before, text_region) - 3.0, "text box not drawn");
}

#[test]
fn annotations_on_rotated_pages_stay_upright_and_in_place() {
    let r = renderer();
    let mut pdf = text_pdf(&[&[]]);
    pdf.rotate_pages(&[0], 90).unwrap();
    let before = render(&r, &mut pdf, 0);
    // Display page is 792 wide x 612 tall; put the note near the displayed top-left.
    pdf.add_note(0, 40.0, 580.0, "n").unwrap();
    let after = render(&r, &mut pdf, 0);
    let (x0, y0, x1, y1) = changed_bounds(&before, &after).expect("note drawn");
    assert!(x1 < 120 && y1 < 120, "note should be at the displayed top-left, got ({x0},{y0})-({x1},{y1})");
}

// ---- incremental updates and revisions -----------------------------------------------------

fn file_bytes(pdf: &mut Pdf) -> Vec<u8> {
    pdf.to_bytes().unwrap()
}

#[test]
fn incremental_update_appends_without_touching_the_original_bytes() {
    let r = renderer();
    let mut base = text_pdf(&[&["original text"], &["second"]]);
    let base_bytes = file_bytes(&mut base);

    let mut edited = Pdf::from_bytes(&base_bytes).unwrap();
    edited.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    let updated = edited.incremental_update(&base_bytes).unwrap();

    assert!(updated.len() > base_bytes.len());
    assert_eq!(&updated[..base_bytes.len()], &base_bytes[..], "original bytes must be untouched");
    let mut reloaded = Pdf::from_bytes(&updated).unwrap();
    assert_eq!(reloaded.page_count(), 2);
    assert!(page_text(&r, &mut reloaded, 0).contains("DRAFT"));
    assert!(page_text(&r, &mut reloaded, 1).contains("DRAFT"));
    assert!(page_text(&r, &mut reloaded, 0).contains("original text"));
}

#[test]
fn saving_keeps_signatures_and_revision_prefixes() {
    let mut doc = text_pdf(&[&["contract body"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let trust = trust_test_identity();

    let mut reopened = Pdf::from_bytes(&signed).unwrap();
    // Opening a document re-serializes it in memory. That must not count as an edit.
    let _ = reopened.to_bytes().unwrap();
    let untouched = reopened.bytes_for_save(Some(&signed), None).unwrap();
    assert_eq!(untouched, signed, "saving with no edits must not rewrite a signed file");
    let reports = verify(&untouched, &trust);
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(!reports[0].modified_after_signing);

    reopened.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    let flat = {
        let mut copy = Pdf::from_bytes(&signed).unwrap();
        copy.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
        copy.to_bytes().unwrap()
    };
    let rewritten = Pdf::verify_signatures(&flat, &trust, chrono::Utc::now()).unwrap_or_default();
    assert!(
        rewritten.first().is_none_or(|r| r.validity != Validity::Valid),
        "a full rewrite destroys the signature, which is the bug save must not hit"
    );

    let saved = reopened.bytes_for_save(Some(&signed), None).unwrap();
    assert!(saved.starts_with(&signed), "the signed bytes have to stay a prefix");
    assert!(saved.len() > signed.len());
    let reports = verify(&saved, &trust);
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(reports[0].modified_after_signing);

    let mut working = Pdf::from_bytes(&signed).unwrap();
    working.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    let v1 = working.save_revision(&signed, "First draft", &pdf_date_now()).unwrap();
    let mut working = Pdf::from_bytes(&v1).unwrap();
    working.apply_bates(&BatesOptions::default()).unwrap();
    let v2 = working.save_revision(&v1, "Second", &pdf_date_now()).unwrap();
    let revisions = Pdf::from_bytes(&v2).unwrap().revisions();
    assert!(revisions.len() >= 2);

    let mut current = Pdf::from_bytes(&v2).unwrap();
    current.apply_watermark(&WatermarkOptions::new("FINAL")).unwrap();
    let saved = current.bytes_for_save(Some(&v2), None).unwrap();
    assert!(saved.starts_with(&v2));
    let after = Pdf::from_bytes(&saved).unwrap();
    let opened = open_revision_bytes(&saved, &after.revisions()[0]).unwrap();
    assert_eq!(opened.len(), after.revisions()[0].file_offset as usize);

    let mut flattened = Pdf::from_bytes(&v2).unwrap();
    flattened.apply_watermark(&WatermarkOptions::new("FINAL")).unwrap();
    let flat_bytes = flattened.to_bytes().unwrap();
    let flat_revisions = Pdf::from_bytes(&flat_bytes).unwrap().revisions();
    assert!(
        open_revision_bytes(&flat_bytes, &flat_revisions[0]).is_err(),
        "rewriting the file makes the stored revision offset unusable"
    );
}

#[test]
fn revisions_are_prefixes_of_the_file_and_open_as_they_were() {
    let r = renderer();
    let mut doc = text_pdf(&[&["contract body"]]);
    let v0 = file_bytes(&mut doc);

    // Revision 1: add a watermark and label it.
    let mut working = Pdf::from_bytes(&v0).unwrap();
    assert!(working.revisions().is_empty());
    working.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    assert!(working.save_revision(&v0, "   ", &pdf_date_now()).is_err(), "blank labels are rejected");
    let v1 = working.save_revision(&v0, "First draft", &pdf_date_now()).unwrap();

    let after_one = Pdf::from_bytes(&v1).unwrap();
    let list = after_one.revisions();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].label, "First draft");
    assert_eq!(list[0].file_offset as usize, v1.len(), "length records the file size at that point");
    assert!(!list[0].display_time().is_empty());
    assert!(!after_one.can_open_revision(), "needs two dated revisions");

    // Revision 2: number the pages and label it with non-ASCII text.
    let mut working = Pdf::from_bytes(&v1).unwrap();
    working.apply_bates(&BatesOptions { prefix: "REV-".into(), ..Default::default() }).unwrap();
    let v2 = working.save_revision(&v1, "Überarbeitung 2", &pdf_date_now()).unwrap();
    assert_eq!(&v2[..v1.len()], &v1[..], "earlier revisions stay byte-for-byte intact");

    let after_two = Pdf::from_bytes(&v2).unwrap();
    let list = after_two.revisions();
    assert_eq!(list.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), vec!["First draft", "Überarbeitung 2"]);
    assert!(after_two.can_open_revision());
    assert_eq!(list[0].file_offset as usize, v1.len());
    assert_eq!(list[1].file_offset as usize, v2.len());

    // Opening revision 1 from the latest file shows the document without the later edits.
    let first = open_revision_bytes(&v2, &list[0]).unwrap();
    assert_eq!(first, v1);
    let mut first_doc = Pdf::from_bytes(&first).unwrap();
    assert!(page_text(&r, &mut first_doc, 0).contains("DRAFT"));
    assert!(!page_text(&r, &mut first_doc, 0).contains("REV-"));

    let mut latest = Pdf::from_bytes(&open_revision_bytes(&v2, &list[1]).unwrap()).unwrap();
    assert!(page_text(&r, &mut latest, 0).contains("REV-000001"));
    assert!(page_text(&r, &mut latest, 0).contains("DRAFT"));

    // A zero or oversized position does nothing useful.
    let mut broken = list[0].clone();
    broken.file_offset = 0;
    assert!(open_revision_bytes(&v2, &broken).is_err());
    broken.file_offset = v2.len() as u64 + 10;
    assert!(open_revision_bytes(&v2, &broken).is_err());
}
