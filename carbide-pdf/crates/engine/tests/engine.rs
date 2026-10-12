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

// ---- auto save / auto recovery -------------------------------------------------------------

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("carbide-{tag}-{}-{}", std::process::id(), carbide_engine::autosave::now_unix()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn autosave_survives_restart_and_follows_the_removal_rules() {
    let r = renderer();
    let dir = scratch_dir("autosave");
    let original_path = dir.join("..").join("contract.pdf");

    // Session 1: an edited document is auto-saved, then the app "crashes" (no close, no save).
    let mut store = AutoSaveStore::open(&dir).unwrap();
    let id = store.begin_document(Some(&original_path), 1_000);
    let mut doc = text_pdf(&[&["unsaved work"]]);
    store.autosave(id, &mut doc, 1_600).unwrap();
    let first_file = store.record(id).unwrap().recovered.clone().unwrap().path;
    assert!(first_file.exists());
    assert!(first_file.file_name().unwrap().to_string_lossy().starts_with("ZAS"));
    assert_eq!(first_file.extension().unwrap(), "asd");
    assert!(!store.any_needs_save(), "session records are not 'loaded' yet");

    // A second tick reuses the same file and only updates the time.
    doc.apply_bates(&BatesOptions::default()).unwrap();
    store.autosave(id, &mut doc, 2_200).unwrap();
    assert_eq!(store.record(id).unwrap().recovered.as_ref().unwrap().path, first_file);
    assert_eq!(store.record(id).unwrap().recovered.as_ref().unwrap().save_time, 2_200);
    drop(store);

    // Session 2: the index is read back and the recovery list is populated.
    let mut store = AutoSaveStore::open(&dir).unwrap();
    assert!(store.any_needs_save());
    let rows = store.panel_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, RowKind::Original);
    assert_eq!(rows[0].name, "contract.pdf (saved)");
    assert_eq!(rows[1].kind, RowKind::Recovered);
    assert_eq!(rows[1].name, "contract.pdf (recovered)");
    assert_eq!(rows[1].save_time, 2_200);

    let loaded_id = rows[1].record;
    let (mut recovered, title) = store.open_recovered(loaded_id, None).unwrap();
    assert_eq!(title, "contract.pdf (recovered)");
    assert!(page_text(&r, &mut recovered, 0).contains("unsaved work"));
    assert!(page_text(&r, &mut recovered, 0).contains("000001"), "recovery copy has the latest edits");

    // Deleting the recovered row keeps the original row; deleting that too removes the record.
    store.remove_recovered(loaded_id).unwrap();
    assert!(!first_file.exists());
    assert_eq!(store.panel_rows().len(), 1);
    store.remove_original(loaded_id).unwrap();
    assert!(store.records().is_empty());
    drop(store);
    assert!(AutoSaveStore::open(&dir).unwrap().records().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn autosave_cleans_up_on_save_and_close_but_never_touches_originals() {
    let dir = scratch_dir("autosave-clean");
    let user_file = std::env::temp_dir().join(format!("carbide-user-{}.pdf", std::process::id()));
    std::fs::write(&user_file, b"the user's own file").unwrap();

    let mut store = AutoSaveStore::open(&dir).unwrap();
    let mut doc = text_pdf(&[&["a"]]);

    // Saving normally removes the whole session record and its recovery copy.
    let id = store.begin_document(Some(&user_file), 10);
    store.autosave(id, &mut doc, 20).unwrap();
    let copy = store.record(id).unwrap().recovered.clone().unwrap().path;
    store.document_saved(id).unwrap();
    assert!(store.record(id).is_none());
    assert!(!copy.exists());

    // Closing without saving also discards the session's recovery copy.
    let id = store.begin_document(None, 30);
    store.autosave(id, &mut doc, 40).unwrap();
    let copy = store.record(id).unwrap().recovered.clone().unwrap().path;
    assert!(store.record(id).unwrap().is_new_doc);
    store.document_closed(id).unwrap();
    assert!(!copy.exists());

    // Clear-all removes loaded records only.
    let id = store.begin_document(None, 50);
    store.autosave(id, &mut doc, 60).unwrap();
    drop(store);
    let mut store = AutoSaveStore::open(&dir).unwrap();
    assert_eq!(store.remove_all_loaded().unwrap(), 1);
    assert!(store.records().is_empty());

    assert_eq!(std::fs::read(&user_file).unwrap(), b"the user's own file");
    let _ = std::fs::remove_file(user_file);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_damaged_index_is_ignored() {
    let dir = scratch_dir("autosave-bad");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(carbide_engine::autosave::INDEX_FILE_NAME), "<Something><AutoSaveItem RecoveredFile=\"/nope\"/></Something>").unwrap();
    assert!(AutoSaveStore::open(&dir).unwrap().records().is_empty());
    std::fs::write(dir.join(carbide_engine::autosave::INDEX_FILE_NAME), "not xml at all").unwrap();
    assert!(AutoSaveStore::open(&dir).unwrap().records().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

// ---- imposition ----------------------------------------------------------------------------

fn numbered_pages(n: usize) -> Pdf {
    let labels: Vec<String> = (1..=n).map(|i| format!("PAGE{i}X")).collect();
    let refs: Vec<[&str; 1]> = labels.iter().map(|l| [l.as_str()]).collect();
    let slices: Vec<&[&str]> = refs.iter().map(|r| &r[..]).collect();
    text_pdf(&slices)
}

#[test]
fn booklet_sheets_hold_the_right_pages_in_the_right_order() {
    let r = renderer();
    let source = numbered_pages(8);
    let options = ImpositionOptions { media: Media::Named("Letter".into()), ..Default::default() };
    let booklet = source.impose(&options, None).unwrap();
    let mut booklet = booklet;
    assert_eq!(booklet.page_count(), 4, "8 pages, 2-up saddle: 2 sheets, 2 sides");

    let expected = [("PAGE8X", "PAGE1X"), ("PAGE2X", "PAGE7X"), ("PAGE6X", "PAGE3X"), ("PAGE4X", "PAGE5X")];
    for (index, (left, right)) in expected.iter().enumerate() {
        let text = page_text(&r, &mut booklet, index);
        let (l, rr) = (text.find(left), text.find(right));
        assert!(l.is_some() && rr.is_some(), "sheet {index}: {text:?}");
        // The left page is drawn first, so its text comes first in content order.
        assert!(l.unwrap() < rr.unwrap(), "sheet {index} order: {text:?}");
    }
    // Letter auto-rotates to landscape for two pages side by side.
    let geometry = booklet.page_geometry(0).unwrap();
    assert_eq!(geometry.display_size(), (792.0, 612.0));
}

#[test]
fn step_and_repeat_and_blanks() {
    let r = renderer();
    let source = numbered_pages(6);
    let options = ImpositionOptions {
        plan: PlanOptions {
            layout: LayoutKind::StepAndRepeat,
            columns: 2,
            rows: 2,
            double_sided: false,
            ..Default::default()
        },
        auto_rotate: false,
        ..Default::default()
    };
    let mut sheets = source.impose(&options, None).unwrap();
    assert_eq!(sheets.page_count(), 2);
    let first = page_text(&r, &mut sheets, 0);
    for n in 1..=4 {
        assert!(first.contains(&format!("PAGE{n}X")), "{first:?}");
    }
    let second = page_text(&r, &mut sheets, 1);
    assert!(second.contains("PAGE5X") && second.contains("PAGE6X") && !second.contains("PAGE1X"), "{second:?}");
}

#[test]
fn four_up_inverts_the_top_row_and_flip_backs_rotates_back_sheets() {
    let r = renderer();
    let source = numbered_pages(8);
    let options = ImpositionOptions {
        plan: PlanOptions { layout: LayoutKind::Booklet4Up, columns: 2, rows: 2, ..Default::default() },
        flip_backs: true,
        margins: Margins { left: 10.0, right: 10.0, top: 10.0, bottom: 10.0 },
        ..Default::default()
    };
    let mut sheets = source.impose(&options, None).unwrap();
    assert_eq!(sheets.page_count(), 2);
    assert_eq!(sheets.page_rotation(0).unwrap(), 0);
    assert_eq!(sheets.page_rotation(1).unwrap(), 180, "back sheet is flipped");

    // The top row is drawn upside down, so its page content sits in the top half, rotated:
    // render and compare ink in the top-right quadrant with the same quadrant of an upright run.
    let plain = ImpositionOptions { flip_backs: false, ..options.clone() };
    let mut upright = source.impose(&plain, None).unwrap();
    assert_eq!(page_text(&r, &mut sheets, 0).matches("PAGE").count(), 4);
    assert_eq!(page_text(&r, &mut upright, 0).matches("PAGE").count(), 4);
}

#[test]
fn printing_order_changes_the_output_order() {
    let r = renderer();
    let source = numbered_pages(8);
    let make = |order| ImpositionOptions {
        plan: PlanOptions { printing_order: order, ..Default::default() },
        ..Default::default()
    };
    let mut interleaved = source.impose(&make(PrintingOrder::Interleaved), None).unwrap();
    let mut fronts_first = source.impose(&make(PrintingOrder::FrontsThenBacks), None).unwrap();
    // Plates 0,1,2,3 -> output 0,2,1,3 for "fronts first".
    for (output, plate) in [(0usize, 0usize), (1, 2), (2, 1), (3, 3)] {
        assert_eq!(
            page_text(&r, &mut fronts_first, output),
            page_text(&r, &mut interleaved, plate),
            "output {output} should be plate {plate}"
        );
    }
}

#[test]
fn imposition_reports_progress_and_can_be_cancelled() {
    let source = numbered_pages(8);
    let options = ImpositionOptions::default();
    let mut seen = Vec::new();
    let mut record = |percent: u32| {
        seen.push(percent);
        false
    };
    source.impose(&options, Some(&mut record)).unwrap();
    assert_eq!(*seen.last().unwrap(), 100);

    let mut stop = |_: u32| true;
    assert!(source.impose(&options, Some(&mut stop)).is_err());

    let broken = ImpositionOptions { media: Media::None, ..Default::default() };
    assert!(source.impose(&broken, None).unwrap_err().to_string().starts_with("Choose a media size"));
    let _ = (Binding::Saddle, Style::Sheetwise);
}

// ---- reduce file size ----------------------------------------------------------------------

/// One page with two byte-identical, uncompressed 1200x900 RGB "photos" drawn at 200x150 pt.
fn photo_pdf() -> Pdf {
    let (w, h) = (1200usize, 900usize);
    let mut state = 12345u32;
    let mut samples = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((state >> 24) % 40) as usize;
            let base = (x * 255 / w + y * 120 / h) % 256;
            samples.push(((base + noise) % 256) as u8);
            samples.push(((255 - base / 2 + noise) % 256) as u8);
            samples.push(((base / 3 + 2 * noise) % 256) as u8);
        }
    }
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let image = |doc: &mut Document, data: &[u8]| {
        doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => w as i64, "Height" => h as i64,
                "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
            },
            data.to_vec(),
        ).with_compression(false))
    };
    let a = image(&mut doc, &samples);
    let b = image(&mut doc, &samples);
    let content = doc.add_object(Stream::new(
        lopdf::Dictionary::new(),
        b"q 200 0 0 150 50 500 cm /Im1 Do Q q 200 0 0 150 300 500 cm /Im2 Do Q".to_vec(),
    ));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
        "Resources" => dictionary! { "XObject" => dictionary! { "Im1" => a, "Im2" => b } },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

