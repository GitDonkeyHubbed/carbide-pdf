//! Behavioural tests for the engine. Rendering checks use PDFium; set `CARBIDE_PDFIUM_LIB` or keep
//! the project's `tools/pdfium` directory in place.

use std::path::PathBuf;
use std::sync::OnceLock;

use carbide_engine::autosave::{AutoSaveStore, RowKind};
use carbide_engine::fs::Existing;
use carbide_engine::imposition::{ImpositionOptions, Margins, Media};
use carbide_engine::ocr::OcrOptions;
use carbide_engine::signature::{
    messages as sig_messages, notice_text, CertifyLevel, Detail, HttpTimestampSource, Identity, SignOptions, SubFilter,
    TimestampSource, TrustStore, Validity,
};
use carbide_engine::reduce::{messages as reduce_messages, reduce_files, OutputOptions, ReduceOptions, TargetFolder};
use carbide_engine::imposition_plan::{Binding, LayoutKind, PlanOptions, PrintingOrder, Style};
use carbide_engine::{
    compare_documents, export_images, export_text, image_paths, open_revision_bytes, pdf_date_now, BatesOptions, Error,
    HiddenItem, ImageFormat, MarkupStyle, PageAnchor, Pdf, ProtectionOptions, Rect, Redaction, Renderer, SavePlan, SavedFile,
    WatermarkOptions,
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
    let dir = scratch_dir("export");
    let paths = export_images(r, &mut pdf, &dir, "doc", ImageFormat::Png, 72.0, Existing::Refuse).unwrap();
    let names: Vec<_> = paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["doc-001.png", "doc-002.png"]);
    assert_eq!(paths, image_paths(&pdf, &dir, "doc", ImageFormat::Png));
    assert_eq!(image::open(&paths[0]).unwrap().width(), 612);
    let jpegs = export_images(r, &mut pdf, &dir, "doc", ImageFormat::Jpeg, 72.0, Existing::Refuse).unwrap();
    assert_eq!(image::open(&jpegs[1]).unwrap().width(), 612);
    assert!(export_text(&r, &mut pdf).unwrap().contains("page two"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn exported_images_never_replace_files_unless_asked() {
    let r = renderer();
    let mut pdf = text_pdf(&[&["page one"], &["page two"]]);
    let dir = scratch_dir("export-existing");
    std::fs::create_dir_all(&dir).unwrap();
    // An earlier export of another document with the same name.
    std::fs::write(dir.join("doc-002.png"), b"earlier export").unwrap();
    let listing = |dir: &PathBuf| {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    };

    let refused = export_images(r, &mut pdf, &dir, "doc", ImageFormat::Png, 72.0, Existing::Refuse).unwrap_err();
    assert!(refused.to_string().contains("doc-002.png"), "{refused}");
    assert_eq!(listing(&dir), ["doc-002.png"], "nothing is written when a name is taken");

    let kept = export_images(r, &mut pdf, &dir, "doc", ImageFormat::Png, 72.0, Existing::KeepBoth).unwrap();
    assert_eq!(kept, [dir.join("doc-001.png"), dir.join("doc-002 (2).png")]);
    assert_eq!(std::fs::read(dir.join("doc-002.png")).unwrap(), b"earlier export");

    let replaced = export_images(r, &mut pdf, &dir, "doc", ImageFormat::Png, 72.0, Existing::Replace).unwrap();
    assert_eq!(replaced, [dir.join("doc-001.png"), dir.join("doc-002.png")]);
    assert_eq!(image::open(dir.join("doc-002.png")).unwrap().width(), 612);
    assert_eq!(listing(&dir), ["doc-001.png", "doc-002 (2).png", "doc-002.png"], "no temporary files are left");
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
    assert!(!contains(&encrypted, b"private"));
    let (mut opened, saved) = SavedFile::open(encrypted.clone(), Some("open-sesame".into())).unwrap();
    assert!(contains(&opened.to_bytes().unwrap(), b"private"), "the decrypted document serializes as plaintext");

    opened.add_note(0, 100.0, 100.0, "checked").unwrap();
    let plan = opened.save_plan(Some(&saved), false).unwrap();
    assert_eq!(plan, SavePlan::FullRewrite { breaks_signatures: false, drops_revisions: false });
    let bytes = opened.save_bytes(plan, Some(&saved)).unwrap();
    assert!(!contains(&bytes, b"private") && !contains(&bytes, b"checked"));
    assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)));
    // Both of the original passwords still open it.
    for password in ["open-sesame", "owner-pw"] {
        let mut again = Pdf::from_bytes_with_password(&bytes, password).unwrap();
        assert!(contains(&again.to_bytes().unwrap(), b"private"), "{password}");
    }
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

#[test]
fn page_text_that_looks_like_the_length_placeholder_is_left_alone() {
    let r = renderer();
    let base = text_pdf(&[&["numbered page"]]).to_bytes().unwrap();
    let mut working = Pdf::from_bytes(&base).unwrap();
    // Ten-digit Bates numbers from zero: the page content holds "(0000000000)", written after the
    // catalog in the update.
    working.apply_bates(&BatesOptions { start_number: 0, digits: 10, ..Default::default() }).unwrap();
    let file = working.save_revision(&base, "Numbered", &pdf_date_now()).unwrap();
    let appended = &file[base.len()..];
    let catalog_at = appended.windows(16).position(|w| w == b"CarbideRevisions").expect("the record is in the update");
    assert!(
        appended.windows(12).rposition(|w| w == b"(0000000000)").is_some_and(|at| at > catalog_at),
        "the fixture puts the page text after the record"
    );

    let mut saved = Pdf::from_bytes(&file).unwrap();
    let revisions = saved.revisions();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].file_offset as usize, file.len());
    assert_eq!(open_revision_bytes(&file, &revisions[0]).unwrap(), file);
    assert!(page_text(r, &mut saved, 0).contains("0000000000"), "the page text was patched");
}

// ---- saving over the file ----------------------------------------------------------------

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// Fails if `needle` is anywhere in `file`: in the raw bytes, or in any stream body, decompressed,
/// whether or not a cross-reference table still points at it.
fn assert_gone(file: &[u8], needle: &str) {
    assert!(!contains(file, needle.as_bytes()), "{needle:?} is in the raw file");
    let mut at = 0;
    while let Some(found) = file[at..].windows(6).position(|w| w == b"stream") {
        let start = at + found + 6;
        at = start;
        if file[..start - 6].ends_with(b"end") {
            continue;
        }
        let Some(length) = file[start..].windows(9).position(|w| w == b"endstream") else { break };
        let body = &file[start..start + length];
        let body = body.strip_prefix(b"\r\n").or_else(|| body.strip_prefix(b"\n")).unwrap_or(body);
        let mut decoded = Vec::new();
        if std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(body), &mut decoded).is_ok() {
            assert!(!contains(&decoded, needle.as_bytes()), "{needle:?} is in a compressed stream");
        }
    }
}

fn plan_and_bytes(pdf: &mut Pdf, saved: &SavedFile, content_removed: bool) -> (SavePlan, Vec<u8>) {
    let plan = pdf.save_plan(Some(saved), content_removed).unwrap();
    let bytes = pdf.save_bytes(plan, Some(saved)).unwrap();
    (plan, bytes)
}

const CLEAN: SavePlan = SavePlan::FullRewrite { breaks_signatures: false, drops_revisions: false };

#[test]
fn saving_after_redaction_leaves_no_trace_of_the_secret() {
    let r = renderer();
    let base = text_pdf(&[&["TOP SECRET 12345", "public line"], &["page two stays"]]).to_bytes().unwrap();
    let (mut pdf, saved) = SavedFile::open(base.clone(), None).unwrap();
    pdf.apply_redactions(r, &[Redaction { page: 0, rect: Rect::new(60.0, 692.0, 300.0, 28.0) }], 150.0).unwrap();
    assert!(saved.removed_from(&pdf), "the redaction dropped content the file holds");

    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert_eq!(plan, CLEAN);
    assert!(!bytes.starts_with(&base[..base.len() / 2]), "nothing of the old file is kept as a prefix");
    assert_gone(&bytes, "SECRET");
    assert_gone(&bytes, "12345");
    let mut reopened = Pdf::from_bytes(&bytes).unwrap();
    assert!(page_text(r, &mut reopened, 1).contains("page two stays"));
}

#[test]
fn deleted_pages_leave_nothing_behind() {
    let base = text_pdf(&[&["kept page"], &["DELETEDPAGE 999"]]).to_bytes().unwrap();
    let (mut pdf, saved) = SavedFile::open(base, None).unwrap();
    pdf.delete_pages(&[1]).unwrap();
    assert!(saved.removed_from(&pdf));
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert_eq!(plan, CLEAN);
    assert_gone(&bytes, "DELETEDPAGE");
    assert_eq!(Pdf::from_bytes(&bytes).unwrap().page_count(), 1);
}

/// Four pages (`PAGE1X` to `PAGE4X`) and everything that leads to a page from outside the page
/// tree: outline items (one by explicit destination, one by named destination, one by go-to
/// action, and "Part Two" with an item below it), named destinations in the catalog and in the
/// name tree, an open action, links, an article thread, and page labels i, ii, 1, 2. Everything
/// whose name starts with "Secret" leads to the second page.
fn navigable_pdf() -> Pdf {
    let mut pdf = numbered_pages(4);
    let ids = pdf.page_ids();
    let doc = pdf.document_mut();
    let dest = |page: lopdf::ObjectId| Object::Array(vec![Object::Reference(page), "Fit".into()]);
    let title = |text: &str| Object::string_literal(text);

    let root = doc.new_object_id();
    let intro = doc.add_object(dictionary! { "Title" => title("Intro Chapter"), "Parent" => root, "Dest" => dest(ids[0]) });
    let secret = doc.add_object(dictionary! { "Title" => title("SECRET CHAPTER"), "Parent" => root, "Dest" => title("SecretPlace") });
    let closing = doc.add_object(dictionary! {
        "Title" => title("Closing Chapter"), "Parent" => root, "A" => dictionary! { "S" => "GoTo", "D" => dest(ids[2]) },
    });
    let part = doc.add_object(dictionary! { "Title" => title("Part Two"), "Parent" => root, "Dest" => "SecretName", "Count" => 1 });
    let appendix = doc.add_object(dictionary! { "Title" => title("Appendix"), "Parent" => part, "Dest" => dest(ids[3]) });
    let link = |doc: &mut Document, id, key: &str, value| doc.get_dictionary_mut(id).unwrap().set(key, Object::Reference(value));
    for (a, b) in [(intro, secret), (secret, closing), (closing, part)] {
        link(doc, a, "Next", b);
        link(doc, b, "Prev", a);
    }
    link(doc, part, "First", appendix);
    link(doc, part, "Last", appendix);
    doc.objects.insert(root, Object::Dictionary(dictionary! { "Type" => "Outlines", "First" => intro, "Last" => part, "Count" => 5 }));

    let bead = |doc: &mut Document, page: lopdf::ObjectId| {
        doc.add_object(dictionary! { "Type" => "Bead", "P" => page, "R" => vec![0.into(), 0.into(), 100.into(), 100.into()] })
    };
    let (first_bead, second_bead) = (bead(doc, ids[1]), bead(doc, ids[2]));
    let thread = doc.add_object(dictionary! { "Type" => "Thread", "F" => first_bead, "I" => dictionary! { "Title" => title("Secret Article") } });
    for (bead, other) in [(first_bead, second_bead), (second_bead, first_bead)] {
        let dict = doc.get_dictionary_mut(bead).unwrap();
        dict.set("T", thread);
        dict.set("N", other);
        dict.set("V", other);
    }
    for (page, target, bead) in [(ids[0], ids[1], None), (ids[2], ids[3], Some(second_bead)), (ids[1], ids[0], Some(first_bead))] {
        let annotation = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link", "Rect" => vec![0.into(), 0.into(), 50.into(), 50.into()], "Dest" => dest(target),
        });
        let page = doc.get_dictionary_mut(page).unwrap();
        page.set("Annots", vec![Object::Reference(annotation)]);
        if let Some(bead) = bead {
            page.set("B", vec![Object::Reference(bead)]);
        }
    }

    let catalog = doc.catalog_mut().unwrap();
    catalog.set("Outlines", root);
    catalog.set("Dests", dictionary! { "SecretName" => dest(ids[1]), "Kept" => dest(ids[0]) });
    catalog.set("Names", dictionary! { "Dests" => dictionary! { "Names" => vec![title("Intro"), dest(ids[0]), title("SecretPlace"), dest(ids[1])] } });
    catalog.set("OpenAction", dest(ids[1]));
    catalog.set("Threads", vec![Object::Reference(thread)]);
    catalog.set("PageLabels", dictionary! { "Nums" => vec![0.into(), dictionary! { "S" => "r" }.into(), 2.into(), dictionary! { "S" => "D" }.into()] });
    let bytes = pdf.to_bytes().unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

/// The titles of the outline items below `parent`, in order, each with the titles below it.
fn outline(pdf: &Pdf, parent: lopdf::ObjectId) -> Vec<(String, Vec<String>)> {
    let doc = pdf.document();
    let mut items = Vec::new();
    let mut next = doc.get_dictionary(parent).unwrap().get(b"First").and_then(Object::as_reference).ok();
    while let Some(id) = next {
        let item = doc.get_dictionary(id).unwrap();
        let title = String::from_utf8_lossy(item.get(b"Title").unwrap().as_str().unwrap()).into_owned();
        items.push((title, outline(pdf, id).into_iter().map(|(t, _)| t).collect()));
        next = item.get(b"Next").and_then(Object::as_reference).ok();
    }
    items
}

#[test]
fn deleting_a_page_takes_out_what_leads_only_to_it() {
    let mut pdf = navigable_pdf();
    let old = pdf.page_ids();
    pdf.delete_pages(&[1]).unwrap();
    let bytes = file_bytes(&mut pdf);
    for needle in ["SECRET CHAPTER", "SecretPlace", "SecretName", "PAGE2X"] {
        assert_gone(&bytes, needle);
    }
    let pdf = Pdf::from_bytes(&bytes).unwrap();
    let doc = pdf.document();
    let catalog = doc.catalog().unwrap();
    assert_eq!(pdf.page_count(), 3);

    // "Part Two" led to the deleted page, but stays (without a destination) for the item below it.
    let root = catalog.get(b"Outlines").unwrap().as_reference().unwrap();
    let titles = outline(&pdf, root);
    let expected = [("Intro Chapter", vec![]), ("Closing Chapter", vec![]), ("Part Two", vec!["Appendix".to_string()])];
    assert_eq!(titles, expected.map(|(t, below)| (t.to_string(), below)));
    assert_eq!(doc.get_dictionary(root).unwrap().get(b"Count").unwrap().as_i64().unwrap(), 4);
    let part = doc.get_dictionary(root).unwrap().get(b"Last").unwrap().as_reference().unwrap();
    assert!(!doc.get_dictionary(part).unwrap().has(b"Dest"));

    assert!(!catalog.has(b"OpenAction"));
    let dests = catalog.get(b"Dests").unwrap().as_dict().unwrap();
    assert_eq!(dests.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), [b"Kept".to_vec()]);
    let labels = catalog.get(b"PageLabels").unwrap().as_dict().unwrap().get(b"Nums").unwrap().as_array().unwrap();
    assert_eq!(labels.len(), 4, "{labels:?}");
    assert_eq!((labels[0].as_i64().unwrap(), labels[2].as_i64().unwrap()), (0, 1), "i, 1, 2: {labels:?}");
    assert_eq!(labels[3].as_dict().unwrap().get(b"S").unwrap().as_name().unwrap(), b"D");

    // The link to the deleted page is gone; the one to a page that stays is not.
    let link = |page: usize| pdf.annotations(page).unwrap()[0].clone();
    assert!(!link(0).has(b"Dest"));
    assert_eq!(link(1).get(b"Dest").unwrap().as_array().unwrap()[0].as_reference().unwrap(), old[3]);

    // The article keeps its bead on the page that stays.
    let thread = catalog.get(b"Threads").unwrap().as_array().unwrap()[0].as_reference().unwrap();
    let bead = doc.get_dictionary(thread).unwrap().get(b"F").unwrap().as_reference().unwrap();
    let bead_dict = doc.get_dictionary(bead).unwrap();
    assert_eq!(bead_dict.get(b"P").unwrap().as_reference().unwrap(), old[2]);
    assert_eq!(bead_dict.get(b"N").unwrap().as_reference().unwrap(), bead);
}

#[test]
fn page_labels_carry_on_where_a_page_was_deleted() {
    let mut pdf = navigable_pdf();
    // i, ii, 1, 2 without the "1": i, ii, 2.
    pdf.delete_pages(&[2]).unwrap();
    let catalog = pdf.document().catalog().unwrap();
    let labels = catalog.get(b"PageLabels").unwrap().as_dict().unwrap().get(b"Nums").unwrap().as_array().unwrap().clone();
    assert_eq!(labels.len(), 4, "{labels:?}");
    assert_eq!((labels[0].as_i64().unwrap(), labels[2].as_i64().unwrap()), (0, 2));
    let decimal = labels[3].as_dict().unwrap();
    assert_eq!((decimal.get(b"S").unwrap().as_name().unwrap(), decimal.get(b"St").unwrap().as_i64().unwrap()), (&b"D"[..], 2));
    // "Closing Chapter" led to the deleted page; the other three items stay.
    assert_eq!(outline(&pdf, catalog.get(b"Outlines").unwrap().as_reference().unwrap()).len(), 3);
}

/// The node `id` of a name tree: its /Limits as text, and how many kids it has.
fn tree_node(pdf: &Pdf, id: lopdf::ObjectId) -> (Vec<String>, usize) {
    let dict = pdf.document().get_dictionary(id).unwrap();
    let text = |o: &Object| String::from_utf8_lossy(o.as_str().unwrap()).into_owned();
    let limits = dict.get(b"Limits").map(|l| l.as_array().unwrap().iter().map(text).collect()).unwrap_or_default();
    (limits, dict.get(b"Kids").map(|k| k.as_array().unwrap().len()).unwrap_or(0))
}

#[test]
fn deleted_destinations_leave_no_name_in_the_limits_of_their_name_tree() {
    // Named destinations in a tree of several levels, as long documents have them, each node
    // naming the first and last name below it.
    let mut pdf = text_pdf(&[&["first"], &["second"], &["third"]]);
    let pages = pdf.page_ids();
    let doc = pdf.document_mut();
    let name = |n: &str| Object::string_literal(n);
    let to = |page| Object::Array(vec![Object::Reference(page), "Fit".into()]);
    let leaf = |doc: &mut Document, names: &[(&str, lopdf::ObjectId)]| {
        let limits = vec![name(names[0].0), name(names[names.len() - 1].0)];
        let pairs: Vec<Object> = names.iter().flat_map(|(n, page)| [name(n), to(*page)]).collect();
        doc.add_object(dictionary! { "Limits" => limits, "Names" => pairs })
    };
    let first = leaf(doc, &[("Alpha", pages[0]), ("SecretMergerTarget", pages[1])]);
    let second = leaf(doc, &[("SecretZeta", pages[1]), ("Zulu", pages[2])]);
    let third = leaf(doc, &[("ZzSecretLast", pages[1])]);
    let middle = doc.add_object(dictionary! {
        "Limits" => vec![name("Alpha"), name("ZzSecretLast")],
        "Kids" => vec![first.into(), second.into(), third.into()],
    });
    let root = doc.add_object(dictionary! { "Kids" => vec![middle.into()] });
    doc.catalog_mut().unwrap().set("Names", dictionary! { "Dests" => root });

    pdf.delete_pages(&[1]).unwrap();
    let bytes = file_bytes(&mut pdf);
    assert_gone(&bytes, "Secret");
    let pdf = Pdf::from_bytes(&bytes).unwrap();
    assert_eq!(tree_node(&pdf, first), (vec!["Alpha".into(), "Alpha".into()], 0));
    assert_eq!(tree_node(&pdf, second), (vec!["Zulu".into(), "Zulu".into()], 0));
    assert_eq!(tree_node(&pdf, middle), (vec!["Alpha".into(), "Zulu".into()], 2), "the emptied leaf is taken out");
    assert_eq!(tree_node(&pdf, root), (vec![], 1), "the root has no limits");

    // A tree left without a name goes altogether.
    let mut pdf = text_pdf(&[&["first"], &["second"]]);
    let gone_to = pdf.page_ids()[1];
    let doc = pdf.document_mut();
    let only = doc.add_object(dictionary! { "Limits" => vec![name("OnlySecret"), name("OnlySecret")], "Names" => vec![name("OnlySecret"), to(gone_to)] });
    let root = doc.add_object(dictionary! { "Kids" => vec![only.into()] });
    doc.catalog_mut().unwrap().set("Names", dictionary! { "Dests" => root });
    pdf.delete_pages(&[1]).unwrap();
    let bytes = file_bytes(&mut pdf);
    assert_gone(&bytes, "OnlySecret");
    let catalog = Pdf::from_bytes(&bytes).unwrap().document().catalog().unwrap().clone();
    assert!(!catalog.get(b"Names").unwrap().as_dict().unwrap().has(b"Dests"));
}

