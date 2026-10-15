//! The command line saves edits the way the desktop app does: a protected input stays protected.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carbide_engine::signature::{Identity, SignOptions};
use carbide_engine::{Error, Pdf, ProtectionOptions};

fn carbide(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carbide")).args(args).output().unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("carbide-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A one-page file showing "private words", protected with `user` (may be empty) and `owner`.
fn protected(path: &Path, user: &str, owner: &str) {
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    pdf.add_text_box(0, 72.0, 720.0, "private words", 18.0).unwrap();
    std::fs::write(path, pdf.to_encrypted_bytes(&ProtectionOptions::new(user, owner)).unwrap()).unwrap();
}

fn key(bytes: &[u8]) -> Vec<u8> {
    let pdf = Pdf::from_bytes_with_password(bytes, "owner pw").unwrap();
    pdf.document().encryption_state.as_ref().expect("still encrypted").file_encryption_key().to_vec()
}

#[test]
fn editing_a_protected_file_in_place_keeps_it_encrypted() {
    let dir = scratch("in-place");
    for (user, password) in [("", None), ("user pw", Some("user pw"))] {
        let file = dir.join("f.pdf");
        protected(&file, user, "owner pw");
        let before = key(&std::fs::read(&file).unwrap());
        let path = file.to_str().unwrap();
        let mut args = vec!["rotate", path, "1", "90", path];
        args.extend(password.iter().flat_map(|p| ["--password", p]));
        let output = carbide(&args);
        assert!(output.status.success(), "{user:?}: {}", String::from_utf8_lossy(&output.stderr));

        let bytes = std::fs::read(&file).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"private"), "{user:?}: written in the clear");
        assert_eq!(key(&bytes), before, "{user:?}: encrypted with another key");
        assert_eq!(Pdf::from_bytes_with_password(&bytes, "owner pw").unwrap().page_rotation(0).unwrap(), 90);
        if !user.is_empty() {
            assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)));
            assert!(Pdf::from_bytes_with_password(&bytes, user).is_ok());
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn new_documents_are_not_written_unencrypted_over_a_protected_input() {
    let dir = scratch("new-document");
    let file = dir.join("f.pdf");
    protected(&file, "", "owner pw");
    let before = std::fs::read(&file).unwrap();
    let path = file.to_str().unwrap();
    let output = carbide(&["extract", path, "1", path, "--unprotected"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("password-protected"));
    assert_eq!(std::fs::read(&file).unwrap(), before);

    // To another file is fine: that is a new, separate document.
    let copy = dir.join("copy.pdf");
    assert!(carbide(&["extract", path, "1", copy.to_str().unwrap(), "--unprotected"]).status.success());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn new_documents_from_a_protected_input_keep_its_protection_unless_asked() {
    let dir = scratch("derived");
    let file = dir.join("f.pdf");
    protected(&file, "user pw", "owner pw");
    let before = key(&std::fs::read(&file).unwrap());
    let path = file.to_str().unwrap();
    let plain = dir.join("plain.pdf");
    plain_file(&plain, &["plain page"]);
    let out = |name: &str| dir.join(name).to_str().unwrap().to_string();

    for (what, args) in [
        ("extract", vec!["extract", path, "1", &out("extract.pdf")]),
        ("split", vec!["split", path, "1", &out("parts")]),
        ("impose", vec!["impose", path, &out("impose.pdf")]),
        ("merge", vec!["merge", &out("merge.pdf"), path, plain.to_str().unwrap()]),
    ] {
        let mut args: Vec<&str> = args;
        args.extend(["--password", "user pw"]);
        let output = carbide(&args);
        assert!(output.status.success(), "{what}: {}", stderr(&output));
        let written = match what {
            "split" => dir.join("parts/f-pages-1-1.pdf"),
            name => dir.join(format!("{name}.pdf")),
        };
        let bytes = std::fs::read(&written).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"private"), "{what}: written in the clear");
        assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)), "{what}");
        assert!(Pdf::from_bytes_with_password(&bytes, "user pw").is_ok(), "{what}");
        assert_eq!(key(&bytes), before, "{what}: protected with another key");
    }

    // Without protection only when asked for.
    let unprotected = out("unprotected.pdf");
    let output = carbide(&["extract", path, "1", &unprotected, "--password", "user pw", "--unprotected"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(Pdf::open(&unprotected).unwrap().document().encryption_state.is_none());

    // Two inputs protected in different ways cannot share one protection.
    let other = dir.join("other.pdf");
    protected(&other, "user pw", "owner pw");
    let output = carbide(&["merge", &out("both.pdf"), path, other.to_str().unwrap(), "--password", "user pw"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--unprotected"), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn protected_documents_are_exported_only_when_asked() {
    let dir = scratch("export-protected");
    let file = dir.join("f.pdf");
    // Page text, not a text box, so that it is part of the exported text.
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    pdf.apply_watermark(&carbide_engine::WatermarkOptions::new("private words")).unwrap();
    std::fs::write(&file, pdf.to_encrypted_bytes(&ProtectionOptions::new("", "owner pw")).unwrap()).unwrap();
    let path = file.to_str().unwrap();
    let images = dir.join("images");

    let output = carbide(&["text", path]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--unprotected"), "{}", stderr(&output));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private"));
    let output = carbide(&["images", path, images.to_str().unwrap(), "--dpi", "20"]);
    assert!(!output.status.success());
    assert!(!images.join("f-001.png").exists());

    let output = carbide(&["text", path, "--unprotected"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("private words"));
    let output = carbide(&["images", path, images.to_str().unwrap(), "--dpi", "20", "--unprotected"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(images.join("f-001.png").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compare_prints_the_text_of_a_protected_document_only_when_asked() {
    let dir = scratch("compare-protected");
    let restricted = dir.join("restricted.pdf");
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    pdf.apply_watermark(&carbide_engine::WatermarkOptions::new("private words")).unwrap();
    std::fs::write(&restricted, pdf.to_encrypted_bytes(&ProtectionOptions::new("", "owner pw")).unwrap()).unwrap();
    let blank = dir.join("blank.pdf");
    plain_file(&blank, &[]);
    let (restricted, blank) = (restricted.to_str().unwrap(), blank.to_str().unwrap());

    // It opens without a password, but its text is not printed without --unprotected, whichever
    // side it is on.
    for args in [["compare", restricted, blank], ["compare", blank, restricted]] {
        let output = carbide(&args);
        assert!(!output.status.success());
        assert!(stderr(&output).contains("--unprotected"), "{}", stderr(&output));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private"));
    }
    let output = carbide(&["compare", restricted, blank, "--unprotected"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("private words"));
    let _ = std::fs::remove_dir_all(dir);
}

/// A plain file with one page per label.
fn plain_file(path: &Path, labels: &[&str]) {
    let mut pdf = Pdf::new();
    for (i, label) in labels.iter().enumerate() {
        pdf.insert_blank_page(i, Some((612.0, 792.0))).unwrap();
        pdf.add_text_box(i, 72.0, 720.0, label, 18.0).unwrap();
    }
    if labels.is_empty() {
        pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    }
    std::fs::write(path, pdf.to_bytes().unwrap()).unwrap();
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn split_and_image_export_replace_files_only_with_force() {
    let dir = scratch("existing");
    let input = dir.join("doc.pdf");
    plain_file(&input, &["one", "two"]);
    let input = input.to_str().unwrap();

    let parts = dir.join("parts");
    std::fs::create_dir_all(&parts).unwrap();
    std::fs::write(parts.join("doc-pages-2-2.pdf"), b"an earlier part").unwrap();
    let output = carbide(&["split", input, "1,2", parts.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("doc-pages-2-2.pdf already exists") && stderr(&output).contains("--force"), "{}", stderr(&output));
    assert_eq!(std::fs::read(parts.join("doc-pages-2-2.pdf")).unwrap(), b"an earlier part");
    assert!(!parts.join("doc-pages-1-1.pdf").exists(), "nothing is written when a name is taken");
    let output = carbide(&["split", input, "1,2", parts.to_str().unwrap(), "--force"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(Pdf::open(parts.join("doc-pages-2-2.pdf")).unwrap().page_count(), 1);
    // A range given twice would write one part over the other: refused before anything is written.
    let again = dir.join("again");
    std::fs::create_dir_all(&again).unwrap();
    let output = carbide(&["split", input, "1-2,2,1-2", again.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("doc-pages-1-2.pdf twice"), "{}", stderr(&output));
    assert_eq!(std::fs::read_dir(&again).unwrap().count(), 0, "a part was written");

    let images = dir.join("images");
    std::fs::create_dir_all(&images).unwrap();
    std::fs::write(images.join("doc-001.png"), b"an earlier image").unwrap();
    let output = carbide(&["images", input, images.to_str().unwrap(), "--dpi", "20"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("doc-001.png already exists"), "{}", stderr(&output));
    assert_eq!(std::fs::read(images.join("doc-001.png")).unwrap(), b"an earlier image");
    assert!(!images.join("doc-002.png").exists());
    let output = carbide(&["images", input, images.to_str().unwrap(), "--dpi", "20", "--force"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_ne!(std::fs::read(images.join("doc-001.png")).unwrap(), b"an earlier image");
    assert!(images.join("doc-002.png").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reduce_keeps_the_originals_and_refuses_names_that_collide() {
    let dir = scratch("reduce");
    // A long text box whose drawing is stored uncompressed, so reducing it gains a lot.
    let bulky = |path: &Path| {
        let mut pdf = Pdf::new();
        pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
        pdf.add_text_box(0, 72.0, 720.0, &"lorem ipsum dolor sit amet ".repeat(400), 10.0).unwrap();
        std::fs::write(path, pdf.to_bytes().unwrap()).unwrap();
    };
    let file = dir.join("big.pdf");
    bulky(&file);
    let before = std::fs::read(&file).unwrap();
    let path = file.to_str().unwrap();

    let output = carbide(&["reduce", path]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(std::fs::read(&file).unwrap(), before, "the original was replaced");
    let reduced = dir.join("big-reduced.pdf");
    assert!(std::fs::metadata(&reduced).unwrap().len() < before.len() as u64);
    // The reduced copy is not replaced without --overwrite.
    std::fs::write(&reduced, b"kept").unwrap();
    assert!(!carbide(&["reduce", path]).status.success());
    assert_eq!(std::fs::read(&reduced).unwrap(), b"kept");
    assert!(carbide(&["reduce", path, "--overwrite"]).status.success());
    assert_ne!(std::fs::read(&reduced).unwrap(), b"kept");

    // Two files called x.pdf into one folder: refused before either is touched.
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    bulky(&a.join("x.pdf"));
    bulky(&b.join("x.pdf"));
    let (a_x, b_x) = (a.join("x.pdf"), b.join("x.pdf"));
    let b_before = std::fs::read(&b_x).unwrap();
    let output = carbide(&["reduce", a_x.to_str().unwrap(), b_x.to_str().unwrap(), "--out-dir", b.to_str().unwrap(), "--suffix", "", "--overwrite"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("same file"), "{}", stderr(&output));
    assert_eq!(std::fs::read(&b_x).unwrap(), b_before);

    // Replacing the originals takes --in-place, and the refusal says so.
    let output = carbide(&["reduce", path, "--suffix", ""]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--in-place"), "{}", stderr(&output));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // --no-overwrite, the default now, is still accepted.
    std::fs::remove_file(&reduced).unwrap();
    let output = carbide(&["reduce", path, "--no-overwrite"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(reduced.exists());
    let output = carbide(&["reduce", path, "--in-place"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(std::fs::read(&file).unwrap().len() < before.len());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sanitize_removes_what_is_chosen_and_keeps_protection() {
    let dir = scratch("sanitize");
    let file = dir.join("noted.pdf");
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    pdf.add_note(0, 100.0, 100.0, "SECRET NOTE").unwrap();
    std::fs::write(&file, pdf.to_bytes().unwrap()).unwrap();
    let path = file.to_str().unwrap();
    let has_note = |path: &Path| std::fs::read(path).unwrap().windows(11).any(|w| w == b"SECRET NOTE");

    let kept = dir.join("kept.pdf");
    let output = carbide(&["sanitize", path, kept.to_str().unwrap(), "--keep", "comments,metadata"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(has_note(&kept), "a kept item was removed");

    let clean = dir.join("clean.pdf");
    let output = carbide(&["sanitize", path, clean.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("removed Comments, text boxes and markup: 1"));
    assert!(!has_note(&clean));
    assert!(has_note(&file), "the input was changed");

    // A note an appended update took off the page is still in the file, in its earlier version:
    // the output is written in full, without it, even when nothing else is chosen.
    let history = dir.join("history.pdf");
    let base = std::fs::read(&file).unwrap();
    let mut later = Pdf::from_bytes(&base).unwrap();
    let page = later.page_ids()[0];
    later.document_mut().get_dictionary_mut(page).unwrap().remove(b"Annots");
    std::fs::write(&history, later.incremental_update(&base).unwrap()).unwrap();
    assert!(has_note(&history));
    let out = dir.join("history-clean.pdf");
    let output = carbide(&["sanitize", history.to_str().unwrap(), out.to_str().unwrap(), "--only", "thumbnails"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("removed Earlier versions and unused objects in the file"));
    assert!(!has_note(&out), "the earlier version was written out again");
    // In place too.
    let output = carbide(&["sanitize", history.to_str().unwrap(), history.to_str().unwrap(), "--only", "thumbnails"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!has_note(&history));

    // A protected input comes out protected the same way.
    let locked = dir.join("locked.pdf");
    protected(&locked, "user pw", "owner pw");
    let before = key(&std::fs::read(&locked).unwrap());
    let out = dir.join("locked-clean.pdf");
    let output = carbide(&["sanitize", locked.to_str().unwrap(), out.to_str().unwrap(), "--only", "comments", "--password", "user pw"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let bytes = std::fs::read(&out).unwrap();
    assert!(matches!(Pdf::from_bytes(&bytes), Err(Error::PasswordRequired)));
    assert_eq!(key(&bytes), before);
    assert!(Pdf::from_bytes_with_password(&bytes, "user pw").unwrap().annotations(0).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// Mean brightness (0..255) of the display-space rect (x, y, w, h) of page `page` of `path`,
/// rendered at 72 dpi.
fn brightness(path: &Path, page: usize, (x, y, w, h): (u32, u32, u32, u32)) -> f32 {
    let image = carbide_engine::Renderer::global().unwrap().render_page(&std::fs::read(path).unwrap(), page, 72.0).unwrap();
    let top = image.height() - y - h;
    let mut total = 0.0;
    for row in top..top + h {
        for column in x..x + w {
            let p = image.get_pixel(column, row);
            total += (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0;
        }
    }
    total / (w * h) as f32
}

#[test]
fn arguments_that_would_do_the_wrong_thing_are_refused_or_read_as_meant() {
    let dir = scratch("arguments");
    let file = dir.join("f.pdf");
    plain_file(&file, &["ONE", "TWO"]);
    let (path, out) = (file.to_str().unwrap(), dir.join("out.pdf"));
    let out_path = out.to_str().unwrap();

    // Turning the other way is a negative number of degrees; only quarter turns are taken.
    let output = carbide(&["rotate", path, "1", "-90", out_path]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(Pdf::open(&out).unwrap().page_rotation(0).unwrap(), 270);
    let output = carbide(&["rotate", path, "1", "45", out_path]);
    assert!(!output.status.success() && stderr(&output).contains("multiple of 90"), "{}", stderr(&output));
    // A page named more than once is turned once.
    assert!(carbide(&["rotate", path, "1,1,1-2", "90", out_path]).status.success());
    let turned = Pdf::open(&out).unwrap();
    assert_eq!((turned.page_rotation(0).unwrap(), turned.page_rotation(1).unwrap()), (90, 90));

    // Merging nothing writes nothing over the output.
    let before = std::fs::read(&out).unwrap();
    let output = carbide(&["merge", out_path]);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&out).unwrap(), before);

    // Bates numbers that cannot be stamped are an error, not a crash.
    for extra in [["--digits", "70000"], ["--start", "18446744073709551615"]] {
        let output = carbide(&[&["bates", path, out_path][..], &extra[..]].concat());
        assert!(!output.status.success() && stderr(&output).starts_with("error: "), "{extra:?}: {}", stderr(&output));
    }

    // An absurd imposition grid is refused before any work.
    let output = carbide(&["impose", path, out_path, "--layout", "step-and-repeat", "--columns", "1000000000"]);
    assert!(!output.status.success() && stderr(&output).contains("Columns and rows"), "{}", stderr(&output));

    // A rectangle given from its other corner redacts the same area; one with no area is refused.
    let output = carbide(&["redact", path, out_path, "--rect", "1:360,720,-300,-28"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(brightness(&out, 0, (70, 694, 280, 24)) < 30.0, "the area was not blacked out");
    for rect in ["1:100,100,0,50", "1:nan,100,10,10", "1:100,100,inf,10"] {
        let output = carbide(&["redact", path, out_path, "--rect", rect]);
        assert!(!output.status.success(), "{rect}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn verify_reads_files_that_need_their_open_password() {
    let dir = scratch("verify");
    let file = dir.join("locked.pdf");
    protected(&file, "user pw", "owner pw");
    let path = file.to_str().unwrap();
    let output = carbide(&["verify", path]);
    assert!(!output.status.success() && stderr(&output).contains("password"), "{}", stderr(&output));
    let output = carbide(&["verify", path, "--password", "user pw"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("No signatures found."));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn inputs_that_cannot_work_are_refused_with_messages_that_say_why() {
    let dir = scratch("inputs");
    let file = dir.join("f.pdf");
    plain_file(&file, &["ONE", "TWO"]);
    let (path, out) = (file.to_str().unwrap(), dir.join("out.pdf"));
    let out_path = out.to_str().unwrap();

    // A watermark that cannot be drawn is refused, and nothing is written.
    for extra in [&["--size", "nan"][..], &["--size", "inf"], &["--size", "0"], &["--size=-5"], &["--angle", "nan"], &["--opacity", "2"]] {
        let output = carbide(&[&["watermark", path, out_path, "--text", "HI"][..], extra].concat());
        assert!(!output.status.success() && stderr(&output).contains("watermark size"), "{extra:?}: {}", stderr(&output));
        assert!(!out.exists(), "{extra:?}: an output was written");
    }
    // A redacted page rasterized at 1 dpi would be unreadable.
    let output = carbide(&["redact", path, out_path, "--rect", "1:72,700,100,30", "--dpi", "1"]);
    assert!(!output.status.success() && stderr(&output).contains("72 dpi"), "{}", stderr(&output));

    // Pages are named the way they were given, counted from 1.
    let output = carbide(&["move", path, "6", "1", out_path]);
    assert!(stderr(&output).contains("page 6 does not exist (the document has 2 pages)"), "{}", stderr(&output));
    let output = carbide(&["blank", path, "7", out_path]);
    assert!(stderr(&output).contains("page 7 does not exist"), "{}", stderr(&output));
    // A file that cannot be read is named.
    let missing = dir.join("missing.pdf");
    let output = carbide(&["verify", missing.to_str().unwrap()]);
    assert!(!output.status.success() && stderr(&output).contains("missing.pdf"), "{}", stderr(&output));

    // A page tree whose count is too large a number for the file to be read: every viewer shows
    // its pages, so it is refused rather than read as a document without pages.
    let damaged = dir.join("damaged.pdf");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 9223372036854775808 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
    ];
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    std::fs::write(&damaged, bytes).unwrap();
    for args in [&["info", damaged.to_str().unwrap()][..], &["sanitize", damaged.to_str().unwrap(), out_path]] {
        let output = carbide(args);
        assert!(!output.status.success() && stderr(&output).contains("page tree is damaged"), "{args:?}: {}", stderr(&output));
    }
    assert!(!out.exists());

    // PDFium that cannot be loaded is said once.
    let bogus = dir.join("libpdfium.dylib");
    std::fs::write(&bogus, b"not a library").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carbide")).args(["text", path]).env("CARBIDE_PDFIUM_LIB", &bogus).output().unwrap();
    if !output.status.success() {
        assert_eq!(stderr(&output).matches("rendering backend").count(), 1, "{}", stderr(&output));
    }
    let _ = std::fs::remove_dir_all(dir);
}

// ---- verify ------------------------------------------------------------------------------------

/// A signing identity named `name` whose certificate is valid from 1 January of `from` to 1 January
/// of `until`, and that certificate as PEM.
fn identity(name: &str, (from, until): (i32, i32)) -> (Identity, String) {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let pair = rcgen::KeyPair::try_from(key.to_pkcs8_der().unwrap().as_bytes()).unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(rcgen::DnType::CommonName, name);
    params.not_before = rcgen::date_time_ymd(from, 1, 1);
    params.not_after = rcgen::date_time_ymd(until, 1, 1);
    let cert = params.self_signed(&pair).unwrap();
    (Identity::new(vec![cert.der().to_vec()], key).unwrap(), cert.pem())
}

/// A one-page file showing `label`, signed by `identity` with the signing time `time`.
fn signed(identity: &Identity, label: &str, time: &str) -> Vec<u8> {
    let mut pdf = Pdf::new();
    pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    pdf.add_text_box(0, 72.0, 720.0, label, 18.0).unwrap();
    let base = pdf.to_bytes().unwrap();
    let pdf = Pdf::from_bytes(&base).unwrap();
    pdf.sign(&base, &SignOptions { identity: Some(identity), signing_time: Some(time.into()), ..Default::default() }).unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn verify_shows_a_time_stamp_time_only_from_a_time_stamp_that_verifies() {
    let dir = scratch("time-stamp-time");
    let (identity, _) = identity("Some Signer", (2020, 2040));
    // An ordinary signature relabelled as a document time stamp: its /M is no time stamp's time.
    let mut bytes = signed(&identity, "stamped", "D:20000101000000Z");
    let at = bytes.windows(19).rposition(|w| w == b"adbe.pkcs7.detached").unwrap();
    bytes[at..at + 19].copy_from_slice(b"ETSI.RFC3161       ");
    let file = dir.join("stamped.pdf");
    std::fs::write(&file, bytes).unwrap();
    let output = carbide(&["verify", file.to_str().unwrap()]);
    assert!(stdout(&output).contains("The time stamp is not valid"), "{}", stdout(&output));
    assert!(!stdout(&output).contains("  time stamp") && !stdout(&output).contains("2000"), "{}", stdout(&output));

    // An ordinary signature's own time is shown as what it claims.
    let file = dir.join("signed.pdf");
    std::fs::write(&file, signed(&identity, "signed", "D:20200601120000Z")).unwrap();
    let output = carbide(&["verify", file.to_str().unwrap()]);
    assert!(stdout(&output).contains("signing time in signature (unverified): 2020-06-0"), "{}", stdout(&output));
    assert!(!stdout(&output).contains("  time stamp"), "{}", stdout(&output));
    let _ = std::fs::remove_dir_all(dir);
}