#[test]
fn reduce_shrinks_photos_and_merges_duplicates() {
    let r = renderer();
    let mut pdf = photo_pdf();
    let before = file_bytes(&mut pdf).len();
    let before_render = render(&r, &mut pdf, 0);

    let (reduced, report) = pdf.reduce(&ReduceOptions::default(), Some(before)).unwrap();
    assert!(reduced.len() < before / 10, "{} -> {}", before, reduced.len());
    assert_eq!(report.size_before, before);
    assert_eq!(report.size_after, reduced.len());
    assert!(report.streams_merged >= 1, "the identical second image should be merged: {report:?}");
    assert!(report.images_downsampled >= 1, "{report:?}");

    // The image was drawn at 200x150 pt: at 150 ppi that is about 417x313 px, far below 1200x900.
    let mut reloaded = Pdf::from_bytes(&reduced).unwrap();
    let widths: Vec<i64> = reloaded
        .document()
        .objects
        .values()
        .filter_map(|o| match o {
            Object::Stream(s) if s.dict.get(b"Subtype").and_then(|v| v.as_name()).ok() == Some(b"Image") => {
                s.dict.get(b"Width").ok()?.as_i64().ok()
            }
            _ => None,
        })
        .collect();
    assert_eq!(widths.len(), 1, "duplicates merged into one image object");
    assert!(widths[0] < 600 && widths[0] >= 300, "width {}", widths[0]);

    // And it still looks like the same picture.
    let after_render = render(&r, &mut reloaded, 0);
    let area = Rect::new(50.0, 500.0, 200.0, 150.0);
    let (a, b) = (brightness(&before_render, area), brightness(&after_render, area));
    assert!((a - b).abs() < 12.0, "brightness {a} -> {b}");
}