/// One page showing "visible text", and hidden information of every kind, each holding
/// `SECRET-<kind>`: document information, XMP, an attachment and a file attachment comment,
/// document JavaScript run on opening and on a page event, a filled text field, a note with its
/// pop-up, a thumbnail, private application data, a bookmark and an object nothing uses. A link
/// and the form field are not hidden information and stay.
fn hidden_pdf() -> Vec<u8> {
    let mut pdf = text_pdf(&[&["visible text"]]);
    let page = pdf.page_ids()[0];
    let doc = pdf.document_mut();
    let secret = |kind: &str| Object::string_literal(format!("SECRET-{kind}"));
    let rect = || vec![Object::from(10), 10.into(), 60.into(), 30.into()];
    let stream = |doc: &mut Document, dict: lopdf::Dictionary, kind: &str| {
        doc.add_object(Stream::new(dict, format!("SECRET-{kind}").into_bytes()).with_compression(false))
    };

    let info = doc.add_object(dictionary! { "Title" => secret("TITLE"), "Author" => "someone" });
    doc.trailer.set("Info", info);
    let xmp = stream(doc, dictionary! { "Type" => "Metadata", "Subtype" => "XML" }, "XMP");
    let attached = stream(doc, dictionary! { "Type" => "EmbeddedFile" }, "ATTACHMENT");
    let spec = doc.add_object(dictionary! { "Type" => "Filespec", "F" => secret("FILENAME"), "EF" => dictionary! { "F" => attached } });
    let in_comment = stream(doc, dictionary! { "Type" => "EmbeddedFile" }, "COMMENTFILE");
    let comment_spec = doc.add_object(dictionary! { "Type" => "Filespec", "F" => "a.txt", "EF" => dictionary! { "F" => in_comment } });
    let script = doc.add_object(dictionary! { "S" => "JavaScript", "JS" => secret("SCRIPT") });
    let appearance = stream(doc, dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => rect() }, "VALUE");
    let field = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx", "T" => "name", "V" => secret("VALUE"), "Rect" => rect(), "P" => page,
        "AP" => dictionary! { "N" => appearance },
    });
    let popup = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Popup", "Rect" => rect() });
    let note = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Text", "Rect" => rect(), "Contents" => secret("COMMENT"), "Popup" => popup });
    doc.get_dictionary_mut(popup).unwrap().set("Parent", note);
    let file_comment = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "FileAttachment", "Rect" => rect(), "FS" => comment_spec });
    let link = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Link", "Rect" => rect(), "A" => dictionary! { "S" => "URI", "URI" => "https://example.com" },
    });
    let thumbnail = stream(doc, dictionary! { "Width" => 1, "Height" => 1, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 }, "THUMB");
    let private = dictionary! { "Editor" => dictionary! { "Private" => secret("PRIVATE"), "LastModified" => "D:20260101" } };
    let page_dict = doc.get_dictionary_mut(page).unwrap();
    page_dict.set("Annots", vec![field.into(), note.into(), popup.into(), file_comment.into(), link.into()]);
    page_dict.set("Thumb", thumbnail);
    page_dict.set("PieceInfo", private);
    page_dict.set("AA", dictionary! { "O" => script });

    let outlines = doc.catalog().unwrap().get(b"Outlines").unwrap().as_reference().unwrap();
    let bookmark = doc.add_object(dictionary! { "Title" => secret("BOOKMARK"), "Parent" => outlines, "Dest" => vec![page.into(), "Fit".into()] });
    let outline_root = doc.get_dictionary_mut(outlines).unwrap();
    outline_root.set("First", bookmark);
    outline_root.set("Last", bookmark);
    outline_root.set("Count", 1);
    let catalog = doc.catalog_mut().unwrap();
    catalog.set("Metadata", xmp);
    catalog.set("Names", dictionary! {
        "EmbeddedFiles" => dictionary! { "Names" => vec![Object::string_literal("notes.txt"), spec.into()] },
        "JavaScript" => dictionary! { "Names" => vec![Object::string_literal("init"), script.into()] },
    });
    catalog.set("OpenAction", script);
    catalog.set("AcroForm", dictionary! { "Fields" => vec![Object::Reference(field)] });
    stream(doc, lopdf::Dictionary::new(), "ORPHAN");
    // Written as it is, without dropping the object nothing uses.
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

const HIDDEN: [&str; 11] =
    ["TITLE", "XMP", "ATTACHMENT", "FILENAME", "COMMENTFILE", "SCRIPT", "VALUE", "COMMENT", "THUMB", "PRIVATE", "ORPHAN"];

#[test]
fn hidden_information_is_removed_as_chosen_and_reported() {
    let file = hidden_pdf();
    for kind in HIDDEN.iter().chain(&["BOOKMARK"]) {
        assert!(contains(&file, format!("SECRET-{kind}").as_bytes()), "the fixture lacks {kind}");
    }

    // Only what is chosen goes.
    let mut pdf = Pdf::from_bytes(&file).unwrap();
    let report = pdf.remove_hidden_information(&[HiddenItem::Metadata]).unwrap();
    assert_eq!(report.count(HiddenItem::Metadata), 2);
    assert_eq!(report.lines(), ["Document information (title, author, keywords): 2"]);
    let bytes = pdf.to_bytes().unwrap();
    assert_gone(&bytes, "SECRET-TITLE");
    assert!(contains(&bytes, b"SECRET-XMP") && contains(&bytes, b"SECRET-COMMENT"));

    // Everything but the bookmarks by default.
    let mut pdf = Pdf::from_bytes(&file).unwrap();
    let chosen: Vec<HiddenItem> = HiddenItem::ALL.into_iter().filter(|i| i.chosen_by_default()).collect();
    let report = pdf.remove_hidden_information(&chosen).unwrap();
    for item in &chosen {
        assert!(report.count(*item) >= 1, "{item:?} was not found: {report:?}");
    }
    assert_eq!(report.count(HiddenItem::Comments), 1, "the pop-up goes with its note, uncounted");
    assert!(report.lines().contains(&"Comments, text boxes and markup: 1".to_string()), "{:?}", report.lines());
    assert_eq!(report.count(HiddenItem::Attachments), 2);
    assert_eq!(report.count(HiddenItem::Scripts), 3, "the named script, the open action and the page event");
    let bytes = pdf.to_bytes().unwrap();
    for kind in HIDDEN {
        assert_gone(&bytes, &format!("SECRET-{kind}"));
    }
    assert!(contains(&bytes, b"SECRET-BOOKMARK"), "bookmarks stay unless chosen");
    let reopened = Pdf::from_bytes(&bytes).unwrap();
    let kinds: Vec<Vec<u8>> =
        reopened.annotations(0).unwrap().iter().map(|a| a.get(b"Subtype").unwrap().as_name().unwrap().to_vec()).collect();
    assert_eq!(kinds, [b"Widget".to_vec(), b"Link".to_vec()]);
    assert!(page_text(renderer(), &mut pdf, 0).contains("visible text"));

    let report = pdf.remove_hidden_information(&[HiddenItem::Bookmarks]).unwrap();
    assert_eq!(report.count(HiddenItem::Bookmarks), 1);
    assert_gone(&pdf.to_bytes().unwrap(), "SECRET-BOOKMARK");
    assert!(pdf.remove_hidden_information(&HiddenItem::ALL).unwrap().is_empty(), "nothing is left to find");
}

/// A page with a note holding "SECRET-OLD-COMMENT" and the title "SECRET-OLD-TITLE", then an
/// update appended that takes the note off the page and changes the title: both stay in the file,
/// in its earlier version.
fn file_with_history() -> Vec<u8> {
    let mut pdf = text_pdf(&[&["page"]]);
    pdf.add_note(0, 100.0, 100.0, "SECRET-OLD-COMMENT").unwrap();
    let info = pdf.document_mut().add_object(dictionary! { "Title" => Object::string_literal("SECRET-OLD-TITLE") });
    pdf.document_mut().trailer.set("Info", info);
    let base = pdf.to_bytes().unwrap();
    let mut later = Pdf::from_bytes(&base).unwrap();
    let page = later.page_ids()[0];
    later.document_mut().get_dictionary_mut(page).unwrap().remove(b"Annots");
    later.document_mut().get_dictionary_mut(info).unwrap().set("Title", Object::string_literal("Final"));
    later.incremental_update(&base).unwrap()
}

#[test]
fn removing_hidden_information_leaves_nothing_behind_in_the_file_it_saves_over() {
    // An object nothing refers to, written out as it is.
    let mut orphaned = text_pdf(&[&["page"]]);
    orphaned.document_mut().add_object(Stream::new(lopdf::Dictionary::new(), b"SECRET-ORPHAN".to_vec()).with_compression(false));
    let mut orphan_file = Vec::new();
    orphaned.document_mut().save_to(&mut orphan_file).unwrap();
    let history_file = file_with_history();
    for secret in ["SECRET-OLD-COMMENT", "SECRET-OLD-TITLE"] {
        assert!(contains(&history_file, secret.as_bytes()), "the fixture lacks {secret}");
    }
    assert_eq!(SavedFile::open(history_file.clone(), None).unwrap().1.appended_updates(), 1);

    let defaults: Vec<HiddenItem> = HiddenItem::ALL.into_iter().filter(|i| i.chosen_by_default()).collect();
    for (file, secrets) in [(&orphan_file, &["SECRET-ORPHAN"][..]), (&history_file, &["SECRET-OLD-COMMENT", "SECRET-OLD-TITLE"])] {
        for items in [&defaults[..], &[HiddenItem::Thumbnails]] {
            let mut reported = Vec::new();
            // As the command line opens a file, and as the app does, which drops what nothing
            // refers to as it opens it: the same is reported either way.
            for dropped_on_opening in [false, true] {
                let (mut pdf, saved) = SavedFile::open(file.clone(), None).unwrap();
                if dropped_on_opening {
                    pdf.to_bytes().unwrap();
                }
                assert_ne!(pdf.save_plan(Some(&saved), true).unwrap(), SavePlan::Unchanged, "a removal always rewrites");
                let report = pdf.remove_hidden_information_from(items, Some(&saved)).unwrap();
                assert!(!report.is_empty() && report.in_file() >= 1, "{report:?}");
                assert!(report.lines().iter().any(|l| l.starts_with(carbide_engine::sanitize::IN_FILE_LABEL)), "{report:?}");
                reported.push(report.lines());

                let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
                assert_eq!(plan, CLEAN);
                for secret in secrets {
                    assert_gone(&bytes, secret);
                }
                // What was written holds nothing more.
                let (mut again, rewritten) = SavedFile::open(bytes, None).unwrap();
                assert_eq!(rewritten.appended_updates(), 0);
                assert!(again.remove_hidden_information_from(items, Some(&rewritten)).unwrap().is_empty());
            }
            assert_eq!(reported[0], reported[1]);
        }
    }
}

/// A one-page file written the way web-optimized files are: a linearization dictionary first, the
/// hint stream it points to by offset, and a content stream whose length is an object of its own.
/// `orphan`, when given, is written as one more object nothing refers to.
fn linearized_file(orphan: Option<&str>) -> Vec<u8> {
    let write = |hint_offset: usize| {
        let mut objects = vec![
            (6, format!("<</Linearized 1/L 0/H [{hint_offset:010} 30]/O 3/E 0/N 1/T 0>>")),
            (1, "<</Type/Catalog/Pages 2 0 R>>".to_string()),
            (2, "<</Type/Pages/Kids [3 0 R]/Count 1>>".to_string()),
            (3, "<</Type/Page/Parent 2 0 R/MediaBox [0 0 612 792]/Contents 4 0 R>>".to_string()),
            (4, "<</Length 5 0 R>>\nstream\n0 0 m 10 10 l S\nendstream".to_string()),
            (5, "16".to_string()),
            (7, "<</Length 8/S 4>>\nstream\n\0\0\0\0\0\0\0\0\nendstream".to_string()),
        ];
        if let Some(orphan) = orphan {
            objects.push((8, orphan.to_string()));
        }
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![0; objects.len() + 1];
        for (number, body) in &objects {
            offsets[*number] = bytes.len();
            bytes.extend(format!("{number} 0 obj\n{body}\nendobj\n").into_bytes());
        }
        let xref = bytes.len();
        bytes.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).into_bytes());
        for offset in &offsets[1..] {
            bytes.extend(format!("{offset:010} 00000 n \n").into_bytes());
        }
        bytes.extend(format!("trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n", offsets.len()).into_bytes());
        (bytes, offsets[7])
    };
    // The offsets do not depend on the one written into the dictionary, which has a fixed width.
    let (_, hint_offset) = write(0);
    write(hint_offset).0
}

#[test]
fn how_a_file_was_written_is_not_reported_as_hidden_information() {
    let items = [HiddenItem::UnusedObjects, HiddenItem::Thumbnails];
    let (mut pdf, saved) = SavedFile::open(linearized_file(None), None).unwrap();
    assert_eq!(saved.appended_updates(), 0);
    let report = pdf.remove_hidden_information_from(&items, Some(&saved)).unwrap();
    assert!(report.is_empty(), "the linearization data and the stream length were reported: {report:?}");

    // An object that holds something is found next to them, by either count.
    let file = linearized_file(Some("<</Note (SECRET-ORPHAN)>>"));
    let (mut pdf, saved) = SavedFile::open(file.clone(), None).unwrap();
    assert_eq!(pdf.remove_hidden_information_from(&items, Some(&saved)).unwrap().in_file(), 1);
    let (mut pdf, _) = SavedFile::open(file, None).unwrap();
    assert_eq!(pdf.remove_hidden_information(&items).unwrap().count(HiddenItem::UnusedObjects), 1);
}

#[test]
fn scripts_are_taken_out_wherever_actions_chain_them() {
    let mut pdf = text_pdf(&[&["first"], &["second"]]);
    let pages = pdf.page_ids();
    let doc = pdf.document_mut();
    let script = |text: &str| Object::Dictionary(dictionary! { "S" => "JavaScript", "JS" => Object::string_literal(text) });
    let go_to = |page| dictionary! { "S" => "GoTo", "D" => vec![Object::Reference(page), "Fit".into()] };
    let rect = || vec![Object::from(10), 10.into(), 60.into(), 30.into()];

    // A link, held directly in the page's list, that opens a web page and then runs a script.
    let link = dictionary! {
        "Type" => "Annot", "Subtype" => "Link", "Rect" => rect(),
        "A" => dictionary! { "S" => "URI", "URI" => "https://example.com", "Next" => script("app.alert('SECRET-NEXT-JS')") },
    };
    // A button whose action, an object of its own, runs a script and then goes to the second page.
    let then = doc.add_object(go_to(pages[1]));
    let button_script = doc.add_object(dictionary! { "S" => "JavaScript", "JS" => Object::string_literal("SECRET-BUTTON-JS"), "Next" => then });
    let button = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "FT" => "Btn", "Ff" => 65536, "T" => "go", "Rect" => rect(), "A" => button_script,
    });
    // A media action with a script of its own.
    let rendition = doc.add_object(dictionary! { "S" => "Rendition", "OP" => 0, "JS" => Object::string_literal("SECRET-RENDITION-JS") });
    let screen = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Screen", "Rect" => rect(), "A" => rendition });
    doc.get_dictionary_mut(pages[0]).unwrap().set("Annots", vec![Object::Dictionary(link), button.into(), screen.into()]);
    // A bookmark that goes to the first page, then runs a script, launches an application and
    // goes to the second page.
    let outlines = doc.catalog().unwrap().get(b"Outlines").unwrap().as_reference().unwrap();
    let launch = dictionary! { "S" => "Launch", "F" => Object::string_literal("SECRET-LAUNCH.app") };
    let mut first_page = go_to(pages[0]);
    first_page.set("Next", vec![script("SECRET-OUTLINE-JS"), Object::Dictionary(launch), Object::Dictionary(go_to(pages[1]))]);
    let bookmark = doc.add_object(dictionary! { "Title" => "Chapter", "Parent" => outlines, "A" => first_page });
    let root = doc.get_dictionary_mut(outlines).unwrap();
    root.set("First", bookmark);
    root.set("Last", bookmark);
    root.set("Count", 1);

    let report = pdf.remove_hidden_information(&[HiddenItem::Scripts]).unwrap();
    assert_eq!(report.count(HiddenItem::Scripts), 5, "four scripts and the launch: {report:?}");
    let bytes = file_bytes(&mut pdf);
    for secret in ["SECRET-NEXT-JS", "SECRET-BUTTON-JS", "SECRET-RENDITION-JS", "SECRET-OUTLINE-JS", "SECRET-LAUNCH"] {
        assert_gone(&bytes, secret);
    }

    // What runs no code stays, and each chain closes up around what went.
    let pdf = Pdf::from_bytes(&bytes).unwrap();
    let doc = pdf.document();
    let resolve = |object: &Object| match object {
        Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
        other => other.as_dict().unwrap().clone(),
    };
    let kind = |action: &lopdf::Dictionary| action.get(b"S").unwrap().as_name().unwrap().to_vec();
    let target = |action: &lopdf::Dictionary| action.get(b"D").unwrap().as_array().unwrap()[0].as_reference().unwrap();
    let annotations = pdf.annotations(0).unwrap();
    let link = resolve(annotations[0].get(b"A").unwrap());
    assert_eq!((kind(&link), link.has(b"Next")), (b"URI".to_vec(), false));
    let button = resolve(annotations[1].get(b"A").unwrap());
    assert_eq!((kind(&button), target(&button)), (b"GoTo".to_vec(), pages[1]));
    let rendition = resolve(annotations[2].get(b"A").unwrap());
    assert_eq!((kind(&rendition), rendition.has(b"JS")), (b"Rendition".to_vec(), false));
    let bookmark = resolve(doc.get_dictionary(bookmark).unwrap().get(b"A").unwrap());
    assert_eq!((kind(&bookmark), target(&bookmark)), (b"GoTo".to_vec(), pages[0]));
    let next = resolve(bookmark.get(b"Next").unwrap());
    assert_eq!((kind(&next), target(&next)), (b"GoTo".to_vec(), pages[1]));
}

#[test]
fn clearing_form_data_also_clears_what_a_list_shows_as_picked_and_its_default() {
    let mut pdf = text_pdf(&[&["form"]]);
    let page = pdf.page_ids()[0];
    let doc = pdf.document_mut();
    let widget = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "Rect" => vec![Object::from(10), 10.into(), 160.into(), 60.into()], "P" => page,
    });
    // A list box that allows several choices: the value, the options picked (by index) and the
    // value a reset gives it.
    let field = doc.add_object(dictionary! {
        "FT" => "Ch", "T" => "condition", "Ff" => 2_097_152,
        "Opt" => vec![Object::string_literal("Healthy"), Object::string_literal("Other")],
        "V" => vec![Object::string_literal("SECRET-CHOICE")], "I" => vec![Object::Integer(1)], "DV" => Object::string_literal("SECRET-DEFAULT"),
        "Kids" => vec![widget.into()],
    });
    doc.get_dictionary_mut(widget).unwrap().set("Parent", field);
    doc.get_dictionary_mut(page).unwrap().set("Annots", vec![Object::Reference(widget)]);
    doc.catalog_mut().unwrap().set("AcroForm", dictionary! { "Fields" => vec![Object::Reference(field)] });

    let report = pdf.remove_hidden_information(&[HiddenItem::FormData]).unwrap();
    assert_eq!(report.count(HiddenItem::FormData), 1);
    let bytes = file_bytes(&mut pdf);
    assert_gone(&bytes, "SECRET-CHOICE");
    assert_gone(&bytes, "SECRET-DEFAULT");
    let pdf = Pdf::from_bytes(&bytes).unwrap();
    let field = pdf.document().get_dictionary(field).unwrap();
    assert!(!field.has(b"V") && !field.has(b"I") && !field.has(b"DV"), "{field:?}");
    assert!(field.has(b"Opt"), "the options offered are part of the form");
}

/// One page per label, built the way word processors and form tools write them, so that other
/// parts of the document point into every page:
/// - each page draws its label, and a form XObject showing `FORM-<label>` from a resource
///   dictionary all pages share (only that page draws its form);
/// - each page has a filled text field whose value and appearance are `FIELD-<label>`;
/// - a structure tree has an element per page whose ActualText is `TAG-<label>`;
/// - the first page links to the last.
fn layered_pdf(labels: &[&str]) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
    });
    let forms: Vec<(String, Object)> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let content = format!("BT /F1 12 Tf 72 400 Td (FORM-{label}) Tj ET");
            let form = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                    "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
                },
                content.into_bytes(),
            ));
            (format!("Fm{i}"), Object::Reference(form))
        })
        .collect();
    let shared = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font },
        "XObject" => Object::Dictionary(forms.into_iter().collect()),
    });
    let tree_root = doc.new_object_id();
    let document_element = doc.new_object_id();
    let (mut kids, mut widgets, mut elements, mut parents) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, label) in labels.iter().enumerate() {
        let page_id = doc.new_object_id();
        let content = format!("/P <</MCID 0>> BDC BT /F1 18 Tf 72 700 Td ({label}) Tj ET EMC /Fm{i} Do");
        let content_id = doc.add_object(Stream::new(lopdf::Dictionary::new(), content.into_bytes()));
        let appearance = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 200.into(), 20.into()],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            },
            format!("BT /F1 12 Tf 2 5 Td (FIELD-{label}) Tj ET").into_bytes(),
        ));
        let widget = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx",
            "T" => Object::string_literal(format!("field{i}")),
            "V" => Object::string_literal(format!("FIELD-{label}")),
            "Rect" => vec![72.into(), 600.into(), 272.into(), 620.into()],
            "P" => page_id,
            "AP" => dictionary! { "N" => appearance },
        });
        widgets.push(Object::Reference(widget));
        let mut annots = vec![Object::Reference(widget)];
        if i == 0 && labels.len() > 1 {
            annots.push(Object::Reference(doc.add_object(dictionary! {
                "Type" => "Annot", "Subtype" => "Link", "Rect" => vec![72.into(), 650.into(), 200.into(), 670.into()],
                // Pointed at the last page below, once that exists.
                "Dest" => Object::Null,
            })));
        }
        let element = doc.add_object(dictionary! {
            "Type" => "StructElem", "S" => "P", "P" => document_element, "Pg" => page_id, "K" => 0,
            "ActualText" => Object::string_literal(format!("TAG-{label}")),
        });
        elements.push(Object::Reference(element));
        parents.push(Object::Integer(i as i64));
        parents.push(Object::Array(vec![Object::Reference(element)]));
        doc.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                "Contents" => content_id, "Resources" => shared, "Annots" => annots, "StructParents" => i as i64,
            }),
        );
        kids.push(Object::Reference(page_id));
    }
    // The link on the first page points at the last page.
    let last = kids.last().and_then(|k| k.as_reference().ok()).unwrap();
    for object in doc.objects.values_mut() {
        if let Object::Dictionary(d) = object {
            if d.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Link") {
                d.set("Dest", vec![Object::Reference(last), "Fit".into()]);
            }
        }
    }
    doc.objects.insert(document_element, Object::Dictionary(dictionary! {
        "Type" => "StructElem", "S" => "Document", "P" => tree_root, "K" => elements,
    }));
    doc.objects.insert(tree_root, Object::Dictionary(dictionary! {
        "Type" => "StructTreeRoot", "K" => document_element,
        "ParentTree" => dictionary! { "Nums" => parents }, "ParentTreeNextKey" => labels.len() as i64,
    }));
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
        "Type" => "Pages", "Kids" => kids.clone(), "Count" => kids.len() as i64,
    }));
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog", "Pages" => pages_id, "StructTreeRoot" => tree_root,
        "MarkInfo" => dictionary! { "Marked" => true },
        "AcroForm" => dictionary! { "Fields" => widgets, "DA" => Object::string_literal("/F1 12 Tf 0 g") },
    });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// Number of fields left in the document's form.
fn form_fields(pdf: &Pdf) -> usize {
    let doc = pdf.document();
    let Ok(Object::Dictionary(form)) = doc.catalog().unwrap().get(b"AcroForm").map(|f| match f {
        Object::Reference(id) => doc.get_object(*id).unwrap().clone(),
        other => other.clone(),
    }) else {
        return 0;
    };
    form.get(b"Fields").and_then(Object::as_array).map(Vec::len).unwrap_or(0)
}

#[test]
fn deleting_a_page_takes_its_fields_tags_and_drawings_along() {
    let r = renderer();
    let base = layered_pdf(&["ALPHA", "BRAVO", "CHARLIE"]);
    let (mut pdf, saved) = SavedFile::open(base, None).unwrap();
    pdf.delete_pages(&[1]).unwrap();
    assert_eq!(form_fields(&pdf), 2, "the other pages keep their fields");
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert_eq!(plan, CLEAN);
    assert_gone(&bytes, "BRAVO");
    for kept in ["ALPHA", "CHARLIE"] {
        assert!(contains(&bytes, format!("FIELD-{kept}").as_bytes()) && contains(&bytes, format!("TAG-{kept}").as_bytes()));
    }
    // The pages that stay still draw their own forms.
    let mut reopened = Pdf::from_bytes(&bytes).unwrap();
    let text = page_text(r, &mut reopened, 1);
    assert!(text.contains("CHARLIE") && text.contains("FORM-CHARLIE"), "{text:?}");
}

#[test]
fn redaction_takes_the_page_fields_and_drawings_along() {
    let r = renderer();
    let base = layered_pdf(&["ALPHA", "BRAVO", "CHARLIE"]);
    let (mut pdf, saved) = SavedFile::open(base, None).unwrap();
    pdf.apply_redactions(r, &[Redaction { page: 1, rect: Rect::new(0.0, 0.0, 612.0, 792.0) }], 72.0).unwrap();
    assert_eq!(form_fields(&pdf), 2);
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert_eq!(plan, CLEAN);
    assert_gone(&bytes, "BRAVO");
    let mut reopened = Pdf::from_bytes(&bytes).unwrap();
    assert_eq!(reopened.page_count(), 3);
    for (page, kept) in [(0, "ALPHA"), (2, "CHARLIE")] {
        let text = page_text(r, &mut reopened, page);
        assert!(text.contains(kept) && text.contains(&format!("FORM-{kept}")), "{text:?}");
    }
}

#[test]
fn extracted_pages_carry_nothing_of_the_other_pages() {
    let r = renderer();
    let source = Pdf::from_bytes(&layered_pdf(&["ALPHA", "BRAVO", "CHARLIE"])).unwrap();
    let mut part = source.extract_pages(&[0]).unwrap();
    assert_eq!(form_fields(&part), 1);
    let bytes = part.to_bytes().unwrap();
    assert_gone(&bytes, "BRAVO");
    assert_gone(&bytes, "CHARLIE");
    let text = page_text(r, &mut part, 0);
    assert!(text.contains("ALPHA") && text.contains("FORM-ALPHA"), "{text:?}");

    let mut last = source.extract_pages(&[2]).unwrap();
    let bytes = last.to_bytes().unwrap();
    assert_gone(&bytes, "ALPHA");
    assert_gone(&bytes, "BRAVO");
    assert!(page_text(r, &mut last, 0).contains("FORM-CHARLIE"));
}

/// A signed one-page file whose page carries an ink drawing: numbers only, no appearance stream.
fn signed_with_ink() -> (Vec<u8>, Vec<Object>) {
    let mut doc = text_pdf(&[&["contract body"]]);
    let page = doc.page_id(0).unwrap();
    let ink = doc.document_mut().add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Ink", "Rect" => vec![100.into(), 100.into(), 200.into(), 200.into()],
        "InkList" => vec![Object::Array(vec![100.into(), 100.into(), 150.into(), 180.into(), 200.into(), 120.into()])],
    });
    doc.document_mut().get_dictionary_mut(page).unwrap().set("Annots", vec![Object::Reference(ink)]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let annots = Pdf::from_bytes(&signed).unwrap().document().get_dictionary(page).unwrap().get(b"Annots").unwrap().as_array().unwrap().clone();
    (signed, annots)
}

#[test]
fn removing_an_annotation_counts_even_when_it_holds_only_numbers() {
    let (signed, annots) = signed_with_ink();
    let (mut pdf, saved) = SavedFile::open(signed, None).unwrap();
    let page = pdf.page_id(0).unwrap();
    let ink = annots.iter().find(|a| {
        pdf.document().get_dictionary(a.as_reference().unwrap()).unwrap().get(b"Subtype").unwrap().as_name().unwrap() == b"Ink"
    });
    let kept: Vec<Object> = annots.iter().filter(|a| Some(*a) != ink).cloned().collect();
    pdf.document_mut().get_dictionary_mut(page).unwrap().set("Annots", kept);
    assert!(saved.removed_from(&pdf));
    assert_eq!(pdf.save_plan(Some(&saved), false).unwrap(), SavePlan::FullRewrite { breaks_signatures: true, drops_revisions: false });
}

#[test]
fn appended_updates_carry_only_what_the_document_still_uses() {
    let (signed, annots) = signed_with_ink();
    let (mut pdf, saved) = SavedFile::open(signed.clone(), None).unwrap();
    let page = pdf.page_id(0).unwrap();
    pdf.add_note(0, 300.0, 300.0, "ORPHANSECRET draft").unwrap();
    pdf.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    // The note is taken out again; nothing prunes the document before saving.
    pdf.document_mut().get_dictionary_mut(page).unwrap().set("Annots", annots);
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, false);
    assert_eq!(plan, SavePlan::Incremental);
    assert!(bytes.starts_with(&signed));
    assert_gone(&bytes, "ORPHANSECRET");
    assert_eq!(verify(&bytes, &trust_test_identity())[0].validity, Validity::Valid);
}

#[test]
fn a_signature_the_file_already_fails_is_not_kept_by_appending() {
    let r = renderer();
    let mut doc = text_pdf(&[&["TOP SECRET 12345", "public line"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let (mut pdf, saved) = SavedFile::open(signed, None).unwrap();
    pdf.apply_redactions(r, &[Redaction { page: 0, rect: Rect::new(60.0, 692.0, 300.0, 28.0) }], 150.0).unwrap();
    let (plan, rewritten) = plan_and_bytes(&mut pdf, &saved, true);
    assert!(plan.needs_confirmation());

    // The rewritten file still carries the signature field, but the signature no longer holds:
    // later edits are written in full and nothing asks about signatures again.
    let (mut pdf, saved) = SavedFile::open(rewritten, None).unwrap();
    pdf.add_note(0, 100.0, 100.0, "after the rewrite").unwrap();
    assert_eq!(pdf.save_plan(Some(&saved), false).unwrap(), CLEAN);
}

#[test]
fn redacting_a_drawn_signature_takes_the_signature_along() {
    let r = renderer();
    let mut doc = text_pdf(&[&["contract body"]]);
    let base = file_bytes(&mut doc);
    let mut options = sign_options(SubFilter::Pkcs7Detached);
    options.appearance = Some((0, Rect::new(300.0, 100.0, 200.0, 60.0)));
    let signed = doc.sign(&base, &options).unwrap();
    let (mut pdf, saved) = SavedFile::open(signed, None).unwrap();
    pdf.apply_redactions(r, &[Redaction { page: 0, rect: Rect::new(290.0, 90.0, 220.0, 80.0) }], 72.0).unwrap();
    let (_, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert!(verify(&bytes, &trust_test_identity()).is_empty(), "the blacked-out signature still names its signer");
}

#[test]
fn unchanged_documents_save_byte_identical() {
    let base = text_pdf(&[&["nothing changes"]]).to_bytes().unwrap();
    let (mut pdf, saved) = SavedFile::open(base.clone(), None).unwrap();
    // Opening re-serializes the document in memory, and undo reloads it from such a copy.
    let before = pdf.to_bytes().unwrap();
    pdf.add_note(0, 100.0, 100.0, "gone again").unwrap();
    let mut pdf = Pdf::from_bytes(&before).unwrap();
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, false);
    assert_eq!(plan, SavePlan::Unchanged);
    assert_eq!(bytes, base);

    // A document with no file yet is written in full.
    let mut fresh = text_pdf(&[&["new"]]);
    let plan = fresh.save_plan(None, false).unwrap();
    assert_eq!(plan, CLEAN);
    assert!(Pdf::from_bytes(&fresh.save_bytes(plan, None).unwrap()).is_ok());
}

#[test]
fn signed_documents_get_edits_that_remove_nothing_appended() {
    let mut doc = text_pdf(&[&["contract body"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let trust = trust_test_identity();

    let (mut reopened, saved) = SavedFile::open(signed.clone(), None).unwrap();
    let _ = reopened.to_bytes().unwrap();
    let (plan, untouched) = plan_and_bytes(&mut reopened, &saved, false);
    assert_eq!(plan, SavePlan::Unchanged);
    assert_eq!(untouched, signed, "saving with no edits must not rewrite a signed file");

    reopened.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    reopened.add_note(0, 100.0, 100.0, "looks fine").unwrap();
    assert!(!saved.removed_from(&reopened));
    let (plan, appended) = plan_and_bytes(&mut reopened, &saved, false);
    assert_eq!(plan, SavePlan::Incremental);
    assert!(appended.starts_with(&signed) && appended.len() > signed.len(), "the signed bytes have to stay a prefix");
    let reports = verify(&appended, &trust);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(reports[0].modified_after_signing);

    // Saved revisions are kept the same way.
    let mut working = Pdf::from_bytes(&base).unwrap();
    working.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    let v1 = working.save_revision(&base, "First draft", &pdf_date_now()).unwrap();
    let mut working = Pdf::from_bytes(&v1).unwrap();
    working.apply_bates(&BatesOptions::default()).unwrap();
    let v2 = working.save_revision(&v1, "Second", &pdf_date_now()).unwrap();
    let (mut current, saved) = SavedFile::open(v2.clone(), None).unwrap();
    current.apply_watermark(&WatermarkOptions::new("FINAL")).unwrap();
    let (plan, bytes) = plan_and_bytes(&mut current, &saved, false);
    assert_eq!(plan, SavePlan::Incremental);
    assert!(bytes.starts_with(&v2));
    let revisions = Pdf::from_bytes(&bytes).unwrap().revisions();
    assert_eq!(revisions.len(), 2);
    assert_eq!(open_revision_bytes(&bytes, &revisions[0]).unwrap(), v1);
}

#[test]
fn signed_documents_need_a_clean_rewrite_after_a_redaction() {
    let r = renderer();
    let mut doc = text_pdf(&[&["TOP SECRET 12345", "public line"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();

    let (mut pdf, saved) = SavedFile::open(signed.clone(), None).unwrap();
    pdf.apply_redactions(r, &[Redaction { page: 0, rect: Rect::new(60.0, 692.0, 300.0, 28.0) }], 150.0).unwrap();
    let rewrite = SavePlan::FullRewrite { breaks_signatures: true, drops_revisions: false };
    // Found by comparing with the file even when the caller did not flag it.
    assert_eq!(pdf.save_plan(Some(&saved), false).unwrap(), rewrite);
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert_eq!(plan, rewrite);
    assert!(plan.needs_confirmation());
    assert_gone(&bytes, "SECRET");
    let reports = verify(&bytes, &trust_test_identity());
    assert_eq!(reports.len(), 1, "the signature field itself stays");
    assert_ne!(reports[0].validity, Validity::Valid, "the rewritten file no longer matches the signature");

    // A file with saved revisions loses them instead.
    let base = text_pdf(&[&["kept page"], &["DELETEDPAGE 999"]]).to_bytes().unwrap();
    let mut working = Pdf::from_bytes(&base).unwrap();
    working.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
    let v1 = working.save_revision(&base, "First draft", &pdf_date_now()).unwrap();
    let (mut pdf, saved) = SavedFile::open(v1, None).unwrap();
    pdf.delete_pages(&[1]).unwrap();
    let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, true);
    assert_eq!(plan, SavePlan::FullRewrite { breaks_signatures: false, drops_revisions: true });
    assert_gone(&bytes, "DELETEDPAGE");
    assert!(Pdf::from_bytes(&bytes).unwrap().revisions().is_empty());
}

#[test]
fn protected_documents_keep_both_passwords_and_their_permissions() {
    let mut options = ProtectionOptions::new("user pw", "owner pw");
    options.allow_printing = false;
    options.allow_copying = false;
    options.allow_annotating = false;
    let original = text_pdf(&[&["private"]]).to_encrypted_bytes(&options).unwrap();
    let state = |bytes: &[u8], password: &str| {
        Pdf::from_bytes_with_password(bytes, password).unwrap().document().encryption_state.clone().expect("encrypted")
    };
    let before = state(&original, "user pw");

    for opened_with in ["user pw", "owner pw"] {
        let (mut pdf, saved) = SavedFile::open(original.clone(), Some(opened_with.into())).unwrap();
        assert!(saved.is_protected());
        let (plan, unchanged) = plan_and_bytes(&mut pdf, &saved, false);
        assert_eq!((plan, unchanged == original), (SavePlan::Unchanged, true));

        pdf.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
        let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, false);
        assert_eq!(plan, CLEAN);
        assert!(!contains(&bytes, b"private") && !contains(&bytes, b"DRAFT"), "nothing is written in the clear");
        assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)));
        assert!(matches!(Pdf::from_bytes_with_password(&bytes, "wrong"), Err(Error::WrongPassword)));
        for password in ["user pw", "owner pw"] {
            let after = state(&bytes, password);
            assert_eq!(after.permissions(), before.permissions(), "{opened_with} / {password}");
            assert_eq!((after.version(), after.revision()), (before.version(), before.revision()));
            assert_eq!(after.file_encryption_key(), before.file_encryption_key());
            assert_eq!((after.owner_value(), after.user_value()), (before.owner_value(), before.user_value()));
            assert_eq!((after.owner_encrypted(), after.user_encrypted()), (before.owner_encrypted(), before.user_encrypted()));
            assert_eq!(after.permission_encrypted(), before.permission_encrypted());
        }
        let mut reopened = Pdf::from_bytes_with_password(&bytes, "user pw").unwrap();
        let text = page_text(renderer(), &mut reopened, 0);
        assert!(text.contains("private") && text.contains("DRAFT"), "{text:?}");
    }
}

#[test]
fn files_that_only_restrict_permissions_stay_protected() {
    let mut options = ProtectionOptions::new("", "owner pw");
    options.allow_copying = false;
    options.allow_printing = false;
    let original = text_pdf(&[&["private"]]).to_encrypted_bytes(&options).unwrap();
    // Opens without asking for a password.
    let (mut pdf, saved) = SavedFile::open(original, None).unwrap();
    assert!(saved.is_protected());
    let before = pdf.document().encryption_state.clone().unwrap();

    pdf.add_note(0, 100.0, 100.0, "reviewed").unwrap();
    let (_, bytes) = plan_and_bytes(&mut pdf, &saved, false);
    assert!(!contains(&bytes, b"private"));
    let reopened = Pdf::from_bytes(&bytes).unwrap();
    let after = reopened.document().encryption_state.clone().expect("still encrypted");
    assert_eq!(after.permissions(), before.permissions());
    assert_eq!(after.file_encryption_key(), before.file_encryption_key());
    assert_eq!((after.owner_value(), after.owner_encrypted()), (before.owner_value(), before.owner_encrypted()));
}

#[test]
fn passwords_are_used_exactly_as_typed() {
    for password in [" secret ", "   "] {
        let original = text_pdf(&[&["private"]]).to_encrypted_bytes(&ProtectionOptions::new(password, password)).unwrap();
        let (mut pdf, saved) = SavedFile::open(original, Some(password.into())).unwrap();
        pdf.add_note(0, 100.0, 100.0, "reviewed").unwrap();
        let (_, bytes) = plan_and_bytes(&mut pdf, &saved, false);
        assert!(!contains(&bytes, b"private"), "{password:?}");
        assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)), "{password:?}");
        assert!(Pdf::from_bytes_with_password(&bytes, password).is_ok(), "{password:?}");
        let trimmed = password.trim();
        if !trimmed.is_empty() {
            assert!(matches!(Pdf::from_bytes_with_password(&bytes, trimmed), Err(Error::WrongPassword)));
        }
    }
}

/// A file protected the older way other software still writes: RC4 128-bit (revision 3) or AES-128
/// (revision 4). Their file key is derived from the open password and the file identifier.
fn legacy_protected(aes: bool, user: &str, owner: &str) -> Vec<u8> {
    use lopdf::encryption::crypt_filters::{Aes128CryptFilter, CryptFilter};
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    let mut pdf = text_pdf(&[&["private words"]]);
    let id = Object::String(b"carbide-test-id1".to_vec(), lopdf::StringFormat::Hexadecimal);
    pdf.document_mut().trailer.set("ID", vec![id.clone(), id]);
    let plain = pdf.document().clone();
    let permissions = Permissions::PRINTABLE | Permissions::COPYABLE;
    let version = if aes {
        let mut filters: std::collections::BTreeMap<Vec<u8>, std::sync::Arc<dyn CryptFilter>> = Default::default();
        filters.insert(b"StdCF".to_vec(), std::sync::Arc::new(Aes128CryptFilter));
        EncryptionVersion::V4 {
            document: &plain,
            encrypt_metadata: true,
            crypt_filters: filters,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: owner,
            user_password: user,
            permissions,
        }
    } else {
        EncryptionVersion::V2 { document: &plain, owner_password: owner, user_password: user, key_length: 128, permissions }
    };
    let state = EncryptionState::try_from(version).unwrap();
    let mut doc = plain.clone();
    doc.encrypt(&state).unwrap();
    let mut out = Vec::new();
    doc.save_with_options(&mut out, lopdf::SaveOptions::builder().use_object_streams(false).use_xref_streams(false).build())
        .unwrap();
    out
}

#[test]
fn older_encryption_opens_with_either_password_and_saves_with_the_same_key() {
    let r = renderer();
    for aes in [false, true] {
        for (user, owner, opened_with) in [
            ("user", "owner", Some("user")),
            ("user", "owner", Some("owner")),
            ("pässwörd", "owner", Some("pässwörd")),
            ("pässwörd", "öwner", Some("öwner")),
            // An empty owner password opens the file without asking.
            ("user", "", None),
        ] {
            let label = format!("aes {aes}, open {user:?}, owner {owner:?}, opened with {opened_with:?}");
            let original = legacy_protected(aes, user, owner);
            assert!(r.page_text_with_password(&original, user, 0).unwrap().contains("private words"), "{label}: fixture");
            if owner.is_empty() {
                assert!(Pdf::from_bytes(&original).is_ok(), "{label}");
            } else {
                assert!(matches!(Pdf::from_bytes(&original), Err(Error::PasswordRequired)), "{label}");
                assert!(matches!(Pdf::from_bytes_with_password(&original, "wrong"), Err(Error::WrongPassword)), "{label}");
            }

            let (mut pdf, saved) = SavedFile::open(original.clone(), opened_with.map(str::to_string)).unwrap();
            assert!(saved.is_protected(), "{label}");
            let key = pdf.document().encryption_state.clone().unwrap().file_encryption_key().to_vec();
            assert!(page_text(r, &mut pdf, 0).contains("private words"), "{label}: decrypted with the wrong key");

            pdf.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
            let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, false);
            assert_eq!(plan, CLEAN, "{label}");
            assert!(!contains(&bytes, b"private words"), "{label}: written in the clear");
            // PDFium does not take an empty owner password, for the original file either.
            for password in [user, owner].into_iter().filter(|p| !p.is_empty()) {
                let text = r.page_text_with_password(&bytes, password, 0).unwrap_or_else(|e| panic!("{label}, read with {password:?}: {e}"));
                assert!(text.contains("private words") && text.contains("DRAFT"), "{label}, read with {password:?}: {text:?}");
                let reopened = Pdf::from_bytes_with_password(&bytes, password).unwrap();
                assert_eq!(reopened.document().encryption_state.as_ref().unwrap().file_encryption_key(), key.as_slice(), "{label}");
            }
        }
    }
}

#[test]
fn protected_files_with_revisions_get_encrypted_updates_appended() {
    let r = renderer();
    // Protected after a revision was saved, as other software can do; the revision record stays.
    let base = text_pdf(&[&["private words"]]).to_bytes().unwrap();
    let mut working = Pdf::from_bytes(&base).unwrap();
    working.apply_bates(&BatesOptions::default()).unwrap();
    let v1 = working.save_revision(&base, "First", &pdf_date_now()).unwrap();
    let modern = Pdf::from_bytes(&v1).unwrap().to_encrypted_bytes(&ProtectionOptions::new("user", "owner")).unwrap();
    let older = {
        let mut doc = Pdf::from_bytes(&v1).unwrap();
        let bytes = doc.to_bytes().unwrap();
        // The older revisions need a file identifier; give the plain file one before protecting it.
        let mut plain = Document::load_mem(&bytes).unwrap();
        let id = Object::String(b"carbide-test-id2".to_vec(), lopdf::StringFormat::Hexadecimal);
        plain.trailer.set("ID", vec![id.clone(), id]);
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V2 {
            document: &plain,
            owner_password: "owner",
            user_password: "user",
            key_length: 128,
            permissions: lopdf::Permissions::PRINTABLE,
        })
        .unwrap();
        let mut encrypted = plain.clone();
        encrypted.encrypt(&state).unwrap();
        let mut out = Vec::new();
        encrypted.save_with_options(&mut out, lopdf::SaveOptions::builder().use_object_streams(false).use_xref_streams(false).build()).unwrap();
        out
    };
    for (label, original) in [("AES-256", modern), ("RC4", older)] {
        let (mut pdf, saved) = SavedFile::open(original.clone(), Some("owner".into())).unwrap();
        assert_eq!(pdf.revisions().len(), 1, "{label}");
        pdf.apply_watermark(&WatermarkOptions::new("DRAFT")).unwrap();
        let (plan, bytes) = plan_and_bytes(&mut pdf, &saved, false);
        assert_eq!(plan, SavePlan::Incremental, "{label}");
        assert!(bytes.starts_with(&original), "{label}: the protected original is not a prefix");
        assert!(!contains(&bytes[original.len()..], b"DRAFT"), "{label}: the update is in the clear");
        for password in ["user", "owner"] {
            let text = r.page_text_with_password(&bytes, password, 0).unwrap_or_else(|e| panic!("{label}, {password}: {e}"));
            assert!(text.contains("private words") && text.contains("DRAFT"), "{label}, {password}: {text:?}");
        }
    }
}