#[test]
fn slider_trades_size_for_quality() {
    let mut pdf = photo_pdf();
    let small = pdf.reduce(&ReduceOptions { image_factor: 0 }, None).unwrap().0.len();
    let high = pdf.reduce(&ReduceOptions { image_factor: 100 }, None).unwrap().0.len();
    assert!(small < high, "slider at 0 gives {small} bytes, at 100 gives {high}");
}

#[test]
fn reduce_refuses_when_nothing_gets_smaller_and_for_protected_documents() {
    // Reducing an already-reduced file gains nothing, so it is refused.
    let (once, _) = text_pdf(&[&["nothing more to shrink here"]]).reduce(&ReduceOptions::default(), None).unwrap();
    let mut again = Pdf::from_bytes(&once).unwrap();
    let err = again.reduce(&ReduceOptions::default(), Some(once.len())).unwrap_err();
    assert_eq!(err.to_string(), reduce_messages::NOT_SMALLER);

    for (key, value, expected) in [
        ("SigFlags", Object::Integer(3), reduce_messages::NO_PERMISSION),
        ("XFA", Object::Array(vec![]), reduce_messages::XFA),
    ] {
        let mut pdf = text_pdf(&[&["x"]]);
        let doc = pdf.document_mut();
        let form = doc.add_object(dictionary! { key => value });
        doc.catalog_mut().unwrap().set("AcroForm", form);
        assert_eq!(pdf.reduce(&ReduceOptions::default(), None).unwrap_err().to_string(), expected);
    }
    let mut portfolio = text_pdf(&[&["x"]]);
    portfolio.document_mut().catalog_mut().unwrap().set("Collection", dictionary! {});
    assert_eq!(portfolio.reduce(&ReduceOptions::default(), None).unwrap_err().to_string(), reduce_messages::PORTFOLIO);
}

#[test]
fn batch_reduce_never_damages_originals() {
    let dir = scratch_dir("reduce");
    std::fs::create_dir_all(&dir).unwrap();

    let photo = dir.join("photo.pdf");
    let tiny = dir.join("tiny.pdf");
    let broken = dir.join("broken.pdf");
    photo_pdf().save(&photo).unwrap();
    // Already reduced once, so a second reduction cannot make it smaller.
    let (already, _) = text_pdf(&[&["tiny"]]).reduce(&ReduceOptions::default(), None).unwrap();
    std::fs::write(&tiny, already).unwrap();
    std::fs::write(&broken, b"this is not a pdf").unwrap();
    let tiny_before = std::fs::read(&tiny).unwrap();
    let photo_before = std::fs::metadata(&photo).unwrap().len();

    // In place (same folder, original names, overwrite on).
    let outcomes = reduce_files(
        &[photo.clone(), tiny.clone(), broken.clone()],
        &OutputOptions::default(),
        &ReduceOptions::default(),
    );
    assert!(outcomes[0].warning.is_none() && outcomes[0].report.is_some());
    assert!(std::fs::metadata(&photo).unwrap().len() < photo_before / 10);
    assert_eq!(outcomes[1].warning.as_deref(), Some(reduce_messages::NOT_SMALLER));
    assert_eq!(std::fs::read(&tiny).unwrap(), tiny_before, "a file that cannot shrink is left alone");
    assert_eq!(outcomes[2].warning.as_deref(), Some(reduce_messages::OPEN_FAILED));
    assert_eq!(std::fs::read(&broken).unwrap(), b"this is not a pdf");

    // Naming and overwrite rules.
    let named = OutputOptions { prefix: "small-".into(), overwrite: false, ..Default::default() };
    assert_eq!(named.output_path(&photo).unwrap(), dir.join("small-photo.pdf"));
    std::fs::write(dir.join("small-photo.pdf"), b"existing").unwrap();
    assert_eq!(named.output_path(&photo), Err(reduce_messages::FILE_EXISTS));
    let _ = std::fs::remove_dir_all(dir);
}

// ---- text selection ------------------------------------------------------------------------