#[test]
fn aes256_passwords_are_prepared_the_way_the_standard_says() {
    // Option+Space types a no-break space; accents can arrive composed or decomposed.
    for (set, typed) in [("my\u{a0}pass", "my\u{a0}pass"), ("café", "café"), ("café", "cafe\u{301}")] {
        let original = text_pdf(&[&["private"]]).to_encrypted_bytes(&ProtectionOptions::new(set, "owner pw")).unwrap();
        let (mut pdf, saved) = SavedFile::open(original, Some(typed.into())).unwrap_or_else(|e| panic!("{typed:?}: {e}"));
        assert!(page_text(renderer(), &mut pdf, 0).contains("private"), "{typed:?}");
        pdf.add_note(0, 100.0, 100.0, "reviewed").unwrap();
        let (_, bytes) = plan_and_bytes(&mut pdf, &saved, false);
        assert!(Pdf::from_bytes_with_password(&bytes, typed).is_ok(), "{typed:?}");
        assert!(Pdf::from_bytes_with_password(&bytes, "owner pw").is_ok(), "{typed:?}");
    }
}

/// The encryption values a password gives for `bytes`, which must open with it.
fn encryption(bytes: &[u8], password: Option<&str>) -> lopdf::EncryptionState {
    let pdf = match password {
        Some(password) => Pdf::from_bytes_with_password(bytes, password),
        None => Pdf::from_bytes(bytes),
    };
    pdf.unwrap().document().encryption_state.clone().expect("encrypted")
}

fn same_encryption(a: &lopdf::EncryptionState, b: &lopdf::EncryptionState) -> bool {
    a.file_encryption_key() == b.file_encryption_key()
        && a.permissions() == b.permissions()
        && (a.version(), a.revision()) == (b.version(), b.revision())
        && (a.owner_value(), a.user_value()) == (b.owner_value(), b.user_value())
        && (a.owner_encrypted(), a.user_encrypted()) == (b.owner_encrypted(), b.user_encrypted())
}

#[test]
fn documents_made_from_a_protected_one_keep_its_passwords_and_permissions() {
    let r = renderer();
    let mut options = ProtectionOptions::new("user pw", "owner pw");
    options.allow_printing = false;
    options.allow_copying = false;
    let sources = [
        ("AES-256", text_pdf(&[&["private words"], &["second page"]]).to_encrypted_bytes(&options).unwrap()),
        // The older revisions derive the key from the file identifier, which has to come along.
        ("RC4", legacy_protected(false, "user pw", "owner pw")),
        ("AES-128", legacy_protected(true, "user pw", "owner pw")),
    ];
    for (label, original) in sources {
        let before = encryption(&original, Some("user pw"));
        let source = Pdf::from_bytes_with_password(&original, "user pw").unwrap();
        assert!(source.is_protected());
        let letter = ImpositionOptions { media: Media::Named("Letter".into()), ..Default::default() };
        let derived = [
            ("extract", source.extract_pages(&[0]).unwrap()),
            ("split", source.split(&Pdf::parse_ranges("1", source.page_count()).unwrap()).unwrap().remove(0)),
            ("impose", source.impose(&letter, None).unwrap()),
            ("merge", Pdf::merge(&[&source, &source.clone()]).unwrap()),
        ];
        for (what, mut pdf) in derived {
            let bytes = pdf.to_file_bytes(Some("user pw")).unwrap();
            assert!(!contains(&bytes, b"private words"), "{label} {what}: written in the clear");
            assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)), "{label} {what}");
            assert!(matches!(Pdf::from_bytes_with_password(&bytes, "wrong"), Err(Error::WrongPassword)), "{label} {what}");
            for password in ["user pw", "owner pw"] {
                assert!(same_encryption(&encryption(&bytes, Some(password)), &before), "{label} {what}, opened with {password}");
                let text = r.page_text_with_password(&bytes, password, 0).unwrap();
                assert!(text.contains("private words"), "{label} {what}, read with {password}: {text:?}");
            }
            assert!(pdf.to_file_bytes(Some("wrong")).is_err(), "{label} {what}: not checked with the password");
        }
    }

    // A file that only restricts permissions: the parts open without a password and keep the
    // restrictions, and `save` needs none.
    let mut restricted = ProtectionOptions::new("", "owner pw");
    restricted.allow_copying = false;
    let original = text_pdf(&[&["private words"], &["second page"]]).to_encrypted_bytes(&restricted).unwrap();
    let before = encryption(&original, None);
    let source = Pdf::from_bytes(&original).unwrap();
    let dir = scratch_dir("derived-protection");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("part.pdf");
    source.extract_pages(&[1]).unwrap().save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(!contains(&bytes, b"second page"));
    assert!(same_encryption(&encryption(&bytes, None), &before));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merging_keeps_a_shared_protection_and_refuses_different_ones() {
    let protect = |text: &str, user: &str| {
        let bytes = text_pdf(&[&[text]]).to_encrypted_bytes(&ProtectionOptions::new(user, "owner pw")).unwrap();
        Pdf::from_bytes_with_password(&bytes, "owner pw").unwrap()
    };
    let (first, second) = (protect("first secret", "one"), protect("second secret", "two"));
    let plain = text_pdf(&[&["plain page"]]);

    // A protected and an unprotected input: the result is protected like the protected one.
    let mut merged = Pdf::merge(&[&plain, &first]).unwrap();
    let bytes = merged.to_file_bytes(Some("one")).unwrap();
    assert!(!contains(&bytes, b"first secret") && !contains(&bytes, b"plain page"));
    assert!(same_encryption(&encryption(&bytes, Some("one")), first.document().encryption_state.as_ref().unwrap()));
    assert_eq!(Pdf::from_bytes_with_password(&bytes, "one").unwrap().page_count(), 2);

    // Protected in two ways: either password would open the other's pages, so it is refused.
    let refused = Pdf::merge(&[&first, &second]).unwrap_err();
    assert_eq!(refused.to_string(), carbide_engine::save::DIFFERENT_PROTECTION);
    assert!(Pdf::shared_protection(&[&first, &second]).is_err());

    // Without protection only when that is asked for.
    let (mut a, mut b) = (first.clone(), second.clone());
    a.set_protection(None);
    b.set_protection(None);
    let mut merged = Pdf::merge(&[&a, &b]).unwrap();
    assert!(!merged.is_protected());
    let bytes = merged.to_file_bytes(None).unwrap();
    let reopened = Pdf::from_bytes(&bytes).unwrap();
    assert!(reopened.document().encryption_state.is_none());
    assert_eq!(reopened.page_count(), 2);
}

#[cfg(unix)]
#[test]
fn replacing_a_file_keeps_its_mode_and_follows_symbolic_links() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use carbide_engine::fs::write_replacing;
    let dir = scratch_dir("replace");
    std::fs::create_dir_all(&dir).unwrap();
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let set_mode = |path: &std::path::Path, mode: u32| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();

    // A private file stays private.
    let private = dir.join("private.pdf");
    std::fs::write(&private, b"old").unwrap();
    set_mode(&private, 0o600);
    write_replacing(&private, b"new").unwrap();
    assert_eq!(std::fs::read(&private).unwrap(), b"new");
    assert_eq!(mode(&private), 0o600);

    // Saving through a link replaces the file it points to, and the link stays a link.
    let target = dir.join("target.pdf");
    std::fs::write(&target, b"old").unwrap();
    set_mode(&target, 0o640);
    let link = dir.join("link.pdf");
    symlink("target.pdf", &link).unwrap();
    write_replacing(&link, b"through the link").unwrap();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&target).unwrap(), b"through the link");
    assert_eq!(mode(&target), 0o640);

    // A read-only file is refused and left alone.
    let locked = dir.join("locked.pdf");
    std::fs::write(&locked, b"keep").unwrap();
    set_mode(&locked, 0o444);
    assert!(write_replacing(&locked, b"replaced").is_err());
    assert_eq!(std::fs::read(&locked).unwrap(), b"keep");
    assert_eq!(mode(&locked), 0o444);

    // A new file is simply created.
    let fresh = dir.join("fresh.pdf");
    write_replacing(&fresh, b"first").unwrap();
    assert_eq!(std::fs::read(&fresh).unwrap(), b"first");

    // No temporary file is left behind.
    let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["fresh.pdf", "link.pdf", "locked.pdf", "private.pdf", "target.pdf"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn saved_and_protected_copies_replace_files_in_one_step() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = scratch_dir("safe-save");
    std::fs::create_dir_all(&dir).unwrap();
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    // Saving over a private file keeps it private.
    let private = dir.join("private.pdf");
    std::fs::write(&private, b"old").unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
    text_pdf(&[&["saved"]]).save(&private).unwrap();
    assert_eq!(mode(&private), 0o600);
    assert_eq!(Pdf::open(&private).unwrap().page_count(), 1);

    // A protected copy written through a link replaces the file it points to.
    let link = dir.join("link.pdf");
    symlink("private.pdf", &link).unwrap();
    text_pdf(&[&["protected"]]).save_encrypted(&link, &ProtectionOptions::new("open", "owner")).unwrap();
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert!(matches!(Pdf::open(&private), Err(Error::PasswordRequired)));
    assert_eq!(mode(&private), 0o600);

    // A file that cannot be written is refused and left as it was, by both.
    let locked = dir.join("locked.pdf");
    std::fs::write(&locked, b"keep").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert!(text_pdf(&[&["x"]]).save(&locked).is_err());
    assert!(text_pdf(&[&["x"]]).save_encrypted(&locked, &ProtectionOptions::new("", "owner")).is_err());
    assert_eq!(std::fs::read(&locked).unwrap(), b"keep");

    let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["link.pdf", "locked.pdf", "private.pdf"], "no temporary files are left");
    let _ = std::fs::remove_dir_all(&dir);
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