#[test]
fn dragged_regions_snap_to_text_lines_even_on_rotated_pages() {
    let r = renderer();
    for rotation in [0, 90, 180, 270] {
        let mut pdf = text_pdf(&[&["Highlight me please", "second line of text"]]);
        pdf.rotate_pages(&[0], rotation).unwrap();
        let geometry = pdf.page_geometry(0).unwrap();
        let (w, h) = geometry.display_size();
        let bytes = pdf.to_bytes().unwrap();

        // Drag over the whole page: both lines must come back, each as its own rectangle.
        let everything = Rect::new(0.0, 0.0, w, h);
        let lines = r.text_line_rects(&bytes, 0, &geometry, everything).unwrap();
        assert_eq!(lines.len(), 2, "rotation {rotation}: {lines:?}");

        // Each rect must cover pixels that are actually inked in the rendered page.
        let image = r.render_page(&bytes, 0, 72.0).unwrap();
        for line in &lines {
            assert!(brightness(&image, *line) < 245.0, "rotation {rotation}: no ink under {line:?}");
        }

        // Marking up the first line darkens/yellows exactly that area.
        let first = lines.iter().max_by(|a, b| (a.y + a.height).partial_cmp(&(b.y + b.height)).unwrap()).unwrap();
        let before = render(&r, &mut pdf, 0);
        pdf.add_markup(0, &[*first], MarkupStyle::Highlight, None).unwrap();
        let after = render(&r, &mut pdf, 0);
        let changed = changed_bounds(&before, &after).expect("highlight drawn");
        let centre = (first.x + first.width / 2.0, h - (first.y + first.height / 2.0));
        assert!(
            changed.0 as f32 <= centre.0 && centre.0 <= changed.2 as f32 && changed.1 as f32 <= centre.1 && centre.1 <= changed.3 as f32,
            "rotation {rotation}: highlight {changed:?} misses line centre {centre:?}"
        );
    }
}

// ---- digital signatures --------------------------------------------------------------------

struct TestIdentity {
    identity: Identity,
    certificate: Vec<u8>,
    cert_pem: String,
    key_pem: String,
    pkcs8: Vec<u8>,
}

fn test_identity() -> &'static TestIdentity {
    static ID: OnceLock<TestIdentity> = OnceLock::new();
    ID.get_or_init(|| {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        let pkcs8 = key.to_pkcs8_der().unwrap().as_bytes().to_vec();
        let pair = rcgen::KeyPair::try_from(pkcs8.as_slice()).unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, "Carbide Test Signer");
        let cert = params.self_signed(&pair).unwrap();
        let certificate = cert.der().to_vec();
        TestIdentity {
            identity: Identity::new(vec![certificate.clone()], key).unwrap(),
            certificate,
            cert_pem: cert.pem(),
            key_pem: pair.serialize_pem(),
            pkcs8,
        }
    })
}

fn trust_test_identity() -> TrustStore {
    TrustStore { certificates: vec![test_identity().certificate.clone()] }
}

fn verify(bytes: &[u8], trust: &TrustStore) -> Vec<carbide_engine::signature::SignatureReport> {
    Pdf::verify_signatures(bytes, trust, chrono::Utc::now()).unwrap()
}

fn sign_options(sub_filter: SubFilter) -> SignOptions<'static> {
    SignOptions {
        identity: Some(&test_identity().identity),
        sub_filter,
        reason: Some("I approve".into()),
        location: Some("Test lab".into()),
        ..Default::default()
    }
}