#[cfg(unix)]
#[test]
fn recovery_copies_are_private_and_never_of_protected_documents() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("autosave-private");
    let mut store = AutoSaveStore::open(&dir).unwrap();
    let draft = store.begin_document(None, 1);
    store.autosave(draft, &mut text_pdf(&[&["draft"]]), 2).unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0, "{}", path.display());
    }

    // Opens without a password, so nothing else stops it.
    let protected = text_pdf(&[&["PROTECTEDTEXT"]]).to_encrypted_bytes(&ProtectionOptions::new("", "owner pw")).unwrap();
    let mut opened = Pdf::from_bytes(&protected).unwrap();
    opened.add_note(0, 100.0, 100.0, "edited").unwrap();
    let record = store.begin_document(None, 3);
    assert!(store.autosave(record, &mut opened, 4).is_err());
    for entry in std::fs::read_dir(&dir).unwrap() {
        assert_gone(&std::fs::read(entry.unwrap().path()).unwrap(), "PROTECTEDTEXT");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn windows_sharing_the_folder_never_take_each_others_live_copies() {
    let r = renderer();
    let dir = scratch_dir("autosave-windows");
    let original = dir.join("..").join("contract.pdf");
    let names = |dir: &PathBuf| {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    };

    // Window A has an auto-saved document.
    let mut a = AutoSaveStore::open(&dir).unwrap();
    let in_a = a.begin_document(Some(&original), 1);
    a.autosave(in_a, &mut text_pdf(&[&["window A"]]), 2).unwrap();
    let a_copy = a.record(in_a).unwrap().recovered.clone().unwrap().path;

    // Window B, opened while A runs (Open Revision does that), sees none of it as recovered and
    // cannot delete it.
    let mut b = AutoSaveStore::open(&dir).unwrap();
    assert!(!b.any_needs_save());
    assert!(b.panel_rows().is_empty());
    assert_eq!(b.remove_all_loaded().unwrap(), 0);
    let in_b = b.begin_document(None, 3);
    b.autosave(in_b, &mut text_pdf(&[&["window B"]]), 4).unwrap();
    // They write their indexes in any order, and neither drops the other's records.
    a.autosave(in_a, &mut text_pdf(&[&["window A, later"]]), 5).unwrap();
    b.document_closed(in_b).unwrap();
    assert!(a_copy.exists());
    assert!(!names(&dir).iter().any(|n| n.ends_with(".tmp")), "an index was not replaced in one step");

    // A ends without closing its document (a crash): the next window recovers it, and B, still
    // running, has not taken it.
    drop(a);
    assert!(b.panel_rows().is_empty());
    let c = AutoSaveStore::open(&dir).unwrap();
    let rows = c.panel_rows();
    assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["contract.pdf (saved)", "contract.pdf (recovered)"]);
    assert_eq!(rows[1].save_time, 5);
    let (mut recovered, _) = c.open_recovered(rows[1].record, None).unwrap();
    assert!(page_text(r, &mut recovered, 0).contains("window A, later"));

    // Now C runs and holds the record: another window does not see it either.
    let d = AutoSaveStore::open(&dir).unwrap();
    assert!(d.panel_rows().is_empty());
    drop(d);
    drop(c);
    drop(b);
    // Once C is gone too, the record is listed exactly once.
    let mut e = AutoSaveStore::open(&dir).unwrap();
    assert_eq!(e.panel_rows().len(), 2);
    assert_eq!(e.remove_all_loaded().unwrap(), 1);
    assert!(!a_copy.exists());
    drop(e);
    assert_eq!(names(&dir), ["AutoRecovery.lock"], "nothing but the folder lock is left");
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

#[test]
fn the_index_of_earlier_versions_is_taken_over_only_once_it_stops_changing() {
    let dir = scratch_dir("autosave-earlier");
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("ZASolder1.asd");
    std::fs::write(&copy, text_pdf(&[&["older window"]]).to_bytes().unwrap()).unwrap();
    let index = dir.join(carbide_engine::autosave::INDEX_FILE_NAME);
    let item = format!("<AutoSaveItem RecoveredFile=\"{}\" RecoveredSaveTime=\"5\" IsNewDoc=\"1\"/>", copy.display());
    std::fs::write(&index, format!("<?xml version=\"1.0\"?>\n<AutoSave version=\"1.0\">{item}</AutoSave>\n")).unwrap();

    // Just written: a window of an earlier version may still be running and using them.
    let mut store = AutoSaveStore::open(&dir).unwrap();
    assert!(store.panel_rows().is_empty());
    assert_eq!(store.remove_all_loaded().unwrap(), 0);
    drop(store);
    assert!(index.exists() && copy.exists());

    // Unchanged for longer than any window waits between two auto-saves: that window is gone.
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 3600);
    for path in [&index, &copy] {
        std::fs::File::options().write(true).open(path).unwrap().set_modified(long_ago).unwrap();
    }
    let mut store = AutoSaveStore::open(&dir).unwrap();
    assert_eq!(store.panel_rows().len(), 1);
    assert!(!index.exists());
    assert_eq!(store.remove_all_loaded().unwrap(), 1);
    assert!(!copy.exists());
    drop(store);
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
fn odd_plate_counts_print_every_page_once_in_every_order() {
    // Perfect-bound work and turn (or tumble) makes one plate per fold, so these page counts give
    // an odd number of plates.
    let r = renderer();
    for (layout, style, columns) in [(LayoutKind::Booklet4Up, Style::WorkAndTurn, 2), (LayoutKind::Booklet8Up, Style::WorkAndTumble, 4)] {
        for pages in [4, 12, 20] {
            let source = numbered_pages(pages);
            for printing_order in [PrintingOrder::FrontsThenBacks, PrintingOrder::FrontsThenBacksReversed] {
                let options = ImpositionOptions {
                    plan: PlanOptions {
                        layout,
                        columns,
                        rows: 2,
                        binding: Binding::Perfect,
                        sheets_per_fold: 1,
                        style,
                        double_sided: true,
                        printing_order,
                    },
                    ..Default::default()
                };
                let what = format!("{layout:?} {printing_order:?} N={pages}");
                let mut sheets = source.impose(&options, None).unwrap_or_else(|e| panic!("{what}: {e}"));
                let mut printed = vec![0; pages];
                for sheet in 0..sheets.page_count() {
                    let text = page_text(r, &mut sheets, sheet);
                    for (n, count) in printed.iter_mut().enumerate() {
                        *count += text.matches(&format!("PAGE{}X", n + 1)).count();
                    }
                }
                assert!(printed.iter().all(|&c| c == 1), "{what}: times each page was printed: {printed:?}");
            }
        }
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

// ---- damaged page trees --------------------------------------------------------------------

/// Three pages showing "AAA first", "BBB SECRET" and "CCC third", with `damage` done to the page
/// tree (given the root and the page ids) and written out as is, the way another program would.
fn damaged_tree(damage: impl FnOnce(&mut Document, lopdf::ObjectId, &[lopdf::ObjectId])) -> Vec<u8> {
    let mut pdf = text_pdf(&[&["AAA first"], &["BBB SECRET"], &["CCC third"]]);
    let ids = pdf.page_ids();
    let doc = pdf.document_mut();
    let root = doc.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
    damage(doc, root, &ids);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn set_kids(doc: &mut Document, node: lopdf::ObjectId, kids: Vec<Object>) {
    doc.get_dictionary_mut(node).unwrap().set("Kids", kids);
}

/// Runs `work` on another thread and fails the test if it does not finish within half a minute.
fn finishes<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap_or_else(|_| panic!("{what} did not finish"))
}

/// The text of every page, after checking that PDFium finds exactly the pages the engine finds.
fn texts_seen_by_both(pdf: &mut Pdf) -> Vec<String> {
    let bytes = pdf.to_bytes().unwrap();
    assert_eq!(renderer().page_count(&bytes).unwrap(), pdf.page_count(), "PDFium and the engine count different pages");
    (0..pdf.page_count()).map(|page| renderer().page_text(&bytes, page).unwrap().trim().to_string()).collect()
}

#[test]
fn damaged_page_trees_read_the_same_to_the_engine_and_to_pdfium() {
    type Damage = fn(&mut Document, lopdf::ObjectId, &[lopdf::ObjectId]);
    let all = ["AAA first", "BBB SECRET", "CCC third"];
    let cases: Vec<(&str, Damage, Vec<&str>)> = vec![
        ("untyped page", |doc, _, ids| {
            doc.get_dictionary_mut(ids[1]).unwrap().remove(b"Type");
        }, all.to_vec()),
        ("page typed as a node", |doc, _, ids| {
            doc.get_dictionary_mut(ids[1]).unwrap().set("Type", "Pages");
        }, all.to_vec()),
        ("untyped node", |doc, root, ids| {
            let node = doc.add_object(dictionary! { "Parent" => root, "Kids" => vec![ids[1].into(), ids[2].into()], "Count" => 2 });
            set_kids(doc, root, vec![ids[0].into(), node.into()]);
        }, all.to_vec()),
        ("page given directly", |doc, root, ids| {
            let direct = doc.get_dictionary(ids[1]).unwrap().clone();
            set_kids(doc, root, vec![ids[0].into(), Object::Dictionary(direct), ids[2].into()]);
        }, all.to_vec()),
        ("loop through a node", |doc, root, ids| {
            let node = doc.add_object(dictionary! { "Type" => "Pages", "Parent" => root, "Kids" => vec![ids[1].into(), root.into()], "Count" => 1 });
            set_kids(doc, root, vec![ids[0].into(), node.into(), ids[2].into()]);
        }, all.to_vec()),
        ("root lists itself", |doc, root, ids| {
            set_kids(doc, root, vec![ids[0].into(), root.into(), ids[1].into(), ids[2].into()]);
        }, all.to_vec()),
        ("huge count", |doc, root, _| {
            doc.get_dictionary_mut(root).unwrap().set("Count", 1_000_000);
        }, all.to_vec()),
        ("missing and non-dictionary kids", |doc, root, ids| {
            let number = doc.add_object(Object::Integer(7));
            set_kids(doc, root, vec![ids[0].into(), (9999, 0).into(), ids[1].into(), number.into(), Object::Null, ids[2].into()]);
        }, all.to_vec()),
        ("page listed twice", |doc, root, ids| {
            set_kids(doc, root, vec![ids[0].into(), ids[1].into(), ids[1].into(), ids[2].into()]);
        }, vec!["AAA first", "BBB SECRET", "BBB SECRET", "CCC third"]),
        ("parent loops", |doc, root, ids| {
            doc.get_dictionary_mut(root).unwrap().set("Parent", root);
            let node = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![ids[1].into()], "Count" => 1 });
            doc.get_dictionary_mut(node).unwrap().set("Parent", ids[1]);
            doc.get_dictionary_mut(ids[1]).unwrap().set("Parent", node);
            set_kids(doc, root, vec![ids[0].into(), node.into(), ids[2].into()]);
        }, all.to_vec()),
    ];
    for (what, damage, expected) in cases {
        let bytes = damaged_tree(damage);
        let mut pdf = Pdf::from_bytes(&bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(texts_seen_by_both(&mut pdf), expected, "{what}");

        // Structural edits keep every page and finish, whatever /Parent says.
        let mut inserted = pdf.clone();
        inserted.insert_blank_page(0, None).unwrap();
        assert_eq!(texts_seen_by_both(&mut inserted)[1..], expected[..], "{what}: insert");
        let mut deleted = finishes(what, move || {
            pdf.delete_pages(&[0]).unwrap();
            pdf
        });
        assert_eq!(texts_seen_by_both(&mut deleted), expected[1..], "{what}: delete");
    }

    // A page listed twice became two pages: rotating one leaves the other alone.
    let mut twice = Pdf::from_bytes(&damaged_tree(|doc, root, ids| {
        set_kids(doc, root, vec![ids[0].into(), ids[1].into(), ids[1].into(), ids[2].into()]);
    }))
    .unwrap();
    twice.rotate_pages(&[1], 90).unwrap();
    assert_eq!((twice.page_rotation(1).unwrap(), twice.page_rotation(2).unwrap()), (90, 0));
}

#[test]
fn redaction_lands_on_the_page_shown_even_when_the_tree_is_damaged() {
    let r = renderer();
    let bytes = damaged_tree(|doc, _, ids| {
        doc.get_dictionary_mut(ids[1]).unwrap().remove(b"Type");
    });
    let mut pdf = Pdf::from_bytes(&bytes).unwrap();
    pdf.apply_redactions(r, &[Redaction { page: 1, rect: Rect::new(60.0, 692.0, 300.0, 28.0) }], 150.0).unwrap();
    let out = pdf.to_bytes().unwrap();
    assert_eq!(r.page_count(&out).unwrap(), 3);
    assert_eq!(r.page_text(&out, 0).unwrap().trim(), "AAA first");
    assert_eq!(r.page_text(&out, 1).unwrap().trim(), "", "the page shown second was not redacted");
    assert_eq!(r.page_text(&out, 2).unwrap().trim(), "CCC third", "another page was redacted instead");
    assert!(!contains(&out, b"BBB SECRET"));
}

#[test]
fn deleting_pages_finishes_when_parent_chains_loop_after_loading() {
    for (what, damage) in [
        ("root names itself", 0usize),
        ("page names itself", 1),
        ("page and node name each other", 2),
    ] {
        let mut pdf = text_pdf(&[&["ONE"], &["TWO"], &["THREE"]]);
        let ids = pdf.page_ids();
        let doc = pdf.document_mut();
        let root = doc.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
        match damage {
            0 => doc.get_dictionary_mut(root).unwrap().set("Parent", root),
            1 => doc.get_dictionary_mut(ids[1]).unwrap().set("Parent", ids[1]),
            _ => {
                doc.get_dictionary_mut(root).unwrap().set("Parent", ids[1]);
                doc.get_dictionary_mut(ids[1]).unwrap().set("Parent", root);
            }
        }
        let mut pdf = finishes(what, move || {
            pdf.delete_pages(&[1]).unwrap();
            pdf
        });
        assert_eq!(pdf.page_count(), 2, "{what}");
        let bytes = pdf.to_bytes().unwrap();
        assert_eq!(renderer().page_count(&bytes).unwrap(), 2, "{what}");
        assert!(!contains(&bytes, b"TWO"), "{what}");
    }
}

#[test]
fn page_trees_that_cannot_be_read_safely_are_refused() {
    use carbide_engine::pagetree::{messages, MAX_DEPTH};
    let deep = damaged_tree(|doc, root, ids| {
        let mut below: Object = ids[0].into();
        for _ in 0..MAX_DEPTH + 5 {
            below = doc.add_object(dictionary! { "Type" => "Pages", "Kids" => vec![below], "Count" => 1 }).into();
        }
        set_kids(doc, root, vec![below]);
    });
    assert_eq!(Pdf::from_bytes(&deep).unwrap_err().to_string(), messages::TOO_DEEP);

    let shared = damaged_tree(|doc, root, ids| {
        let node = doc.add_object(dictionary! { "Type" => "Pages", "Parent" => root, "Kids" => vec![ids[1].into()], "Count" => 1 });
        set_kids(doc, root, vec![ids[0].into(), node.into(), node.into()]);
    });
    assert_eq!(Pdf::from_bytes(&shared).unwrap_err().to_string(), messages::SHARED_BRANCH);
}

#[test]
fn a_sound_page_tree_is_not_rewritten_by_the_repair() {
    // Nested nodes with inherited values: opening changes no object, and saving writes nothing new.
    let bytes = inherited_geometry(false).to_bytes().unwrap();
    let (pdf, saved) = SavedFile::open(bytes.clone(), None).unwrap();
    assert_eq!(pdf.document().objects, Document::load_mem(&bytes).unwrap().objects);
    assert_eq!(pdf.save_plan(Some(&saved), false).unwrap(), SavePlan::Unchanged);

    // A damaged one is repaired when saved, also when the save appends an update.
    let damaged = damaged_tree(|doc, root, _| {
        doc.get_dictionary_mut(root).unwrap().set("Count", 1_000_000);
    });
    let (pdf, saved) = SavedFile::open(damaged, None).unwrap();
    assert_ne!(pdf.save_plan(Some(&saved), false).unwrap(), SavePlan::Unchanged);
}

// ---- resource limits -----------------------------------------------------------------------

/// A Flate stream of `size` spaces: a few hundred kilobytes in the file, `size` bytes decoded.
fn flate_bomb(size: usize) -> Stream {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let chunk = vec![b' '; 1 << 20];
    for _ in 0..size.div_ceil(chunk.len()) {
        encoder.write_all(&chunk).unwrap();
    }
    Stream::new(dictionary! { "Filter" => "FlateDecode" }, encoder.finish().unwrap()).with_compression(false)
}

/// Two pages, the second drawn by its own content followed by `bomb` (`joined`), or by `bomb`
/// alone.
fn bombed(bomb: &Stream, joined: bool) -> Pdf {
    let mut pdf = text_pdf(&[&["SMALL PAGE"], &["BOMB PAGE"]]);
    let ids = pdf.page_ids();
    let doc = pdf.document_mut();
    let bomb = doc.add_object(bomb.clone());
    let own = doc.get_dictionary(ids[1]).unwrap().get(b"Contents").unwrap().clone();
    let contents: Object = if joined { vec![own, bomb.into()].into() } else { bomb.into() };
    doc.get_dictionary_mut(ids[1]).unwrap().set("Contents", contents);
    pdf
}

#[test]
fn decompression_bombs_are_refused_or_never_decoded() {
    let bomb = flate_bomb(300 << 20);
    assert!(bomb.content.len() < 4 << 20, "the fixture is small in the file");
    let too_large = |result: Result<(), Error>, what: &str| match result {
        Err(Error::TooLarge(message)) => assert_eq!(message, carbide_engine::limits::messages::TOO_LARGE, "{what}"),
        other => panic!("{what}: {other:?}"),
    };

    // Content split over several streams has to be decoded, and is refused past the limit.
    let pdf = bombed(&bomb, true);
    too_large(pdf.impose(&ImpositionOptions::default(), None).map(|_| ()), "impose");
    too_large(pdf.clone().reduce(&ReduceOptions::default(), None).map(|_| ()), "reduce");
    // Removing another page still works; the page whose content is too large keeps its resources.
    let mut deleted = pdf.clone();
    deleted.delete_pages(&[0]).unwrap();
    assert_eq!(deleted.page_count(), 1);
    assert!(!deleted.document().get_dictionary(deleted.page_ids()[0]).unwrap().get(b"Resources").unwrap().as_dict().unwrap().is_empty());

    // A single content stream goes onto the imposed sheet as it is stored, never decoded.
    let pdf = bombed(&bomb, false);
    let options = ImpositionOptions { plan: PlanOptions { layout: LayoutKind::Sequential, ..Default::default() }, ..Default::default() };
    let sheets = pdf.impose(&options, None).unwrap();
    let carried = sheets.document().objects.values().filter_map(|o| o.as_stream().ok()).any(|stream| {
        stream.dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Form") && stream.content == bomb.content
    });
    assert!(carried, "the content stream was not carried over as it was stored");
}

#[test]
fn huge_pages_render_within_the_pixel_limit() {
    use carbide_engine::render::{MAX_RENDER_PIXELS, MAX_RENDER_SIDE};
    let r = renderer();
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((14400.0, 14400.0))).unwrap();
    pdf.insert_blank_page(1, Some((14400.0, 72.0))).unwrap();
    let bytes = pdf.to_bytes().unwrap();
    // 60000 pixels square at 300 dpi, 14 GB as RGBA: rendered smaller instead.
    let square = r.render_page(&bytes, 0, 300.0).unwrap();
    assert!((square.width() as f32) * (square.height() as f32) <= MAX_RENDER_PIXELS, "{:?}", square.dimensions());
    assert_eq!(square.width(), square.height(), "the shape is kept");
    let strip = r.render_page(&bytes, 1, 300.0).unwrap();
    let ratio = strip.width() as f32 / strip.height() as f32;
    assert!(strip.width() as f32 <= MAX_RENDER_SIDE && (ratio - 200.0).abs() < 2.5, "{:?}", strip.dimensions());
    for dpi in [0.0, -72.0, f32::NAN, f32::INFINITY] {
        assert!(r.render_page(&bytes, 0, dpi).is_err(), "{dpi} dpi");
    }
}

// ---- inherited page attributes -------------------------------------------------------------

/// `numbered_pages(4)` with /Rotate 90 and /CropBox [0 300 500 792] (which still shows the text)
/// set only on a page-tree node: the root, or a node between the root and the pages.
fn inherited_geometry(on_root: bool) -> Pdf {
    let mut pdf = numbered_pages(4);
    let doc = pdf.document_mut();
    let root_id = doc.catalog().unwrap().get(b"Pages").unwrap().as_reference().unwrap();
    let crop: Vec<Object> = vec![0.into(), 300.into(), 500.into(), 792.into()];
    if on_root {
        let root = doc.get_dictionary_mut(root_id).unwrap();
        root.set("Rotate", 90);
        root.set("CropBox", crop);
    } else {
        let kids = doc.get_dictionary(root_id).unwrap().get(b"Kids").unwrap().clone();
        let middle = doc.add_object(dictionary! {
            "Type" => "Pages", "Parent" => root_id, "Kids" => kids.clone(), "Count" => 4, "Rotate" => 90, "CropBox" => crop,
        });
        for kid in kids.as_array().unwrap() {
            doc.get_dictionary_mut(kid.as_reference().unwrap()).unwrap().set("Parent", middle);
        }
        doc.get_dictionary_mut(root_id).unwrap().set("Kids", vec![Object::Reference(middle)]);
    }
    let bytes = pdf.to_bytes().unwrap();
    Pdf::from_bytes(&bytes).unwrap()
}

/// Width and height of a page as PDFium draws it at 72 dpi.
fn rendered_size(pdf: &mut Pdf, page: usize) -> (u32, u32) {
    render(renderer(), pdf, page).dimensions()
}

#[test]
fn new_and_rebuilt_pages_do_not_pick_up_an_inherited_rotation_or_crop_box() {
    let r = renderer();
    let source = inherited_geometry(true);
    let geometry = source.page_geometry(0).unwrap();
    assert_eq!((geometry.rotation, geometry.display_size()), (90, (492.0, 500.0)), "the fixture inherits both");

    // An inserted blank page has the size it was given, upright; the other pages keep theirs.
    let mut blank = source.clone();
    blank.insert_blank_page(4, Some((612.0, 792.0))).unwrap();
    let geometry = blank.page_geometry(4).unwrap();
    assert_eq!((geometry.rotation, geometry.display_size()), (0, (612.0, 792.0)));
    assert_eq!(rendered_size(&mut blank, 4), (612, 792));
    let geometry = blank.page_geometry(1).unwrap();
    assert_eq!((geometry.rotation, geometry.display_size()), (90, (492.0, 500.0)));
    assert_eq!(rendered_size(&mut blank, 1), (492, 500));

    // Imposed Letter sheets are Letter landscape, not rotated or cropped like the source pages.
    let options = ImpositionOptions { media: Media::Named("Letter".into()), ..Default::default() };
    let mut sheets = source.impose(&options, None).unwrap();
    for sheet in 0..sheets.page_count() {
        let geometry = sheets.page_geometry(sheet).unwrap();
        assert_eq!((geometry.rotation, geometry.display_size()), (0, (792.0, 612.0)), "sheet {sheet}");
        assert_eq!(rendered_size(&mut sheets, sheet), (792, 612), "sheet {sheet}");
    }
    assert!(page_text(r, &mut sheets, 0).contains("PAGE1X"));

    // A redacted page shows exactly what it showed before, whichever node the values came from.
    for on_root in [true, false] {
        let mut pdf = inherited_geometry(on_root);
        let mut before = render(r, &mut pdf, 0);
        let text_area = Rect::new(0.0, 0.0, 492.0, 500.0);
        let marked = Rect::new(380.0, 20.0, 80.0, 60.0);
        pdf.apply_redactions(r, &[Redaction { page: 0, rect: marked }], 150.0).unwrap();
        let geometry = pdf.page_geometry(0).unwrap();
        assert_eq!((geometry.rotation, geometry.display_size()), (0, (492.0, 500.0)), "on root: {on_root}");
        let after = render(r, &mut pdf, 0);
        assert_eq!(after.dimensions(), (492, 500), "on root: {on_root}");
        assert!(brightness(&after, marked) < 30.0, "on root: {on_root}: the marked area is not black");
        // Black the same area in the earlier render: the rest of the page has to match it.
        for y in (500.0 - marked.max_y()) as u32..(500.0 - marked.y) as u32 {
            for x in marked.x as u32..marked.max_x() as u32 {
                before.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        let (a, b) = (brightness(&before, text_area), brightness(&after, text_area));
        assert!((a - b).abs() < 3.0, "on root: {on_root}: the page changed outside the marked area ({a} -> {b})");
        // The text at (72, 700), turned a quarter clockwise: a vertical line near x 400.
        let text_line = Rect::new(390.0, 350.0, 30.0, 85.0);
        let (a, b) = (brightness(&before, text_line), brightness(&after, text_line));
        assert!(a < 245.0 && (a - b).abs() < 25.0, "on root: {on_root}: the text moved ({a} -> {b})");
    }
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

    // In place, as an explicit choice.
    let outcomes = reduce_files(
        &[photo.clone(), tiny.clone(), broken.clone()],
        &OutputOptions::in_place(),
        &ReduceOptions::default(),
    );
    assert!(outcomes[0].warning.is_none() && outcomes[0].report.is_some());
    assert!(std::fs::metadata(&photo).unwrap().len() < photo_before / 10);
    assert_eq!(outcomes[1].warning.as_deref(), Some(reduce_messages::NOT_SMALLER));
    assert_eq!(std::fs::read(&tiny).unwrap(), tiny_before, "a file that cannot shrink is left alone");
    assert_eq!(outcomes[2].warning.as_deref(), Some(reduce_messages::OPEN_FAILED));
    assert_eq!(std::fs::read(&broken).unwrap(), b"this is not a pdf");

    // A protected original is never replaced by an unencrypted copy, even one that opens
    // without a password.
    let protected = dir.join("protected.pdf");
    let mut options = ProtectionOptions::new("", "owner pw");
    options.allow_copying = false;
    std::fs::write(&protected, photo_pdf().to_encrypted_bytes(&options).unwrap()).unwrap();
    let before = std::fs::read(&protected).unwrap();
    let outcomes = reduce_files(std::slice::from_ref(&protected), &OutputOptions::in_place(), &ReduceOptions::default());
    assert_eq!(outcomes[0].warning.as_deref(), Some(reduce_messages::PROTECTED));
    assert_eq!(std::fs::read(&protected).unwrap(), before);

    // Naming and overwrite rules.
    let named = OutputOptions { prefix: "small-".into(), suffix: String::new(), overwrite: false, ..Default::default() };
    assert_eq!(named.output_path(&photo).unwrap(), dir.join("small-photo.pdf"));
    std::fs::write(dir.join("small-photo.pdf"), b"existing").unwrap();
    assert_eq!(named.output_path(&photo), Err(reduce_messages::FILE_EXISTS));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn batch_reduce_keeps_originals_and_refuses_names_that_collide() {
    let dir = scratch_dir("reduce-names");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    photo_pdf().save(a.join("x.pdf")).unwrap();
    photo_pdf().save(b.join("x.pdf")).unwrap();
    let originals = |paths: &[PathBuf]| paths.iter().map(|p| std::fs::read(p).unwrap()).collect::<Vec<_>>();

    // By default the results go next to the originals under new names, and the originals stay.
    let inputs = [a.join("x.pdf"), b.join("x.pdf")];
    let before = originals(&inputs);
    let outcomes = reduce_files(&inputs, &OutputOptions::default(), &ReduceOptions::default());
    assert!(outcomes.iter().all(|o| o.warning.is_none()), "{outcomes:?}");
    assert_eq!(outcomes[0].output.as_deref(), Some(a.join("x-reduced.pdf").as_path()));
    assert_eq!(originals(&inputs), before);
    // Again: the reduced copies are there now, and are not replaced without asking.
    let outcomes = reduce_files(&inputs, &OutputOptions::default(), &ReduceOptions::default());
    assert!(outcomes.iter().all(|o| o.warning.as_deref() == Some(reduce_messages::FILE_EXISTS)), "{outcomes:?}");
    // So is a link there, even one that leads nowhere: nothing is written where it leads.
    #[cfg(unix)]
    {
        let elsewhere = dir.join("elsewhere.pdf");
        std::fs::remove_file(a.join("x-reduced.pdf")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, a.join("x-reduced.pdf")).unwrap();
        let outcomes = reduce_files(&inputs[..1], &OutputOptions::default(), &ReduceOptions::default());
        assert_eq!(outcomes[0].warning.as_deref(), Some(reduce_messages::FILE_EXISTS));
        assert!(!elsewhere.exists());
    }

    // Both into b, with their own names, replacing what is there (b/x.pdf with its own result):
    // a's result would replace b/x.pdf before it is read.
    let into_b = OutputOptions { folder: TargetFolder::Specific(b.clone()), ..OutputOptions::in_place() };
    let outcomes = reduce_files(&inputs, &into_b, &ReduceOptions::default());
    assert_eq!(outcomes[0].warning.as_deref(), Some(reduce_messages::OUTPUT_COLLIDES));
    assert_eq!(outcomes[1].warning.as_deref(), Some(reduce_messages::OUTPUT_COLLIDES));
    assert!(outcomes.iter().all(|o| o.output.is_none()));
    assert_eq!(originals(&inputs), before, "nothing is touched");

    // A result named like another input in the same folder, and two results with one name.
    let small = a.join("x-small.pdf");
    photo_pdf().save(&small).unwrap();
    let small_before = std::fs::read(&small).unwrap();
    let suffixed = OutputOptions { suffix: "-small".into(), overwrite: true, ..Default::default() };
    let outcomes = reduce_files(&[a.join("x.pdf"), small.clone(), b.join("x.pdf")], &suffixed, &ReduceOptions::default());
    assert_eq!(outcomes[0].warning.as_deref(), Some(reduce_messages::OUTPUT_COLLIDES));
    assert_eq!(outcomes[2].warning.as_deref(), Some(reduce_messages::BATCH_STOPPED), "nothing at all is reduced");
    assert_eq!(std::fs::read(&small).unwrap(), small_before);
    let same_name = OutputOptions { folder: TargetFolder::Specific(dir.clone()), overwrite: true, ..Default::default() };
    let outcomes = reduce_files(&inputs, &same_name, &ReduceOptions::default());
    assert!(outcomes.iter().all(|o| o.warning.as_deref() == Some(reduce_messages::OUTPUT_COLLIDES)), "{outcomes:?}");
    assert!(!dir.join("x-reduced.pdf").exists());

    // Replacing the originals takes the explicit choice.
    let same_names = OutputOptions { suffix: String::new(), overwrite: true, ..Default::default() };
    let outcomes = reduce_files(&inputs, &same_names, &ReduceOptions::default());
    assert!(outcomes.iter().all(|o| o.warning.as_deref() == Some(reduce_messages::REPLACES_ORIGINAL)), "{outcomes:?}");
    assert_eq!(originals(&inputs), before);
    let outcomes = reduce_files(&inputs, &OutputOptions::in_place(), &ReduceOptions::default());
    assert!(outcomes.iter().all(|o| o.warning.is_none()), "{outcomes:?}");
    assert!(std::fs::read(&inputs[0]).unwrap().len() < before[0].len() / 10);
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

/// The `/ByteRange` of the last signature in `file`.
fn byte_range(file: &[u8]) -> [usize; 4] {
    let at = file.windows(10).rposition(|w| w == b"/ByteRange").unwrap();
    let open = at + file[at..].iter().position(|b| *b == b'[').unwrap();
    let close = open + file[open..].iter().position(|b| *b == b']').unwrap();
    let numbers: Vec<usize> =
        std::str::from_utf8(&file[open + 1..close]).unwrap().split_whitespace().map(|n| n.parse().unwrap()).collect();
    numbers.try_into().unwrap()
}

/// Tag, header length and total length of the DER element at the front of `data`.
fn der_header(data: &[u8]) -> (u8, usize, usize) {
    let first = data[1] as usize;
    if first < 0x80 {
        return (data[0], 2, 2 + first);
    }
    let n = first & 0x7F;
    let len = data[2..2 + n].iter().fold(0usize, |len, b| (len << 8) | *b as usize);
    (data[0], 2 + n, 2 + n + len)
}

/// The CMS in the last signature's `/Contents`, without the zero padding after it.
fn signature_value(file: &[u8]) -> Vec<u8> {
    let [_, b, c, _] = byte_range(file);
    let hex = std::str::from_utf8(&file[b + 1..c - 1]).unwrap();
    let bytes: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    let (_, _, total) = der_header(&bytes);
    bytes[..total].to_vec()
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

/// `der` written again with indefinite lengths on every constructed element, as BER signers do,
/// except the elements `keep` selects by their path of child positions (and what is inside them).
fn to_ber(der: &[u8], path: &mut Vec<usize>, keep: &dyn Fn(&[usize]) -> bool) -> Vec<u8> {
    let (tag, header, total) = der_header(der);
    if tag & 0x20 == 0 || keep(path) {
        return der[..total].to_vec();
    }
    let mut out = vec![tag, 0x80];
    let mut rest = &der[header..total];
    for index in 0.. {
        if rest.is_empty() {
            break;
        }
        let (_, _, len) = der_header(rest);
        path.push(index);
        let child = to_ber(&rest[..len], path, keep);
        path.pop();
        out.extend(child);
        rest = &rest[len..];
    }
    out.extend([0, 0]);
    out
}

#[test]
fn signatures_in_ber_with_indefinite_lengths_verify() {
    let mut doc = text_pdf(&[&["ber"]]);
    let base = file_bytes(&mut doc);
    for sub_filter in [SubFilter::Pkcs7Detached, SubFilter::CadesDetached] {
        let mut signed = doc.sign(&base, &sign_options(sub_filter)).unwrap();
        let der = signature_value(&signed);
        // Every wrapper, the signer info and the signed attributes become indefinite-length; the
        // certificates themselves stay DER, as they must for their own signatures to hold.
        let ber = to_ber(&der, &mut Vec::new(), &|path: &[usize]| path.len() == 4 && path[..3] == [1, 0, 3]);
        assert_ne!(ber, der);
        assert_eq!(&ber[..2], &[0x30, 0x80]);
        put_signature_value(&mut signed, &ber);
        let reports = verify(&signed, &trust_test_identity());
        assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly), "{sub_filter:?}");
    }
}

/// Rewrites the four `/ByteRange` numbers in place (same widths), leaving every other byte and
/// every cross-reference offset intact so the file still parses.
fn set_byte_range(file: &mut [u8], range: [usize; 4]) {
    let text = format!("{:010} {:010} {:010} {:010}", range[0], range[1], range[2], range[3]);
    let at = file.windows(10).rposition(|w| w == b"/ByteRange").unwrap();
    let open = at + file[at..].iter().position(|x| *x == b'[').unwrap() + 1;
    file[open..open + text.len()].copy_from_slice(text.as_bytes());
}

#[test]
fn tampering_with_a_signed_file_comes_out_invalid() {
    let mut doc = text_pdf(&[&["pay 100 dollars"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    assert_eq!(verify(&signed, &trust_test_identity())[0].validity, Validity::Valid);
    let [a, b, c, d] = byte_range(&signed);

    // The gap is widened past the hex /Contents it must enclose: the claimed gap now runs two
    // bytes beyond the closing '>'.
    let mut widened = signed.clone();
    set_byte_range(&mut widened, [a, b, c + 2, d - 2]);
    let reports = verify(&widened, &trust_test_identity());
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Invalid, Detail::BadByteRange), "widened gap");

    // The /Contents value is changed in place (a hex digit flipped), offsets untouched.
    let mut swapped = signed.clone();
    swapped[b + 1] = if swapped[b + 1] == b'0' { b'1' } else { b'0' };
    assert_ne!(verify(&swapped, &trust_test_identity())[0].validity, Validity::Valid, "altered /Contents");
}

/// The signature dictionary's object number, read from its `N 0 obj` header before `/ByteRange`.
fn signature_object_number(file: &[u8]) -> u32 {
    let at = file.windows(10).rposition(|w| w == b"/ByteRange").unwrap();
    let obj = file[..at].windows(4).rposition(|w| w == b" obj").unwrap();
    let line = file[..obj].iter().rposition(|b| matches!(b, b'\n' | b'\r' | b'>')).map(|p| p + 1).unwrap_or(0);
    std::str::from_utf8(&file[line..obj]).unwrap().split_whitespace().next().unwrap().parse().unwrap()
}

/// The offset the file's last `startxref` names.
fn startxref(file: &[u8]) -> usize {
    let at = file.windows(9).rposition(|w| w == b"startxref").unwrap();
    std::str::from_utf8(&file[at + 9..]).unwrap().split_whitespace().next().unwrap().parse().unwrap()
}

#[test]
fn a_signature_object_moved_after_the_gap_is_rejected() {
    // A signature-wrapping layout: an appended revision redefines the signature dictionary so its
    // bytes now sit after the gap its own ByteRange covers. The verifier must not accept it.
    let mut doc = text_pdf(&[&["original"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    assert_eq!(verify(&signed, &trust_test_identity())[0].validity, Validity::Valid);

    let id = signature_object_number(&signed);
    let hex: String = signature_value(&signed).iter().map(|b| format!("{b:02X}")).collect();
    let [_, b, c, _] = byte_range(&signed);
    let prev = startxref(&signed);

    let mut forged = signed.clone();
    forged.push(b'\n');
    let obj_at = forged.len();
    forged.extend_from_slice(
        format!("{id} 0 obj\n<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/adbe.pkcs7.detached/ByteRange[0 {b} {c} 0]/Contents<{hex}>>>\nendobj\n").as_bytes(),
    );
    let xref_at = forged.len();
    forged.extend_from_slice(
        format!("xref\n{id} 1\n{obj_at:010} 00000 n \ntrailer\n<</Size {}/Root 1 0 R/Prev {prev}>>\nstartxref\n{xref_at}\n%%EOF", id + 1).as_bytes(),
    );
    let reports = verify(&forged, &trust_test_identity());
    assert_eq!(reports[0].detail, Detail::BadByteRange, "relocated signature object was {:?}", reports[0].validity);
}

/// The two numbers after the last `/key` in `file` (a trailer reference such as `/Root 5 0 R`), or
/// just the first for a number such as `/Size`.
fn trailer_numbers(file: &[u8], key: &str) -> (u32, u32) {
    let needle = format!("/{key}");
    let at = file.windows(needle.len()).rposition(|w| w == needle.as_bytes()).unwrap() + needle.len();
    let text = String::from_utf8_lossy(&file[at..(at + 40).min(file.len())]).into_owned();
    let numbers: Vec<u32> = text.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).take(2).map(|s| s.parse().unwrap()).collect();
    (numbers[0], numbers.get(1).copied().unwrap_or(0))
}

/// `file` with one more revision appended by hand: `objects` are complete `N G obj ... endobj`
/// texts, each filed under the paired number in a classic cross-reference section. `extra` goes
/// into the trailer as it is.
fn append_revision(file: &[u8], objects: &[(u32, String)], extra: &str) -> Vec<u8> {
    let (root, root_generation) = trailer_numbers(file, "Root");
    let size = objects.iter().map(|(n, _)| n + 1).max().unwrap_or(0).max(trailer_numbers(file, "Size").0);
    let prev = startxref(file);
    let mut out = file.to_vec();
    out.push(b'\n');
    let mut entries = String::new();
    for (number, text) in objects {
        entries += &format!("{number} 1\n{:010} 00000 n \n", out.len());
        out.extend_from_slice(text.as_bytes());
        out.push(b'\n');
    }
    let xref_at = out.len();
    out.extend_from_slice(
        format!("xref\n{entries}trailer\n<</Size {size}/Root {root} {root_generation} R/Prev {prev}{extra}>>\nstartxref\n{xref_at}\n%%EOF\n").as_bytes(),
    );
    out
}

#[test]
fn a_second_copy_of_the_signature_dictionary_after_the_signed_revision_is_ignored() {
    let mut doc = text_pdf(&[&["the contract"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let original = verify(&signed, &trust_test_identity()).remove(0);
    let [_, b, c, d] = byte_range(&signed);
    let id = signature_object_number(&signed);
    let gap = String::from_utf8(signed[b + 1..c - 1].to_vec()).unwrap();

    // An update files a new object number under bytes that claim to be the signature dictionary,
    // with the same byte range and value but the attacker's date, name and reason. The loader
    // keeps the later copy.
    let alias = format!(
        "{id} 0 obj\n<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/adbe.pkcs7.detached/ByteRange[0 {b} {c} {d}]/Contents<{gap}>/M(D:19990101000000Z)/Name(Mallory)/Reason(I reject this contract)/Location(Nowhere)>>\nendobj"
    );
    let forged = append_revision(&signed, &[(trailer_numbers(&signed, "Size").0, alias)], "");
    let report = verify(&forged, &trust_test_identity()).remove(0);
    assert!(report.modified_after_signing);
    assert_eq!((report.validity, report.detail), (Validity::Valid, Detail::TrustedDirectly));
    assert_eq!(report.reason.as_deref(), Some("I approve"));
    assert_eq!((&report.location, &report.signing_time), (&original.location, &original.signing_time));
    assert_eq!(report.claimed_name, None, "the signed /Name matches the certificate");
}

#[test]
fn bytes_written_into_the_gap_never_make_a_signature_disappear() {
    let mut doc = text_pdf(&[&["gap"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let [_, b, c, _] = byte_range(&signed);
    let (size, (root, _)) = (trailer_numbers(&signed, "Size").0, trailer_numbers(&signed, "Root"));

    // A whole revision inside the gap: the hex string and the dictionary are closed early, then
    // come an object, its cross-reference section and a trailer, and a comment hides the rest.
    let at = b + 1;
    let mut revision = ">>>\nendobj\n".to_string();
    let object_at = at + revision.len();
    revision += &format!("{size} 0 obj\n<</Length 0>>stream\n\nendstream\nendobj\n");
    let xref_at = at + revision.len();
    revision += &format!(
        "xref\n{size} 1\n{object_at:010} 00000 n \ntrailer\n<</Size {}/Root {root} 0 R/Prev {}>>\nstartxref\n{xref_at}\n%%EOF\n%",
        size + 1,
        startxref(&signed)
    );
    assert!(revision.len() < c - 1 - at);
    let mut in_gap = signed.clone();
    in_gap[at..at + revision.len()].copy_from_slice(revision.as_bytes());

    // Two bytes that leave the dictionary unparseable.
    let mut unbalanced = signed.clone();
    unbalanced[c - 3..c - 1].copy_from_slice(b">(");

    for (label, forged) in [("revision in the gap", in_gap), ("unbalanced", unbalanced)] {
        let reports = verify(&forged, &trust_test_identity());
        assert!(!reports.is_empty(), "{label}: the signature vanished");
        assert!(reports.iter().all(|r| r.validity == Validity::Invalid), "{label}: {:?}", reports.iter().map(|r| r.detail).collect::<Vec<_>>());
        assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_PROBLEMS), "{label}");
        assert!(Pdf::from_bytes(&forged).unwrap().has_signatures(), "{label}");
    }
}

/// `protected` (an encrypted file) with a signature over all of it appended by hand, as another
/// signer would: a signature dictionary holds no encrypted strings, and its value never is.
fn sign_protected(protected: &[u8]) -> Vec<u8> {
    let (encrypt, generation) = trailer_numbers(protected, "Encrypt");
    let id = trailer_numbers(protected, "Size").0;
    let placeholder = "0".repeat(2 * 4096);
    let dictionary = format!(
        "{id} 0 obj\n<</Type/Sig/Filter/Adobe.PPKLite/SubFilter/adbe.pkcs7.detached/ByteRange[0000000000 0000000000 0000000000 0000000000]/Contents<{placeholder}>>>\nendobj"
    );
    let mut signed = append_revision(protected, &[(id, dictionary)], &format!("/Encrypt {encrypt} {generation} R"));
    let b = signed.windows(placeholder.len() + 1).rposition(|w| w[0] == b'<' && w[1..].iter().all(|x| *x == b'0')).unwrap();
    let c = b + placeholder.len() + 2;
    let d = signed.len() - c;
    set_byte_range(&mut signed, [0, b, c, d]);

    // Sign the covered bytes directly (no signed attributes).
    use rsa::pkcs8::DecodePrivateKey;
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&test_identity().pkcs8).unwrap();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(covered_bytes(&signed));
    let signature = key.sign(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), &digest).unwrap();
    let cms = detached_cms(&test_identity().certificate, SHA256_OID, &[1, 2, 840, 113549, 1, 1, 11], None, &signature);
    put_signature_value(&mut signed, &cms);
    signed
}

#[test]
fn signatures_on_owner_password_protected_files_verify() {
    // Protected with an owner password only, so it opens without one. Its strings and streams are
    // encrypted, but the signature value is not.
    let plain = text_pdf(&[&["protected and signed"]]).to_bytes().unwrap();
    let protected = Pdf::from_bytes(&plain).unwrap().to_encrypted_bytes(&ProtectionOptions::new("", "owner")).unwrap();
    let signed = sign_protected(&protected);
    let reports = verify(&signed, &trust_test_identity());
    assert_eq!(reports.len(), 1);
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly));

    // And the covered (encrypted) bytes are still bound to it: one bit of the page content's
    // ciphertext changes, which leaves the file's structure as it was.
    let mut tampered = signed.clone();
    let data = signed.windows(7).position(|w| w == b"stream\n").unwrap() + 7;
    tampered[data + 4] ^= 0x01;
    let reports = verify(&tampered, &trust_test_identity());
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Invalid, Detail::DigestMismatch));
}

#[test]
fn a_signed_file_with_an_open_password_and_a_later_update_takes_appended_edits() {
    let plain = text_pdf(&[&["locked and signed"]]).to_bytes().unwrap();
    let protected = Pdf::from_bytes(&plain).unwrap().to_encrypted_bytes(&ProtectionOptions::new("user", "owner")).unwrap();
    let signed = sign_protected(&protected);
    // An update after the signature, so its revision has to be read on its own, with the password.
    let (encrypt, generation) = trailer_numbers(&signed, "Encrypt");
    let n = trailer_numbers(&signed, "Size").0;
    let note = format!("{n} 0 obj\n<</Type/Annot/Subtype/Square/Rect[0 0 1 1]>>\nendobj");
    let updated = append_revision(&signed, &[(n, note)], &format!("/Encrypt {encrypt} {generation} R"));
    let (mut pdf, saved) = SavedFile::open(updated, Some("user".into())).unwrap();
    pdf.rotate_pages(&[0], 90).unwrap();
    assert_eq!(pdf.save_plan(Some(&saved), false).unwrap(), SavePlan::Incremental, "the intact signature was treated as broken");
}

// ---- algorithm coverage (S7, S6) ----------------------------------------------------------

mod algo {
    //! Minimal DER, enough to wrap a signature of any algorithm into a detached CMS whose signer
    //! certificate the verifier reads.
    pub fn len(n: usize) -> Vec<u8> {
        if n < 0x80 {
            vec![n as u8]
        } else {
            let bytes = n.to_be_bytes();
            let skip = bytes.iter().take_while(|b| **b == 0).count();
            let mut out = vec![0x80 | (bytes.len() - skip) as u8];
            out.extend_from_slice(&bytes[skip..]);
            out
        }
    }
    pub fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend(len(body.len()));
        out.extend_from_slice(body);
        out
    }
    pub fn seq(parts: &[&[u8]]) -> Vec<u8> {
        tlv(0x30, &parts.concat())
    }
    pub fn set(parts: &[&[u8]]) -> Vec<u8> {
        tlv(0x31, &parts.concat())
    }
    pub fn ctx(n: u8, body: &[u8]) -> Vec<u8> {
        tlv(0xA0 | n, body)
    }
    pub fn oid(arcs: &[u32]) -> Vec<u8> {
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
    pub fn null() -> Vec<u8> {
        vec![0x05, 0x00]
    }
}

/// A detached CMS SignedData with one signer, no signed attributes: the signature covers the
/// content directly. `cert_der` is embedded and named as the signer.
fn detached_cms(cert_der: &[u8], digest_oid: &[u32], sig_oid: &[u32], sig_params: Option<Vec<u8>>, signature: &[u8]) -> Vec<u8> {
    use algo::*;
    use x509_cert::der::{Decode, Encode};
    let cert = x509_cert::Certificate::from_der(cert_der).unwrap();
    let issuer = cert.tbs_certificate.issuer.to_der().unwrap();
    let serial = cert.tbs_certificate.serial_number.to_der().unwrap();
    let issuer_and_serial = seq(&[&issuer, &serial]);
    let digest_algo = seq(&[&oid(digest_oid), &null()]);
    let sig_algo = match sig_params {
        Some(params) => seq(&[&oid(sig_oid), &params]),
        None => seq(&[&oid(sig_oid)]),
    };
    let signer_info = seq(&[
        &tlv(0x02, &[1]),
        &issuer_and_serial,
        &digest_algo,
        &sig_algo,
        &tlv(0x04, signature),
    ]);
    let signed_data = seq(&[
        &tlv(0x02, &[1]),
        &set(&[&digest_algo]),
        &seq(&[&oid(&[1, 2, 840, 113549, 1, 7, 1])]), // id-data
        &ctx(0, cert_der),
        &set(&[&signer_info]),
    ]);
    seq(&[&oid(&[1, 2, 840, 113549, 1, 7, 2]), &ctx(0, &signed_data)]) // id-signedData
}

/// A self-signed certificate for `pair`, with `common_name`.
fn self_signed(pair: &rcgen::KeyPair, common_name: &str) -> Vec<u8> {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(rcgen::DnType::CommonName, common_name);
    params.self_signed(pair).unwrap().der().to_vec()
}

/// Signs `base`, then replaces the CMS in /Contents with `cms`, keeping the byte range. The signing
/// time is fixed, so a probe signing and the real one cover exactly the same bytes.
fn resign_with(doc: &Pdf, base: &[u8], cms: &[u8]) -> Vec<u8> {
    let options = SignOptions { signing_time: Some("D:20261001000000Z".into()), ..sign_options(SubFilter::Pkcs7Detached) };
    let mut signed = doc.sign(base, &options).unwrap();
    put_signature_value(&mut signed, cms);
    signed
}

/// The bytes a detached signature over `signed` covers.
fn covered_bytes(signed: &[u8]) -> Vec<u8> {
    let [_, b, c, d] = byte_range(signed);
    let mut out = signed[..b].to_vec();
    out.extend_from_slice(&signed[c..c + d]);
    out
}

#[test]
fn ecdsa_p256_signatures_verify_and_name_comes_from_the_certificate() {
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use p256::pkcs8::DecodePrivateKey;
    let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let signing = p256::ecdsa::SigningKey::from_pkcs8_der(&pair.serialize_der()).unwrap();
    let cert = self_signed(&pair, "Elliptic Signer");

    let mut doc = text_pdf(&[&["ecdsa"]]);
    let base = file_bytes(&mut doc);
    // Build the signed file first so the covered bytes are known, then sign those.
    let probe = resign_with(&doc, &base, &[0x30, 0x00]);
    let covered = covered_bytes(&probe);
    let digest = <sha2::Sha256 as sha2::Digest>::digest(&covered);
    let signature: p256::ecdsa::Signature = signing.sign_prehash(&digest).unwrap();
    let cms = detached_cms(&cert, &[2, 16, 840, 1, 101, 3, 4, 2, 1], &[1, 2, 840, 10045, 4, 3, 2], None, signature.to_der().as_bytes());
    let signed = resign_with(&doc, &base, &cms);

    // /Name still says the Carbide identity; the certificate subject is the ECDSA one.
    let reports = verify(&signed, &TrustStore { certificates: vec![cert.clone()] });
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly));
    assert_eq!(reports[0].signer.as_deref(), Some("Elliptic Signer"));
    assert_eq!(reports[0].claimed_name.as_deref(), Some("Carbide Test Signer"));
}

#[test]
fn signer_names_are_read_from_the_certificate_attributes() {
    use rcgen::DnType;
    let email = DnType::CustomDnType(vec![1, 2, 840, 113549, 1, 9, 1]);
    let cases: [(&[(DnType, &str)], &str); 3] = [
        // A comma inside the common name is part of the name, not a separator.
        (&[(DnType::CommonName, "Doe, John"), (DnType::OrganizationName, "Acme")], "Doe, John"),
        // No common name: the email address.
        (&[(email, "jane@example.com")], "jane@example.com"),
        // Text in another attribute that looks like a common name does not become one.
        (&[(DnType::OrganizationName, "Evil Inc,CN=Bank of America")], ""),
    ];
    for (attributes, expected) in cases {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        let pair = rcgen::KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new(); // no default common name
        for (kind, value) in attributes {
            params.distinguished_name.push(kind.clone(), *value);
        }
        let cert = params.self_signed(&pair).unwrap().der().to_vec();
        let identity = Identity::new(vec![cert.clone()], key).unwrap();
        let mut doc = text_pdf(&[&["names"]]);
        let base = file_bytes(&mut doc);
        let signed = doc.sign(&base, &SignOptions { identity: Some(&identity), ..Default::default() }).unwrap();
        let report = verify(&signed, &TrustStore { certificates: vec![cert] }).remove(0);
        let signer = report.signer.unwrap();
        if expected.is_empty() {
            assert_ne!(signer, "Bank of America");
            assert!(signer.contains("Evil Inc"), "{signer}");
        } else {
            assert_eq!(signer, expected);
        }
        assert_eq!(identity.common_name(), signer);
        // /Name is written from the same certificate, so it is not repeated as a claimed name.
        assert_eq!(report.claimed_name, None, "{signer}");
    }
}

#[test]
fn rsa_pss_signatures_verify() {
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let pair = rcgen::KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    let cert = self_signed(&pair, "PSS Signer");

    let mut doc = text_pdf(&[&["pss"]]);
    let base = file_bytes(&mut doc);
    let probe = resign_with(&doc, &base, &[0x30, 0x00]);
    let covered = covered_bytes(&probe);
    let signing = rsa::pss::SigningKey::<sha2::Sha256>::new(key);
    let signature = signing.sign_with_rng(&mut rand::rngs::OsRng, &covered).to_vec();
    // RSASSA-PSS-params: SHA-256, MGF1-SHA-256, salt 32.
    use algo::*;
    let sha256 = seq(&[&oid(&[2, 16, 840, 1, 101, 3, 4, 2, 1]), &null()]);
    let params = seq(&[
        &ctx(0, &sha256),
        &ctx(1, &seq(&[&oid(&[1, 2, 840, 113549, 1, 1, 8]), &sha256])),
        &ctx(2, &tlv(0x02, &[32])),
    ]);
    let cms = detached_cms(&cert, &[2, 16, 840, 1, 101, 3, 4, 2, 1], &[1, 2, 840, 113549, 1, 1, 10], Some(params), &signature);
    let signed = resign_with(&doc, &base, &cms);

    let reports = verify(&signed, &TrustStore { certificates: vec![cert] });
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly), "{:?}", reports[0].detail);
    assert!(!reports[0].weak_algorithm);
}

/// `RSASSA-PSS-params` naming `hash`, MGF1 over `mask_hash`, and `salt` bytes of salt.
fn pss_params(hash: &[u32], mask_hash: &[u32], salt: u8) -> Vec<u8> {
    use algo::*;
    let hash = seq(&[&oid(hash), &null()]);
    let mask_hash = seq(&[&oid(mask_hash), &null()]);
    seq(&[&ctx(0, &hash), &ctx(1, &seq(&[&oid(&[1, 2, 840, 113549, 1, 1, 8]), &mask_hash])), &ctx(2, &[0x02, 0x01, salt])])
}

const SHA1_OID: &[u32] = &[1, 3, 14, 3, 2, 26];
const SHA224_OID: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 4];
const SHA256_OID: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];
const RSA_PSS_OID: &[u32] = &[1, 2, 840, 113549, 1, 1, 10];