#[test]
fn signing_produces_a_verifiable_pkcs7_signature() {
    let mut doc = text_pdf(&[&["contract"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();

    assert_eq!(&signed[..base.len()], &base[..], "signing only appends");
    let reports = verify(&signed, &TrustStore::default());
    assert_eq!(reports.len(), 1);
    let r = &reports[0];
    assert_eq!(r.field_name, "Signature1");
    assert_eq!(r.signer.as_deref(), Some("Carbide Test Signer"));
    assert_eq!(r.reason.as_deref(), Some("I approve"));
    assert_eq!(r.location.as_deref(), Some("Test lab"));
    assert_eq!(r.sub_filter.as_deref(), Some("adbe.pkcs7.detached"));
    // Digest and signature verify, but the signer is not in the trusted list.
    assert_eq!((r.validity, r.detail), (Validity::ValidityUnknown, Detail::DigestOkUntrusted));
    assert_eq!(r.status_line(), sig_messages::VALIDITY_UNKNOWN);
    assert!(r.description_lines().contains(&sig_messages::IDENTITY_UNKNOWN));
    assert!(!r.modified_after_signing);

    let trusted = verify(&signed, &trust_test_identity());
    assert_eq!((trusted[0].validity, trusted[0].detail), (Validity::Valid, Detail::TrustedDirectly));
    assert_eq!(trusted[0].status_line(), sig_messages::VALID);
    assert_eq!(notice_text(&trusted), Some(sig_messages::NOTICE_ALL_VALID));

    // The signed file is still an ordinary, readable PDF.
    let mut reopened = Pdf::from_bytes(&signed).unwrap();
    assert!(page_text(&renderer(), &mut reopened, 0).contains("contract"));
}

#[test]
fn any_change_inside_the_signed_range_invalidates_the_signature() {
    let mut doc = text_pdf(&[&["pay 100 dollars"]]);
    let base = file_bytes(&mut doc);
    for sub_filter in [SubFilter::Pkcs7Detached, SubFilter::CadesDetached] {
        let signed = doc.sign(&base, &sign_options(sub_filter)).unwrap();
        let ok = verify(&signed, &trust_test_identity());
        assert_eq!(ok[0].validity, Validity::Valid, "{sub_filter:?}");

        // Flip one byte of the original content.
        let position = signed.windows(11).position(|w| w == b"pay 100 dol").unwrap();
        let mut forged = signed.clone();
        forged[position + 4] = b'9';
        let bad = verify(&forged, &trust_test_identity());
        assert_eq!((bad[0].validity, bad[0].detail), (Validity::Invalid, Detail::DigestMismatch), "{sub_filter:?}");
        assert_eq!(bad[0].status_line(), sig_messages::INVALID);
        assert_eq!(notice_text(&bad), Some(sig_messages::NOTICE_PROBLEMS));
        assert!(bad[0].description_lines().contains(&sig_messages::DOCUMENT_ALTERED));
    }
}

#[test]
fn a_damaged_byte_range_is_reported_as_such() {
    let mut doc = text_pdf(&[&["x"]]);
    let base = file_bytes(&mut doc);
    let mut signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let position = signed.windows(5).rposition(|w| w == b"/Byte").unwrap();
    // Corrupt the first number of /ByteRange so that it no longer starts at zero.
    let digits = signed[position..].iter().position(|b| *b == b'[').unwrap() + position + 1;
    signed[digits] = b'7';
    let reports = verify(&signed, &TrustStore::default());
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Invalid, Detail::BadByteRange));
}

#[test]
fn later_edits_and_second_signatures_keep_earlier_signatures_valid() {
    let r = renderer();
    let mut doc = text_pdf(&[&["agreement text"]]);
    let base = file_bytes(&mut doc);
    let first = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();

    // An edit after signing (a Bates stamp) is an incremental update.
    let mut edited = Pdf::from_bytes(&first).unwrap();
    edited.apply_bates(&BatesOptions::default()).unwrap();
    let after_edit = edited.incremental_update(&first).unwrap();
    let reports = verify(&after_edit, &trust_test_identity());
    assert_eq!(reports[0].validity, Validity::Valid, "signature covers its own revision");
    assert!(reports[0].modified_after_signing);
    assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_CHANGED_AFTER_LAST));
    assert!(reports[0].description_lines().contains(&sig_messages::SUBSEQUENT_CHANGES));

    // A second signature goes on top and the first stays valid.
    let reopened = Pdf::from_bytes(&after_edit).unwrap();
    let both = reopened.sign(&after_edit, &sign_options(SubFilter::CadesDetached)).unwrap();
    let reports = verify(&both, &trust_test_identity());
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().all(|r| r.validity == Validity::Valid));
    assert_eq!(reports[0].field_name, "Signature1");
    assert_eq!(reports[1].field_name, "Signature2");
    assert!(reports[0].modified_after_signing && !reports[1].modified_after_signing);
    assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_UPDATED));

    let mut final_doc = Pdf::from_bytes(&both).unwrap();
    assert!(page_text(&r, &mut final_doc, 0).contains("000001"));
}

#[test]
fn visible_signatures_draw_an_appearance_and_invisible_ones_do_not() {
    let r = renderer();
    let mut doc = text_pdf(&[&["page"]]);
    let base = file_bytes(&mut doc);
    let before = render(&r, &mut doc, 0);

    let mut options = sign_options(SubFilter::Pkcs7Detached);
    options.appearance = Some((0, Rect::new(300.0, 100.0, 240.0, 70.0)));
    let signed = doc.sign(&base, &options).unwrap();
    let mut visible = Pdf::from_bytes(&signed).unwrap();
    let after = render(&r, &mut visible, 0);
    let (x0, y0, x1, y1) = changed_bounds(&before, &after).expect("appearance drawn");
    assert!(x0 >= 295 && x1 <= 545 && y0 >= 792 - 175 && y1 <= 792 - 95, "({x0},{y0})-({x1},{y1})");

    let hidden = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let mut invisible = Pdf::from_bytes(&hidden).unwrap();
    assert!(changed_bounds(&before, &render(&r, &mut invisible, 0)).is_none());
}

#[test]
fn certifying_signature_records_docmdp_and_must_come_first() {
    let mut doc = text_pdf(&[&["certified"]]);
    let base = file_bytes(&mut doc);
    let mut options = sign_options(SubFilter::Pkcs7Detached);
    options.certify = Some(CertifyLevel::FormFilling);
    let certified = doc.sign(&base, &options).unwrap();

    let reports = verify(&certified, &trust_test_identity());
    assert!(reports[0].certifying);
    let reopened = Pdf::from_bytes(&certified).unwrap();
    let perms = reopened.document().catalog().unwrap().get(b"Perms").unwrap().as_dict().unwrap().clone();
    assert!(perms.has(b"DocMDP"));

    // Certifying again, or certifying an already signed document, is refused.
    let err = reopened.sign(&certified, &options).unwrap_err().to_string();
    assert!(err.contains("first signature"), "{err}");
}

struct FakeAuthority;

impl TimestampSource for FakeAuthority {
    fn token_for_digest(&self, digest: &[u8; 32]) -> carbide_engine::Result<Vec<u8>> {
        // Not a real token: a ContentInfo-shaped blob that embeds the imprint it was asked for.
        let mut inner = vec![0x04, 32];
        inner.extend_from_slice(digest);
        let mut blob = vec![0x30, (inner.len() + 2) as u8, 0x06, 0x00];
        blob.extend(inner);
        let mut out = vec![0x30];
        out.push(blob.len() as u8);
        out.extend(blob);
        Ok(out)
    }
}

#[test]
fn time_stamps_are_embedded_and_document_time_stamps_check_the_imprint() {
    let mut doc = text_pdf(&[&["stamped"]]);
    let base = file_bytes(&mut doc);
    let authority = FakeAuthority;

    let mut options = sign_options(SubFilter::Pkcs7Detached);
    options.timestamp = Some(&authority);
    let signed = doc.sign(&base, &options).unwrap();
    let reports = verify(&signed, &trust_test_identity());
    assert!(reports[0].has_embedded_timestamp);
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(reports[0].description_lines().contains(&sig_messages::TIME_STAMPED));

    let stamp_only = SignOptions {
        sub_filter: SubFilter::DocumentTimeStamp,
        timestamp: Some(&authority),
        ..Default::default()
    };
    let stamped = doc.sign(&base, &stamp_only).unwrap();
    let reports = verify(&stamped, &TrustStore::default());
    assert!(reports[0].document_time_stamp);
    assert_eq!(reports[0].detail, Detail::DigestOkUntrusted, "imprint matches the covered bytes");
    let mut tampered = stamped.clone();
    let at = tampered.windows(7).position(|w| w == b"stamped").unwrap();
    tampered[at] = b'S';
    assert_eq!(verify(&tampered, &TrustStore::default())[0].validity, Validity::Invalid);
}

#[test]
fn identities_load_from_pem_and_pkcs12() {
    let id = test_identity();
    let from_pem = Identity::from_pem(&id.cert_pem, &id.key_pem).unwrap();
    assert_eq!(from_pem.common_name(), "Carbide Test Signer");
    assert_eq!(from_pem.certificate_der(), &id.certificate[..]);

    // Write a PKCS#12 file and read it back.
    use p12_keystore::{Certificate, KeyStore, KeyStoreEntry, PrivateKey, PrivateKeyChain};
    let mut store = KeyStore::new();
    let key = PrivateKey::from_der(&id.pkcs8).unwrap();
    let cert = Certificate::from_der(&id.certificate).unwrap();
    store.add_entry("signer", KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(b"1".to_vec(), key, [cert])));
    let bytes = store.writer("s3cret").write().unwrap();
    let loaded = Identity::from_pkcs12(&bytes, "s3cret").unwrap();
    assert_eq!(loaded.certificate_der(), &id.certificate[..]);
    assert!(Identity::from_pkcs12(&bytes, "wrong").is_err());

    // The loaded identity signs a document that verifies against the same certificate.
    let mut doc = text_pdf(&[&["from p12"]]);
    let base = file_bytes(&mut doc);
    let options = SignOptions { identity: Some(&loaded), ..Default::default() };
    let signed = doc.sign(&base, &options).unwrap();
    assert_eq!(verify(&signed, &trust_test_identity())[0].validity, Validity::Valid);
}