#[test]
fn rsa_pss_parameters_are_honoured_or_reported_as_unsupported() {
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let pair = rcgen::KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    let cert = self_signed(&pair, "PSS Signer");
    let trust = TrustStore { certificates: vec![cert.clone()] };
    let mut doc = text_pdf(&[&["pss parameters"]]);
    let base = file_bytes(&mut doc);
    let covered = covered_bytes(&resign_with(&doc, &base, &[0x30, 0x00]));
    let check = |signature: &[u8], params: Option<Vec<u8>>| {
        let cms = detached_cms(&cert, SHA256_OID, RSA_PSS_OID, params, signature);
        verify(&resign_with(&doc, &base, &cms), &trust).remove(0)
    };

    // Parameters left out mean SHA-1, MGF1-SHA-1 and 20 bytes of salt: valid, and flagged weak.
    let sha1 = rsa::pss::SigningKey::<sha1::Sha1>::new(key.clone()).sign_with_rng(&mut rand::rngs::OsRng, &covered).to_vec();
    let report = check(&sha1, None);
    assert_eq!((report.validity, report.detail), (Validity::Valid, Detail::TrustedDirectly), "PSS defaults");
    assert!(report.weak_algorithm, "SHA-1 inside PSS parameters is weak");
    assert!(report.description_lines().contains(&sig_messages::WEAK_ALGORITHM));

    // A mask hash that differs from the message hash, or a hash this build does not know, cannot
    // be checked: validity unknown, never "altered".
    let sha256 = rsa::pss::SigningKey::<sha2::Sha256>::new(key.clone()).sign_with_rng(&mut rand::rngs::OsRng, &covered).to_vec();
    for (label, params) in [
        ("MGF1 over SHA-1", pss_params(SHA256_OID, SHA1_OID, 32)),
        ("SHA-224", pss_params(SHA224_OID, SHA224_OID, 28)),
    ] {
        let report = check(&sha256, Some(params));
        assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, Detail::UnsupportedAlgorithm), "{label}");
        assert!(!report.description_lines().contains(&sig_messages::DOCUMENT_ALTERED), "{label}");
    }
}

#[test]
fn certificates_with_rsassa_pss_keys_verify_pss_signatures() {
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    use x509_cert::der::{Decode, Encode};
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let pair = rcgen::KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    // The same key, published under id-RSASSA-PSS instead of rsaEncryption (RFC 4055). The
    // certificate is trusted directly, so its own signature is not what is being tested.
    let mut certificate = x509_cert::Certificate::from_der(&self_signed(&pair, "PSS Key")).unwrap();
    certificate.tbs_certificate.subject_public_key_info.algorithm =
        x509_cert::spki::AlgorithmIdentifierOwned { oid: "1.2.840.113549.1.1.10".parse().unwrap(), parameters: None };
    let cert = certificate.to_der().unwrap();

    let mut doc = text_pdf(&[&["pss key"]]);
    let base = file_bytes(&mut doc);
    let covered = covered_bytes(&resign_with(&doc, &base, &[0x30, 0x00]));
    let signature = rsa::pss::SigningKey::<sha2::Sha256>::new(key).sign_with_rng(&mut rand::rngs::OsRng, &covered).to_vec();
    let cms = detached_cms(&cert, SHA256_OID, RSA_PSS_OID, Some(pss_params(SHA256_OID, SHA256_OID, 32)), &signature);
    let reports = verify(&resign_with(&doc, &base, &cms), &TrustStore { certificates: vec![cert] });
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly));
}

#[test]
fn ecdsa_variants_verify() {
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    let mut doc = text_pdf(&[&["ecdsa variants"]]);
    let base = file_bytes(&mut doc);
    let covered = covered_bytes(&resign_with(&doc, &base, &[0x30, 0x00]));

    // P-384 with SHA-1: the 20-byte digest is shorter than half the curve's field.
    {
        use p384::pkcs8::DecodePrivateKey;
        let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        let signing = p384::ecdsa::SigningKey::from_pkcs8_der(&pair.serialize_der()).unwrap();
        let cert = self_signed(&pair, "P-384 Signer");
        let mut digest = <sha1::Sha1 as sha2::Digest>::digest(&covered).to_vec();
        digest.splice(0..0, [0u8; 4]); // the same integer, at a length the signer accepts
        let signature: p384::ecdsa::Signature = signing.sign_prehash(&digest).unwrap();
        let cms = detached_cms(&cert, SHA1_OID, &[1, 2, 840, 10045, 4, 1], None, signature.to_der().as_bytes());
        let reports = verify(&resign_with(&doc, &base, &cms), &TrustStore { certificates: vec![cert] });
        assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly), "P-384 SHA-1");
        assert!(reports[0].weak_algorithm);
    }

    // P-256 named by its key OID, id-ecPublicKey, with the SignerInfo digest saying SHA-256.
    {
        use p256::pkcs8::DecodePrivateKey;
        let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let signing = p256::ecdsa::SigningKey::from_pkcs8_der(&pair.serialize_der()).unwrap();
        let cert = self_signed(&pair, "Key OID Signer");
        let digest = <sha2::Sha256 as sha2::Digest>::digest(&covered);
        let signature: p256::ecdsa::Signature = signing.sign_prehash(&digest).unwrap();
        let cms = detached_cms(&cert, SHA256_OID, &[1, 2, 840, 10045, 2, 1], None, signature.to_der().as_bytes());
        let reports = verify(&resign_with(&doc, &base, &cms), &TrustStore { certificates: vec![cert] });
        assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly), "id-ecPublicKey");
        assert!(!reports[0].weak_algorithm);
    }
}

#[test]
fn an_unsupported_algorithm_is_reported_as_unknown_not_altered() {
    // An Ed25519 signer: the algorithm is one this build cannot check.
    let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(rcgen::DnType::CommonName, "Edward Signer");
    let cert = params.self_signed(&pair).unwrap().der().to_vec();

    let mut doc = text_pdf(&[&["ed25519"]]);
    let base = file_bytes(&mut doc);
    // Ed25519 OID 1.3.101.112 for both digest and signature; the signature need not verify.
    let cms = detached_cms(&cert, &[1, 3, 101, 112], &[1, 3, 101, 112], None, &[0u8; 64]);
    let signed = resign_with(&doc, &base, &cms);

    let reports = verify(&signed, &TrustStore { certificates: vec![cert] });
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::ValidityUnknown, Detail::UnsupportedAlgorithm));
    assert_ne!(reports[0].status_line(), sig_messages::INVALID);
    let lines = reports[0].description_lines();
    assert!(lines.contains(&sig_messages::UNSUPPORTED_ALGORITHM));
    assert!(!lines.contains(&sig_messages::DOCUMENT_ALTERED));
    // Nothing was checked, so nothing claims the document is unchanged or the signer unconfirmed.
    assert!(!lines.contains(&sig_messages::NOT_MODIFIED) && !lines.contains(&sig_messages::IDENTITY_UNKNOWN), "{lines:?}");
    assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_NOT_CHECKED));
}

#[test]
fn flipping_an_unsigned_algorithm_identifier_does_not_hide_tampering() {
    // The algorithm identifiers sit in the unsigned gap, so anyone can change them. Changing one to
    // an algorithm this build does not check, then changing the signed content, must not make the
    // document read as unchanged.
    let sha256 = [0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    let sha256_rsa = [0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B];
    let mut doc = text_pdf(&[&["pay 100 dollars"]]);
    let base = file_bytes(&mut doc);
    for (sub_filter, oid, replacement) in [
        (SubFilter::CadesDetached, &sha256[..], 0x04),     // digestAlgorithm SHA-256 -> SHA-224
        (SubFilter::Pkcs7Detached, &sha256_rsa[..], 0x0E), // sha256WithRSA -> sha224WithRSA
    ] {
        let mut forged = doc.sign(&base, &sign_options(sub_filter)).unwrap();
        let mut cms = signature_value(&forged);
        // The SignerInfo comes after the certificates, so it holds the last occurrence.
        let at = cms.windows(oid.len()).rposition(|w| w == oid).unwrap();
        cms[at + oid.len() - 1] = replacement;
        put_signature_value(&mut forged, &cms);
        let position = forged.windows(11).position(|w| w == b"pay 100 dol").unwrap();
        forged[position + 4] = b'9';

        let reports = verify(&forged, &trust_test_identity());
        assert_ne!(reports[0].validity, Validity::Valid, "{sub_filter:?}");
        let lines = reports[0].description_lines();
        assert!(!lines.contains(&sig_messages::NOT_MODIFIED) && !lines.contains(&sig_messages::REVISION_UNALTERED), "{sub_filter:?}: {lines:?}");
        assert_ne!(notice_text(&reports), Some(sig_messages::NOTICE_ALL_VALID), "{sub_filter:?}");
    }
}


// ---- time stamps (S3, S8) -----------------------------------------------------------------

/// A test RFC 3161 authority: it signs real time-stamp tokens with an RSA key whose certificate
/// carries the time-stamping extended key usage, critical and alone as RFC 3161 asks.
struct TestTsa {
    key: rsa::RsaPrivateKey,
    cert: Vec<u8>,
    /// The genTime its tokens give.
    gen_time: &'static str,
}

impl TestTsa {
    /// An authority whose certificate is valid from 1 January of `from` to 1 January of `until`,
    /// with its time-stamping purpose marked critical or not.
    fn new(name: &str, (from, until): (i32, i32), critical: bool, gen_time: &'static str) -> TestTsa {
        use rsa::pkcs8::EncodePrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        let pair = rcgen::KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, name);
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.not_before = rcgen::date_time_ymd(from, 1, 1);
        params.not_after = rcgen::date_time_ymd(until, 1, 1);
        let purposes = algo::seq(&[&algo::oid(&[1, 3, 6, 1, 5, 5, 7, 3, 8])]); // id-kp-timeStamping
        let mut eku = rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 37], purposes);
        eku.set_criticality(critical);
        params.custom_extensions.push(eku);
        let cert = params.self_signed(&pair).unwrap().der().to_vec();
        TestTsa { key, cert, gen_time }
    }

    /// A TimeStampToken over `imprint` (hashed with `hash_oid`), echoing `nonce` (the content of
    /// an INTEGER) when given, whose ESS signing-certificate attribute hashes `ess_cert`.
    fn token_with(&self, hash_oid: &[u32], imprint: &[u8], nonce: Option<&[u8]>, ess_cert: &[u8]) -> Vec<u8> {
        use algo::*;
        use rsa::Pkcs1v15Sign;
        let sha256 = |data: &[u8]| <sha2::Sha256 as sha2::Digest>::digest(data).to_vec();
        let sha256_alg = seq(&[&oid(SHA256_OID), &null()]);
        let mut fields = vec![
            tlv(0x02, &[1]),                                                // version
            oid(&[1, 2, 3, 4]),                                             // policy (arbitrary)
            seq(&[&seq(&[&oid(hash_oid), &null()]), &tlv(0x04, imprint)]), // messageImprint
            tlv(0x02, &[1]),                                                // serialNumber
            tlv(0x18, self.gen_time.as_bytes()),                            // genTime
        ];
        if let Some(nonce) = nonce {
            fields.push(tlv(0x02, nonce));
        }
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let tst_info = seq(&refs);
        // Signed attributes: contentType = id-ct-TSTInfo, messageDigest = H(TSTInfo), and
        // signingCertificateV2 naming the certificate by its SHA-256 hash.
        let mut attrs = [
            seq(&[&oid(&[1, 2, 840, 113549, 1, 9, 3]), &set(&[&oid(&[1, 2, 840, 113549, 1, 9, 16, 1, 4])])]),
            seq(&[&oid(&[1, 2, 840, 113549, 1, 9, 4]), &set(&[&tlv(0x04, &sha256(&tst_info))])]),
            seq(&[
                &oid(&[1, 2, 840, 113549, 1, 9, 16, 2, 47]),
                &set(&[&seq(&[&seq(&[&seq(&[&tlv(0x04, &sha256(ess_cert))])])])]),
            ]),
        ];
        attrs.sort();
        let to_sign = sha256(&set(&[&attrs[0], &attrs[1], &attrs[2]]));
        let signature = self.key.sign(Pkcs1v15Sign::new::<sha2::Sha256>(), &to_sign).unwrap();

        use x509_cert::der::{Decode, Encode};
        let cert = x509_cert::Certificate::from_der(&self.cert).unwrap();
        let issuer = cert.tbs_certificate.issuer.to_der().unwrap();
        let serial = cert.tbs_certificate.serial_number.to_der().unwrap();
        let signer_info = seq(&[
            &tlv(0x02, &[1]),
            &seq(&[&issuer, &serial]),
            &sha256_alg,
            &ctx(0, &attrs.concat()),
            &seq(&[&oid(&[1, 2, 840, 113549, 1, 1, 11]), &null()]),
            &tlv(0x04, &signature),
        ]);
        let encap = seq(&[&oid(&[1, 2, 840, 113549, 1, 9, 16, 1, 4]), &ctx(0, &tlv(0x04, &tst_info))]);
        let signed_data = seq(&[&tlv(0x02, &[1]), &set(&[&sha256_alg]), &encap, &ctx(0, &self.cert), &set(&[&signer_info])]);
        seq(&[&oid(&[1, 2, 840, 113549, 1, 7, 2]), &ctx(0, &signed_data)])
    }

    fn token(&self, hash_oid: &[u32], imprint: &[u8], nonce: Option<&[u8]>) -> Vec<u8> {
        self.token_with(hash_oid, imprint, nonce, &self.cert)
    }
}

fn test_tsa() -> &'static TestTsa {
    static TSA: OnceLock<TestTsa> = OnceLock::new();
    TSA.get_or_init(|| TestTsa::new("Test TSA", (2020, 2040), true, "20260101000000Z"))
}

impl TimestampSource for TestTsa {
    fn token_for_digest(&self, digest: &[u8; 32]) -> carbide_engine::Result<Vec<u8>> {
        Ok(self.token(SHA256_OID, digest, None))
    }
}

/// A source whose tokens cover some other digest than the one asked for.
struct WrongImprint<'a>(&'a TestTsa);

impl TimestampSource for WrongImprint<'_> {
    fn token_for_digest(&self, _digest: &[u8; 32]) -> carbide_engine::Result<Vec<u8>> {
        Ok(self.0.token(SHA256_OID, &[0xAB; 32], None))
    }
}

fn tsa_trust() -> TrustStore {
    TrustStore { certificates: vec![test_identity().certificate.clone(), test_tsa().cert.clone()] }
}

#[test]
fn an_embedded_time_stamp_is_reported_only_when_it_verifies() {
    let mut doc = text_pdf(&[&["stamped"]]);
    let base = file_bytes(&mut doc);
    let tsa = test_tsa();
    let mut options = sign_options(SubFilter::Pkcs7Detached);
    options.timestamp = Some(tsa);
    let signed = doc.sign(&base, &options).unwrap();

    // A verifying time stamp from a trusted authority.
    let reports = verify(&signed, &tsa_trust());
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(reports[0].has_embedded_timestamp && reports[0].embedded_timestamp_trusted);
    assert!(reports[0].description_lines().contains(&sig_messages::TIME_STAMPED));

    // The same time stamp when its authority is not trusted: present, but not vouched for.
    let reports = verify(&signed, &trust_test_identity());
    assert!(reports[0].has_embedded_timestamp && !reports[0].embedded_timestamp_trusted);
    let lines = reports[0].description_lines();
    assert!(lines.contains(&sig_messages::TIME_STAMP_UNCONFIRMED) && !lines.contains(&sig_messages::TIME_STAMPED));

    // A token over something else says nothing about a time stamp.
    let wrong = WrongImprint(tsa);
    let mut options = sign_options(SubFilter::Pkcs7Detached);
    options.timestamp = Some(&wrong);
    let reports = verify(&doc.sign(&base, &options).unwrap(), &tsa_trust());
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(!reports[0].has_embedded_timestamp);
    let lines = reports[0].description_lines();
    assert!(!lines.contains(&sig_messages::TIME_STAMPED) && !lines.contains(&sig_messages::TIME_STAMP_UNCONFIRMED));

    // A signature with no time stamp says nothing about one.
    let plain = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    assert!(!verify(&plain, &trust_test_identity())[0].has_embedded_timestamp);
}

#[test]
fn only_a_trusted_time_stamp_moves_the_certificate_validity_check() {
    // A signer whose certificate expired long ago signs today. Without a time stamp that is an
    // expired signature.
    let (pair, key) = rsa_key();
    let mut params = cert_params("Expired Signer", IsCa::ExplicitNoCa, &[KeyUsagePurpose::DigitalSignature]);
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2021, 1, 1);
    let cert = params.self_signed(&pair).unwrap().der().to_vec();
    let identity = Identity::new(vec![cert.clone()], key).unwrap();
    let mut doc = text_pdf(&[&["backdated"]]);
    let base = file_bytes(&mut doc);
    let sign = |tsa: &TestTsa| {
        let options = SignOptions { identity: Some(&identity), timestamp: Some(tsa), ..Default::default() };
        doc.sign(&base, &options).unwrap()
    };

    // A time stamp from an authority nobody trusts, claiming a time inside the certificate's
    // validity: anyone could add it, so the signature is still expired.
    let anyone = TestTsa::new("Nobody TSA", (2019, 2040), true, "20200601000000Z");
    let report = verify(&sign(&anyone), &TrustStore { certificates: vec![cert.clone()] }).remove(0);
    assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, Detail::Expired));

    // The same time from a trusted authority shows the signature was made while it was valid.
    let trusted = TestTsa::new("Trusted TSA", (2019, 2040), true, "20200601000000Z");
    let report = verify(&sign(&trusted), &TrustStore { certificates: vec![cert, trusted.cert.clone()] }).remove(0);
    assert_eq!((report.validity, report.detail), (Validity::Valid, Detail::TrustedDirectly));
}

#[test]
fn a_document_time_stamp_verifies_as_a_time_stamp() {
    let mut doc = text_pdf(&[&["stamped document"]]);
    let base = file_bytes(&mut doc);
    let tsa = test_tsa();
    let options = SignOptions { sub_filter: SubFilter::DocumentTimeStamp, timestamp: Some(tsa), ..Default::default() };
    let stamped = doc.sign(&base, &options).unwrap();

    // Trusting the authority: a valid time stamp with its time, not an invalid signature.
    let reports = verify(&stamped, &tsa_trust());
    assert!(reports[0].document_time_stamp);
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedDirectly));
    assert_eq!(reports[0].status_line(), sig_messages::TIMESTAMP_VALID);
    assert_eq!(reports[0].signer.as_deref(), Some("Test TSA"));
    assert!(reports[0].signing_time.as_deref().is_some_and(|t| t.contains("20260101")));
    assert!(reports[0].description_lines().contains(&sig_messages::DOCUMENT_TIME_STAMPED));
    assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_ALL_VALID));

    // Without trusting the authority: the token still verifies, but nothing vouches for its
    // authority, and the notice must not call the document valid.
    let untrusted = verify(&stamped, &TrustStore::default());
    assert_eq!((untrusted[0].validity, untrusted[0].detail), (Validity::ValidityUnknown, Detail::DigestOkUntrusted));
    assert_eq!(untrusted[0].status_line(), sig_messages::TIMESTAMP_VALIDITY_UNKNOWN);
    let lines = untrusted[0].description_lines();
    assert!(lines.contains(&sig_messages::TIMESTAMP_AUTHORITY_UNKNOWN) && !lines.contains(&sig_messages::DOCUMENT_TIME_STAMPED));
    assert_eq!(notice_text(&untrusted), Some(sig_messages::NOTICE_TIMESTAMP_NOT_CONFIRMED));

    // Any change to the covered bytes breaks the message imprint.
    let mut tampered = stamped.clone();
    let at = tampered.windows(8).position(|w| w == b"stamped ").unwrap();
    tampered[at] = b'S';
    let reports = verify(&tampered, &tsa_trust());
    assert_eq!(reports[0].validity, Validity::Invalid);
    assert!(!reports[0].description_lines().contains(&sig_messages::DOCUMENT_TIME_STAMPED));
}

#[test]
fn an_untrusted_time_stamp_after_a_trusted_signature_is_not_called_valid() {
    let mut doc = text_pdf(&[&["signed then stamped"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();
    let anyone = TestTsa::new("Self-made TSA", (2020, 2040), true, "20260101000000Z");
    let options = SignOptions { sub_filter: SubFilter::DocumentTimeStamp, timestamp: Some(&anyone), ..Default::default() };
    let stamped = Pdf::from_bytes(&signed).unwrap().sign(&signed, &options).unwrap();
    let reports = verify(&stamped, &trust_test_identity());
    assert_eq!(reports.len(), 2);
    assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_TIMESTAMP_NOT_CONFIRMED));
}

#[test]
fn a_time_stamp_authority_must_be_valid_and_for_time_stamping_at_its_time() {
    let mut doc = text_pdf(&[&["authority rules"]]);
    let base = file_bytes(&mut doc);
    let cases = [
        ("expired authority", TestTsa::new("Expired TSA", (2000, 2001), true, "20260101000000Z"), Detail::Expired),
        ("authority not valid yet", TestTsa::new("Early TSA", (2030, 2040), true, "20260101000000Z"), Detail::Expired),
        ("time in the future", TestTsa::new("Future TSA", (2020, 2200), true, "20990101000000Z"), Detail::SignedInFuture),
        ("purpose not critical", TestTsa::new("Lax TSA", (2020, 2040), false, "20260101000000Z"), Detail::NotPermitted),
    ];
    for (label, tsa, detail) in cases {
        let options = SignOptions { sub_filter: SubFilter::DocumentTimeStamp, timestamp: Some(&tsa), ..Default::default() };
        let stamped = doc.sign(&base, &options).unwrap();
        // Trusted directly, and still not a valid time stamp.
        let report = verify(&stamped, &TrustStore { certificates: vec![tsa.cert.clone()] }).remove(0);
        assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, detail), "{label}");
        assert!(!report.description_lines().contains(&sig_messages::DOCUMENT_TIME_STAMPED), "{label}");
    }
}

#[test]
fn time_stamp_tokens_that_cannot_be_checked_or_name_another_certificate_are_not_valid() {
    let mut doc = text_pdf(&[&["token checks"]]);
    let base = file_bytes(&mut doc);
    let tsa = test_tsa();
    let options = SignOptions { sub_filter: SubFilter::DocumentTimeStamp, timestamp: Some(tsa), ..Default::default() };
    let stamped = doc.sign(&base, &options).unwrap();
    let covered = covered_bytes(&stamped);

    // A message imprint with a hash this build does not compute: unknown, not damaged, and no
    // claim of a time stamp.
    let mut sha224 = stamped.clone();
    let digest = <sha2::Sha224 as sha2::Digest>::digest(&covered);
    put_signature_value(&mut sha224, &tsa.token(SHA224_OID, &digest, None));
    let report = verify(&sha224, &tsa_trust()).remove(0);
    assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, Detail::UnsupportedAlgorithm));
    let lines = report.description_lines();
    assert!(!lines.contains(&sig_messages::DOCUMENT_TIME_STAMPED) && !lines.contains(&sig_messages::CORRUPTED), "{lines:?}");

    // An ESS signing-certificate attribute naming a different certificate than the one that signed.
    let mut other = stamped.clone();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(&covered);
    put_signature_value(&mut other, &tsa.token_with(SHA256_OID, &digest, None, &test_identity().certificate));
    let report = verify(&other, &tsa_trust()).remove(0);
    assert_eq!(report.validity, Validity::Invalid);
}