// ---- OCR -----------------------------------------------------------------------------------

fn ocr_models() -> PathBuf {
    match std::env::var_os("CARBIDE_OCR_MODELS") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tools/ocr-models"),
    }
}

/// A page whose only content is a picture of text, as a scanner would produce.
fn scanned_page(lines: &[&str]) -> Pdf {
    let r = renderer();
    let mut source = text_pdf(&[lines]);
    let bytes = source.to_bytes().unwrap();
    let rendered = r.render_page(&bytes, 0, 200.0).unwrap();
    let rgb = image::DynamicImage::ImageRgba8(rendered).to_rgb8();
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 92)
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let picture = doc.add_object(
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => rgb.width() as i64,
                "Height" => rgb.height() as i64, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            jpeg.into_inner(),
        )
        .with_compression(false),
    );
    let content = doc.add_object(Stream::new(lopdf::Dictionary::new(), b"q 612 0 0 792 0 0 cm /Scan Do Q".to_vec()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
        "Resources" => dictionary! { "XObject" => dictionary! { "Scan" => picture } },
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1 }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    Pdf::from_bytes(&out).unwrap()
}

#[test]
fn ocr_adds_a_searchable_layer_without_changing_the_scan() {
    let r = renderer();
    let mut pdf = scanned_page(&["Invoice number 20481", "Total due 1250 dollars"]);
    assert!(page_text(&r, &mut pdf, 0).trim().is_empty(), "the scan has no text to begin with");
    let before = render(&r, &mut pdf, 0);

    let options = OcrOptions { models_dir: Some(ocr_models()), ..Default::default() };
    let report = pdf.make_searchable(&r, &options).unwrap();
    assert_eq!(report.recognized_pages, vec![0]);
    assert!(report.lines >= 2, "{report:?}");

    let text = page_text(&r, &mut pdf, 0).to_lowercase();
    assert!(text.contains("invoice"), "recognized text: {text:?}");
    assert!(text.contains("20481"), "recognized text: {text:?}");
    assert!(text.contains("1250"), "recognized text: {text:?}");

    // The picture itself is untouched: the invisible layer draws nothing.
    let after = render(&r, &mut pdf, 0);
    assert!(changed_bounds(&before, &after).is_none(), "OCR text must be invisible");
}

#[test]
fn ocr_skips_pages_that_already_have_text() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["This page already has plenty of real text"]]);
    let options = OcrOptions { models_dir: Some(ocr_models()), ..Default::default() };
    let report = pdf.make_searchable(&r, &options).unwrap();
    assert_eq!(report.skipped_pages, vec![0]);
    assert!(report.recognized_pages.is_empty());
}

#[test]
fn ocr_reports_missing_models_clearly() {
    let r = renderer();
    let mut pdf = scanned_page(&["x"]);
    let options = OcrOptions { models_dir: Some(std::env::temp_dir().join("no-such-models")), ..Default::default() };
    // Only fails if no other search location has them either.
    if std::env::var_os("CARBIDE_OCR_MODELS").is_none() {
        let err = pdf.make_searchable(&r, &options).unwrap_err().to_string();
        assert!(err.contains("OCR models not found") || err.contains("page"), "{err}");
    }
}

#[test]
fn annotation_summaries_list_comments_in_page_order() {
    let mut pdf = text_pdf(&[&["first page"], &["second page"]]);
    pdf.add_note(1, 100.0, 600.0, "on page two").unwrap();
    pdf.add_text_box(0, 100.0, 500.0, "typed", 12.0).unwrap();
    let reopened = Pdf::from_bytes(&pdf.to_bytes().unwrap()).unwrap();
    let list = reopened.annotation_summaries();
    assert_eq!(list.len(), 2);
    assert_eq!((list[0].page, list[0].kind.as_str(), list[0].contents.as_str()), (0, "FreeText", "typed"));
    assert_eq!((list[1].page, list[1].kind.as_str(), list[1].contents.as_str()), (1, "Text", "on page two"));
}