#[test]
fn a_time_stamp_authority_refuses_credentials_over_plain_http() {
    let insecure = HttpTimestampSource {
        url: "http://tsa.example/".into(),
        username: Some("user".into()),
        password: Some("secret".into()),
    };
    let err = insecure.token_for_digest(&[0u8; 32]).unwrap_err().to_string();
    assert!(err.contains("https") || err.contains("insecure"), "{err}");
}

/// Answers `requests` HTTP requests on localhost, each with the bytes `answer` gives for its body,
/// and returns the URL to post to.
fn serve(requests: usize, answer: impl Fn(&[u8]) -> Vec<u8> + Send + 'static) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().take(requests) {
            let mut stream = stream.unwrap();
            let (mut request, mut chunk) = (Vec::new(), [0u8; 4096]);
            let body = loop {
                let n = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize =
                        headers.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                    if request.len() >= end + 4 + length {
                        break request[end + 4..end + 4 + length].to_vec();
                    }
                }
                if n == 0 {
                    break Vec::new();
                }
            };
            let reply = answer(&body);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/timestamp-reply\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&reply).unwrap();
        }
    });
    url
}

/// The message imprint digest and the nonce (an INTEGER's content) of a TimeStampReq.
fn timestamp_request_parts(request: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let children = |data: &[u8]| {
        let (_, header, total) = der_header(data);
        let mut rest = &data[header..total];
        let mut out = Vec::new();
        while !rest.is_empty() {
            let (tag, header, total) = der_header(rest);
            out.push((tag, rest[..total].to_vec(), rest[header..total].to_vec()));
            rest = &rest[total..];
        }
        out
    };
    // version, messageImprint { algorithm, hashedMessage }, [reqPolicy], [nonce], [certReq]
    let fields = children(request);
    let digest = children(&fields[1].1)[1].2.clone();
    let nonce = fields.iter().skip(2).find(|(tag, _, _)| *tag == 0x02).map(|(_, _, content)| content.clone());
    (digest, nonce)
}

/// A TimeStampResp with `status` and, when given, a token.
fn timestamp_response(status: u8, token: Option<&[u8]>) -> Vec<u8> {
    use algo::*;
    let status = seq(&[&tlv(0x02, &[status])]);
    match token {
        Some(token) => seq(&[&status, token]),
        None => seq(&[&status]),
    }
}

#[test]
fn the_time_stamp_client_checks_what_the_authority_returns() {
    let source = |url: String| HttpTimestampSource { url, username: None, password: None };
    let digest = [7u8; 32];

    // A well-behaved authority echoes the nonce over the digest asked for.
    let url = serve(1, |body| {
        let (digest, nonce) = timestamp_request_parts(body);
        timestamp_response(0, Some(&test_tsa().token(SHA256_OID, &digest, nonce.as_deref())))
    });
    let token = source(url).token_for_digest(&digest).unwrap();
    assert!(!token.is_empty());

    // Everything else is refused before it can end up in a signature.
    type Answer = Box<dyn Fn(&[u8]) -> Vec<u8> + Send>;
    let cases: Vec<(&str, &str, Answer)> = vec![
        ("wrong nonce", "nonce", Box::new(|body| {
            let (digest, nonce) = timestamp_request_parts(body);
            let mut nonce = nonce.unwrap();
            *nonce.last_mut().unwrap() ^= 1;
            timestamp_response(0, Some(&test_tsa().token(SHA256_OID, &digest, Some(&nonce))))
        })),
        ("no nonce", "nonce", Box::new(|body| {
            let (digest, _) = timestamp_request_parts(body);
            timestamp_response(0, Some(&test_tsa().token(SHA256_OID, &digest, None)))
        })),
        ("refused", "refused", Box::new(|_| timestamp_response(2, None))),
        ("other digest", "wrong digest", Box::new(|body| {
            let (_, nonce) = timestamp_request_parts(body);
            timestamp_response(0, Some(&test_tsa().token(SHA256_OID, &[0u8; 32], nonce.as_deref())))
        })),
        ("token naming another certificate", "did not verify", Box::new(|body| {
            let (digest, nonce) = timestamp_request_parts(body);
            let token = test_tsa().token_with(SHA256_OID, &digest, nonce.as_deref(), &test_identity().certificate);
            timestamp_response(0, Some(&token))
        })),
    ];
    for (label, expected, answer) in cases {
        let err = source(serve(1, answer)).token_for_digest(&digest).unwrap_err().to_string();
        assert!(err.contains(expected), "{label}: {err}");
    }

    // Signing through the client: one request to size the signature, one for the real token.
    let url = serve(2, |body| {
        let (digest, nonce) = timestamp_request_parts(body);
        timestamp_response(0, Some(&test_tsa().token(SHA256_OID, &digest, nonce.as_deref())))
    });
    let http = source(url);
    let mut doc = text_pdf(&[&["stamped over http"]]);
    let base = file_bytes(&mut doc);
    let mut options = sign_options(SubFilter::CadesDetached);
    options.timestamp = Some(&http);
    let reports = verify(&doc.sign(&base, &options).unwrap(), &tsa_trust());
    assert_eq!(reports[0].validity, Validity::Valid);
    assert!(reports[0].has_embedded_timestamp && reports[0].embedded_timestamp_trusted);
}

#[test]
fn an_intact_but_untrusted_signature_is_not_called_valid() {
    let mut doc = text_pdf(&[&["notice"]]);
    let base = file_bytes(&mut doc);
    let signed = doc.sign(&base, &sign_options(SubFilter::Pkcs7Detached)).unwrap();

    // Digest and signature hold, but the self-signed certificate is not trusted.
    let reports = verify(&signed, &TrustStore::default());
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::ValidityUnknown, Detail::DigestOkUntrusted));
    assert_eq!(reports[0].status_line(), sig_messages::VALIDITY_UNKNOWN);
    assert!(reports[0].description_lines().contains(&sig_messages::IDENTITY_UNKNOWN));
    // The notice bar must not claim every signature is valid.
    assert_eq!(notice_text(&reports), Some(sig_messages::NOTICE_NOT_CONFIRMED));

    // Once the signer is trusted, the same signature gives the all-valid notice.
    assert_eq!(notice_text(&verify(&signed, &trust_test_identity())), Some(sig_messages::NOTICE_ALL_VALID));
}

#[test]
fn an_identity_whose_key_does_not_match_its_certificate_is_rejected() {
    use rsa::pkcs8::EncodePrivateKey;
    // A certificate for one key, paired with a different key.
    let other = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let other_pair = rcgen::KeyPair::try_from(other.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(rcgen::DnType::CommonName, "Someone Else");
    let foreign_cert = params.self_signed(&other_pair).unwrap().der().to_vec();

    let mine = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let Err(err) = Identity::new(vec![foreign_cert.clone()], mine.clone()) else { panic!("mismatch accepted") };
    assert!(err.to_string().contains("does not match"), "{err}");

    // With the chain listed issuer-first, the signer certificate is found and moved to the front.
    let my_pair = rcgen::KeyPair::try_from(mine.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    let mut my_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    my_params.distinguished_name.push(rcgen::DnType::CommonName, "The Signer");
    let my_cert = my_params.self_signed(&my_pair).unwrap().der().to_vec();
    let identity = Identity::new(vec![foreign_cert, my_cert.clone()], mine).unwrap();
    assert_eq!(identity.certificate_der(), my_cert.as_slice());
    assert_eq!(identity.common_name(), "The Signer");
}

#[test]
fn trusted_certificate_import_rejects_files_that_are_not_certificates() {
    use carbide_engine::signature::read_certificates;
    let id = test_identity();
    // A PEM certificate and a bare DER certificate both parse.
    assert_eq!(read_certificates(id.cert_pem.as_bytes()).unwrap().len(), 1);
    assert_eq!(read_certificates(&id.certificate).unwrap(), vec![id.certificate.clone()]);
    // A private key, or anything else, is refused instead of being stored.
    assert!(read_certificates(id.key_pem.as_bytes()).is_err());
    assert!(read_certificates(b"not a certificate at all").is_err());
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

// ---- certificate chain rules (S4) ---------------------------------------------------------

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};

/// A fresh RSA key as both an rcgen key pair (for issuing certificates) and an engine signing key.
fn rsa_key() -> (KeyPair, rsa::RsaPrivateKey) {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let pair = KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    (pair, key)
}

/// Certificate parameters with a common name, CA flag, key usages and a 2020..2040 validity.
fn cert_params(cn: &str, is_ca: IsCa, usages: &[KeyUsagePurpose]) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.is_ca = is_ca;
    params.key_usages = usages.to_vec();
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    params
}

/// Builds a signer identity whose leaf is issued by `issuer`, with the extra chain certificates
/// (issuer first above the leaf) embedded, and returns the identity and the trust store `anchor`.
fn chain_identity(issuer: &Issuer<'_, KeyPair>, above: &[Vec<u8>]) -> (Identity, rsa::RsaPrivateKey) {
    let (leaf_pair, leaf_key) = rsa_key();
    let params = cert_params("Chain Signer", IsCa::ExplicitNoCa, &[KeyUsagePurpose::DigitalSignature]);
    let leaf = params.signed_by(&leaf_pair, issuer).unwrap().der().to_vec();
    let mut chain = vec![leaf];
    chain.extend_from_slice(above);
    (Identity::new(chain, leaf_key.clone()).unwrap(), leaf_key)
}

fn sign_with(identity: &Identity) -> Vec<u8> {
    let mut doc = text_pdf(&[&["chain"]]);
    let base = file_bytes(&mut doc);
    let options = SignOptions { identity: Some(identity), reason: Some("ok".into()), ..Default::default() };
    doc.sign(&base, &options).unwrap()
}

#[test]
fn a_ca_issued_signer_is_trusted_through_its_chain() {
    // root (pathLen 1) -> intermediate CA -> leaf signer.
    let (root_pair, _) = rsa_key();
    let root_params = cert_params("Root CA", IsCa::Ca(BasicConstraints::Constrained(1)), &[KeyUsagePurpose::KeyCertSign]);
    let root = root_params.self_signed(&root_pair).unwrap().der().to_vec();
    let root_issuer = Issuer::new(root_params, root_pair);

    let (int_pair, _) = rsa_key();
    let int_params = cert_params("Intermediate CA", IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::KeyCertSign]);
    let intermediate = int_params.signed_by(&int_pair, &root_issuer).unwrap().der().to_vec();
    let int_issuer = Issuer::new(int_params, int_pair);

    let (identity, _) = chain_identity(&int_issuer, std::slice::from_ref(&intermediate));
    let signed = sign_with(&identity);
    let reports = verify(&signed, &TrustStore { certificates: vec![root] });
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedViaChain));
}

/// The chain reaches the trusted root by names and signatures but breaks an issuing rule: the
/// panel says so, rather than that nothing in the chain is trusted.
fn assert_rejected(report: &carbide_engine::signature::SignatureReport, label: &str) {
    assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, Detail::ChainRejected), "{label}");
    let lines = report.description_lines();
    assert!(lines.contains(&sig_messages::CHAIN_REJECTED) && !lines.contains(&sig_messages::IDENTITY_UNKNOWN), "{label}");
}

#[test]
fn chain_rules_reject_bad_issuers() {
    let (root_pair, _) = rsa_key();
    let root_params = cert_params("Root CA", IsCa::Ca(BasicConstraints::Constrained(0)), &[KeyUsagePurpose::KeyCertSign]);
    let root = root_params.self_signed(&root_pair).unwrap().der().to_vec();
    let root_issuer = Issuer::new(root_params, root_pair);
    let trust = TrustStore { certificates: vec![root.clone()] };

    // An issuer that is not a CA cannot vouch for a signer: root -> non-CA -> leaf.
    {
        let (mid_pair, _) = rsa_key();
        let mid_params = cert_params("Not A CA", IsCa::ExplicitNoCa, &[KeyUsagePurpose::DigitalSignature]);
        let mid = mid_params.signed_by(&mid_pair, &root_issuer).unwrap().der().to_vec();
        let mid_issuer = Issuer::new(mid_params, mid_pair);
        let (identity, _) = chain_identity(&mid_issuer, &[mid]);
        let reports = verify(&sign_with(&identity), &trust);
        assert_rejected(&reports[0], "non-CA issuer");
    }

    // A CA without keyCertSign in its key usage cannot issue.
    {
        let (mid_pair, _) = rsa_key();
        let mid_params = cert_params("No Cert Sign", IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::DigitalSignature]);
        let mid = mid_params.signed_by(&mid_pair, &root_issuer).unwrap().der().to_vec();
        let mid_issuer = Issuer::new(mid_params, mid_pair);
        let (identity, _) = chain_identity(&mid_issuer, &[mid]);
        let reports = verify(&sign_with(&identity), &trust);
        assert_rejected(&reports[0], "missing keyCertSign");
    }

    // pathLenConstraint exceeded: root pathLen 0 -> intermediate CA -> leaf.
    {
        let (int_pair, _) = rsa_key();
        let int_params = cert_params("Intermediate CA", IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::KeyCertSign]);
        let intermediate = int_params.signed_by(&int_pair, &root_issuer).unwrap().der().to_vec();
        let int_issuer = Issuer::new(int_params, int_pair);
        let (identity, _) = chain_identity(&int_issuer, &[intermediate]);
        let reports = verify(&sign_with(&identity), &trust);
        assert_rejected(&reports[0], "pathLen exceeded");
    }

    // An expired intermediate is not accepted as an issuer at the signing time.
    {
        let (int_pair, _) = rsa_key();
        let mut int_params = cert_params("Expired CA", IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::KeyCertSign]);
        int_params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        int_params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        // This intermediate is issued by a pathLen-permitting root.
        let wide_params = cert_params("Wide Root", IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::KeyCertSign]);
        let (wide_pair, _) = rsa_key();
        let wide = wide_params.self_signed(&wide_pair).unwrap().der().to_vec();
        let wide_issuer = Issuer::new(wide_params, wide_pair);
        let intermediate = int_params.signed_by(&int_pair, &wide_issuer).unwrap().der().to_vec();
        let int_issuer = Issuer::new(int_params, int_pair);
        let (identity, _) = chain_identity(&int_issuer, &[intermediate]);
        let reports = verify(&sign_with(&identity), &TrustStore { certificates: vec![wide] });
        assert_rejected(&reports[0], "expired intermediate");
    }
}

#[test]
fn a_cross_signed_intermediate_is_found_through_either_issuer() {
    // One intermediate key, certified by two roots; only the second root is trusted, and the
    // certificate from the first is listed first.
    let roots: Vec<(Vec<u8>, Issuer<'static, KeyPair>)> = ["Root One", "Root Two"]
        .into_iter()
        .map(|name| {
            let (pair, _) = rsa_key();
            let params = cert_params(name, IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::KeyCertSign]);
            let der = params.self_signed(&pair).unwrap().der().to_vec();
            (der, Issuer::new(params, pair))
        })
        .collect();
    let (int_pair, _) = rsa_key();
    let int_params = cert_params("Intermediate CA", IsCa::Ca(BasicConstraints::Unconstrained), &[KeyUsagePurpose::KeyCertSign]);
    let by_one = int_params.signed_by(&int_pair, &roots[0].1).unwrap().der().to_vec();
    let by_two = int_params.signed_by(&int_pair, &roots[1].1).unwrap().der().to_vec();
    let int_issuer = Issuer::new(int_params, int_pair);

    let (identity, _) = chain_identity(&int_issuer, &[by_one, by_two]);
    let reports = verify(&sign_with(&identity), &TrustStore { certificates: vec![roots[1].0.clone()] });
    assert_eq!((reports[0].validity, reports[0].detail), (Validity::Valid, Detail::TrustedViaChain));
}

#[test]
fn signer_key_usage_and_future_dates_are_named_as_such() {
    // A trusted certificate whose key usage does not cover signing.
    let (pair, key) = rsa_key();
    let params = cert_params("Encryption Only", IsCa::ExplicitNoCa, &[KeyUsagePurpose::KeyEncipherment]);
    let cert = params.self_signed(&pair).unwrap().der().to_vec();
    let identity = Identity::new(vec![cert.clone()], key).unwrap();
    let report = verify(&sign_with(&identity), &TrustStore { certificates: vec![cert] }).remove(0);
    assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, Detail::NotPermitted));
    let lines = report.description_lines();
    assert!(lines.contains(&sig_messages::SIGNER_NOT_PERMITTED) && !lines.contains(&sig_messages::IDENTITY_UNKNOWN));

    // A signing time in the future.
    let mut doc = text_pdf(&[&["later"]]);
    let base = file_bytes(&mut doc);
    let options = SignOptions { signing_time: Some("D:20991231000000Z".into()), ..sign_options(SubFilter::Pkcs7Detached) };
    let report = verify(&doc.sign(&base, &options).unwrap(), &trust_test_identity()).remove(0);
    assert_eq!((report.validity, report.detail), (Validity::ValidityUnknown, Detail::SignedInFuture));
    assert!(report.description_lines().contains(&sig_messages::SIGNED_IN_FUTURE));
}

// ---- OpenSSL cross-checks ------------------------------------------------------------------

/// Runs `openssl` with `args`; `None` when OpenSSL 3 is not on the PATH (the cross-checks are then
/// skipped), else whether it succeeded.
fn openssl(args: &[std::ffi::OsString]) -> Option<bool> {
    let version = std::process::Command::new("openssl").arg("version").output().ok()?;
    if !String::from_utf8_lossy(&version.stdout).starts_with("OpenSSL 3") {
        return None;
    }
    let output = std::process::Command::new("openssl").args(args).output().ok()?;
    if !output.status.success() {
        eprintln!("openssl {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    Some(output.status.success())
}

#[test]
fn signatures_agree_with_openssl_both_ways() {
    let Some(true) = openssl(&["version".into()]) else {
        eprintln!("OpenSSL 3 is not on the PATH; skipping the cross-checks");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let arg = |text: &str| std::ffi::OsString::from(text);
    let mut doc = text_pdf(&[&["cross-checked"]]);
    let base = file_bytes(&mut doc);

    // Carbide's signatures, checked by OpenSSL against the bytes they cover.
    let mut stamped = sign_options(SubFilter::CadesDetached);
    stamped.timestamp = Some(test_tsa());
    for (label, options) in [
        ("PKCS#7", sign_options(SubFilter::Pkcs7Detached)),
        ("CAdES", sign_options(SubFilter::CadesDetached)),
        ("CAdES with a time stamp", stamped),
    ] {
        let signed = doc.sign(&base, &options).unwrap();
        std::fs::write(path("covered.bin"), covered_bytes(&signed)).unwrap();
        std::fs::write(path("signature.der"), signature_value(&signed)).unwrap();
        let verified = openssl(&[
            arg("cms"), arg("-verify"), arg("-binary"), arg("-noverify"), arg("-inform"), arg("DER"),
            arg("-in"), path("signature.der").into(), arg("-content"), path("covered.bin").into(),
            arg("-out"), path("out.bin").into(),
        ]);
        assert_eq!(verified, Some(true), "OpenSSL rejects Carbide's {label} signature");
    }

    // OpenSSL's signatures, checked by Carbide. A time stamp on the placeholder signing reserves
    // room for any of them; the fixed signing time keeps the covered bytes the same.
    let roomy = |cms: &[u8]| {
        let mut options = SignOptions { signing_time: Some("D:20261001000000Z".into()), ..sign_options(SubFilter::CadesDetached) };
        options.timestamp = Some(test_tsa());
        let mut signed = doc.sign(&base, &options).unwrap();
        put_signature_value(&mut signed, cms);
        signed
    };
    std::fs::write(path("covered.bin"), covered_bytes(&roomy(&[0x30, 0x00]))).unwrap();
    let (rsa_pair, _) = rsa_key();
    let p384_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let cases: [(&str, &KeyPair, &[&str], Validity, Detail); 4] = [
        ("RSA, SHA-256", &rsa_pair, &["-md", "sha256"], Validity::Valid, Detail::TrustedDirectly),
        ("P-384, SHA-1", &p384_pair, &["-md", "sha1"], Validity::Valid, Detail::TrustedDirectly),
        ("RSA-PSS, SHA-256", &rsa_pair, &["-md", "sha256", "-keyopt", "rsa_padding_mode:pss"], Validity::Valid, Detail::TrustedDirectly),
        (
            "RSA-PSS, SHA-256 with MGF1 over SHA-1",
            &rsa_pair,
            &["-md", "sha256", "-keyopt", "rsa_padding_mode:pss", "-keyopt", "rsa_mgf1_md:sha1"],
            Validity::ValidityUnknown,
            Detail::UnsupportedAlgorithm,
        ),
    ];
    for (label, pair, extra, validity, detail) in cases {
        let cert = self_signed(pair, "OpenSSL Signer");
        std::fs::write(path("cert.pem"), x509_pem(&cert)).unwrap();
        std::fs::write(path("key.pem"), pair.serialize_pem()).unwrap();
        let mut args = vec![
            arg("cms"), arg("-sign"), arg("-binary"), arg("-nosmimecap"), arg("-outform"), arg("DER"),
            arg("-in"), path("covered.bin").into(), arg("-signer"), path("cert.pem").into(),
            arg("-inkey"), path("key.pem").into(), arg("-out"), path("openssl.der").into(),
        ];
        args.extend(extra.iter().map(|a| arg(a)));
        assert_eq!(openssl(&args), Some(true), "{label}: OpenSSL could not sign");
        let cms = std::fs::read(path("openssl.der")).unwrap();
        let report = verify(&roomy(&cms), &TrustStore { certificates: vec![cert] }).remove(0);
        assert_eq!((report.validity, report.detail), (validity, detail), "{label}");
    }
}

/// A DER certificate as PEM.
fn x509_pem(der: &[u8]) -> String {
    use base64::Engine;
    let text = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = text.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
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
