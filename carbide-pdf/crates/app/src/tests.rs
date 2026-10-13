//! How the window's commands drive saving: the save question, what follows a save, background OCR
//! and Auto Recovery. Driven without a window; dialogs are answered the way their buttons do.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use carbide_engine::autosave::{AutoSaveSettings, AutoSaveStore};
use carbide_engine::ocr::OcrReport;
use carbide_engine::{pdf_date_now, Pdf, ProtectionOptions};
use eframe::egui;

use crate::dialogs::{AfterSave, Dialog, Export};
use crate::{CarbideApp, OcrJob, SignJob};

/// A folder of its own for one test.
fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("carbide-app-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The application with no window and no renderer, keeping recovery copies in `dir`.
fn app(dir: &Path) -> CarbideApp {
    let store = AutoSaveStore::open(dir.join("AutoSave")).ok();
    CarbideApp::with_parts(None, None, store, AutoSaveSettings::default())
}

/// A document with one page per label, each showing its label.
fn document(labels: &[&str]) -> Pdf {
    let mut pdf = Pdf::new();
    for (i, label) in labels.iter().enumerate() {
        pdf.insert_blank_page(i, Some((612.0, 792.0))).unwrap();
        pdf.add_text_box(i, 72.0, 720.0, label, 18.0).unwrap();
    }
    pdf
}

/// `labels` written to `name` with one saved revision, so a save that removes content has to ask.
fn with_revision(dir: &Path, name: &str, labels: &[&str]) -> PathBuf {
    let base = document(labels).to_bytes().unwrap();
    let mut working = Pdf::from_bytes(&base).unwrap();
    working.rotate_pages(&[0], 90).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, working.save_revision(&base, "First", &pdf_date_now()).unwrap()).unwrap();
    path
}

fn plain(dir: &Path, name: &str, labels: &[&str]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, document(labels).to_bytes().unwrap()).unwrap();
    path
}

fn contains(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn active_id(app: &CarbideApp) -> u64 {
    app.doc.as_ref().expect("a document is open").id
}

/// The document and file a pending save question is about.
fn question(app: &CarbideApp) -> Option<(u64, PathBuf)> {
    match &app.dialog {
        Some(Dialog::ConfirmRewrite { doc, path, .. }) => Some((*doc, path.clone())),
        _ => None,
    }
}

fn recovery_copies(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("AutoSave"))
        .map(|entries| entries.filter(|e| e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "asd"))).count())
        .unwrap_or(0)
}

#[test]
fn a_save_question_is_dropped_when_another_document_comes_forward() {
    let dir = scratch();
    let a = with_revision(&dir, "a.pdf", &["KEEP", "REMOVED-A"]);
    let b = plain(&dir, "b.pdf", &["OTHER DOCUMENT B"]);
    let before = std::fs::read(&a).unwrap();
    let mut app = app(&dir);

    // Every way another document can come forward drops the question about this one.
    type BringForward = fn(&mut CarbideApp, &Path);
    let ways: [(&str, BringForward); 3] = [
        ("new", |app, _| app.new_blank()),
        ("open", |app, other| app.open_path(other)),
        ("close", |app, _| app.close_tab(app.active_tab.unwrap())),
    ];
    for (how, bring_forward) in ways {
        app.open_path(&a);
        app.edit_removing("Delete page", |p| p.delete_pages(&[1]));
        app.save();
        let (doc, path) = question(&app).unwrap_or_else(|| panic!("{how}: saving has to ask"));
        assert_eq!((doc, path.as_path()), (active_id(&app), a.as_path()));
        bring_forward(&mut app, &b);
        assert!(question(&app).is_none(), "{how}: the question stayed up");

        // An answer that still arrives for it writes nothing.
        app.confirm_rewrite(doc, &a, AfterSave::Nothing);
        assert_eq!(std::fs::read(&a).unwrap(), before, "{how}: another document was written over a.pdf");
        assert!(!contains(&std::fs::read(&b).unwrap(), "KEEP"), "{how}");
        while let Some(index) = app.active_tab {
            app.close_tab(index);
        }
    }
}

#[test]
fn answering_the_save_question_writes_a_clean_file_and_cancelling_writes_nothing() {
    let dir = scratch();
    let a = with_revision(&dir, "a.pdf", &["KEEP", "REMOVED-A"]);
    let before = std::fs::read(&a).unwrap();
    let mut app = app(&dir);
    app.open_path(&a);
    app.edit_removing("Delete page", |p| p.delete_pages(&[1]));

    app.save();
    assert!(question(&app).is_some());
    app.dialog = None; // Cancel
    assert_eq!(std::fs::read(&a).unwrap(), before);
    assert!(app.doc.as_ref().unwrap().dirty);

    app.save();
    let (doc, path) = question(&app).unwrap();
    app.dialog = None;
    app.confirm_rewrite(doc, &path, AfterSave::Nothing);
    let written = std::fs::read(&a).unwrap();
    assert!(!contains(&written, "REMOVED-A"), "the deleted page is still in the file");
    let reopened = Pdf::from_bytes(&written).unwrap();
    assert_eq!((reopened.page_count(), reopened.revisions().len()), (1, 0));

    // What is shown is what is on disk now.
    let open = app.doc.as_ref().unwrap();
    assert!(!open.dirty && !open.content_removed);
    assert_eq!(open.saved.as_ref().unwrap().bytes(), written.as_slice());
    assert_eq!((open.pdf.page_count(), open.geometries.len()), (1, 1));
    assert!(app.notice.is_none());
    // Nothing more to ask about: the revisions are gone.
    app.save();
    assert!(question(&app).is_none());
}

#[test]
fn close_and_quit_finish_after_the_save_question() {
    let dir = scratch();
    let a = with_revision(&dir, "a.pdf", &["KEEP", "REMOVED-A"]);
    let mut app = app(&dir);

    // Close prompt, Save: the tab closes once the question is answered.
    app.open_path(&a);
    app.edit_removing("Delete page", |p| p.delete_pages(&[1]));
    app.save_then(AfterSave::CloseTab);
    let (doc, path) = question(&app).unwrap();
    app.dialog = None;
    app.confirm_rewrite(doc, &path, AfterSave::CloseTab);
    assert!(app.tabs.is_empty(), "the tab stayed open");
    assert!(!contains(&std::fs::read(&a).unwrap(), "REMOVED-A"));

    // Quit prompt, Save All: the document that does not ask is saved first, then the one that
    // does asks, and quitting goes on after the answer.
    let a = with_revision(&dir, "a2.pdf", &["KEEP", "REMOVED-A"]);
    let b = plain(&dir, "b.pdf", &["B"]);
    app.open_path(&a);
    app.edit_removing("Delete page", |p| p.delete_pages(&[1]));
    app.open_path(&b);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    let ctx = egui::Context::default();
    app.save_all_and_quit(&ctx);
    assert_eq!(Pdf::open(&b).unwrap().page_rotation(0).unwrap(), 90, "b.pdf was left unsaved");
    let (doc, path) = question(&app).expect("a2.pdf asks");
    assert_eq!(path, a);
    let then = match app.dialog.take() {
        Some(Dialog::ConfirmRewrite { then, .. }) => then,
        _ => unreachable!(),
    };
    app.confirm_rewrite(doc, &path, then);
    app.prompts(&ctx);
    assert!(app.tabs.is_empty(), "quitting did not go on");
    assert!(!contains(&std::fs::read(&a).unwrap(), "REMOVED-A"));
}

#[test]
fn ocr_results_only_replace_the_unchanged_document_they_came_from() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["PAGE ONE", "SECRET PAGE"]);
    let b = plain(&dir, "b.pdf", &["B"]);
    let mut app = app(&dir);
    let report = || OcrReport { recognized_pages: vec![0], skipped_pages: vec![], lines: 1 };
    let start = |app: &mut CarbideApp| {
        let doc = app.doc.as_ref().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.ocr_job = Some(OcrJob { doc: doc.id, version: doc.version, receiver, result: None });
        (sender, doc.pdf.clone())
    };

    // The page is deleted while OCR runs: the result must not bring it back.
    app.open_path(&a);
    let (sender, copy) = start(&mut app);
    app.edit_removing("Delete page", |p| p.delete_pages(&[1]));
    sender.send(Ok((copy, report()))).unwrap();
    app.poll_ocr();
    assert!(app.ocr_job.is_none());
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_count(), 1, "the deleted page came back");
    assert!(app.doc.as_ref().unwrap().content_removed);

    // Another document is in front when OCR finishes: the result waits for its own.
    let (sender, mut copy) = start(&mut app);
    copy.rotate_pages(&[0], 180).unwrap();
    let a_index = app.active_tab.unwrap();
    app.open_path(&b);
    sender.send(Ok((copy, report()))).unwrap();
    app.poll_ocr();
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_rotation(0).unwrap(), 0, "b.pdf got a.pdf's result");
    assert!(app.ocr_job.is_some());
    app.activate(Some(a_index));
    app.poll_ocr();
    assert!(app.ocr_job.is_none());
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_rotation(0).unwrap(), 180);
}

#[test]
fn a_time_stamped_signature_is_written_only_over_the_unchanged_document() {
    let dir = scratch();
    let mut app = app(&dir);
    let start = |app: &mut CarbideApp, path: &Path| {
        let doc = app.doc.as_ref().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let done = "Signed".to_string();
        app.sign_job = Some(SignJob { doc: doc.id, version: doc.version, path: path.to_path_buf(), done, receiver, result: None });
        sender
    };
    let signed = || document(&["STAMPED"]).to_bytes().unwrap();

    // The document is edited while the authority answers: its file is left alone.
    let a = plain(&dir, "a.pdf", &["ORIGINAL"]);
    app.open_path(&a);
    let sender = start(&mut app, &a);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    sender.send(Ok(signed())).unwrap();
    app.poll_sign();
    assert!(app.sign_job.is_none());
    assert!(!contains(&std::fs::read(&a).unwrap(), "STAMPED"), "an edited document's file was replaced");

    // Unchanged: the signed bytes replace the file, and the document is reloaded from them.
    let b = plain(&dir, "b.pdf", &["ORIGINAL"]);
    app.open_path(&b);
    let sender = start(&mut app, &b);
    app.poll_sign();
    assert!(app.sign_job.is_some(), "nothing has arrived yet");
    sender.send(Ok(signed())).unwrap();
    app.poll_sign();
    assert!(app.sign_job.is_none());
    assert!(contains(&std::fs::read(&b).unwrap(), "STAMPED"));
    assert_eq!(app.status, "Signed");
    assert!(!app.doc.as_ref().unwrap().dirty);
}

#[test]
fn protected_documents_get_no_recovery_copy_and_saving_retires_one() {
    let dir = scratch();
    let mut options = ProtectionOptions::new("", "owner pw");
    options.allow_copying = false;
    let protected = dir.join("protected.pdf");
    std::fs::write(&protected, document(&["PROTECTED"]).to_encrypted_bytes(&options).unwrap()).unwrap();
    let a = plain(&dir, "a.pdf", &["A"]);
    let mut app = app(&dir);
    let tick = |app: &mut CarbideApp| {
        app.last_autosave = Instant::now() - Duration::from_secs(24 * 3600);
        app.autosave_tick();
    };

    app.open_path(&protected);
    assert!(app.doc.as_ref().unwrap().saved.as_ref().unwrap().is_protected());
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    tick(&mut app);
    assert_eq!(recovery_copies(&dir), 0, "a protected document was copied unencrypted");
    assert!(app.save_revision("v1").is_err(), "revisions of protected documents are refused");

    app.open_path(&a);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    tick(&mut app);
    assert_eq!(recovery_copies(&dir), 1);
    // Saving a revision saves the document, so its recovery copy is no longer needed.
    app.save_revision("v1").unwrap();
    assert!(!app.doc.as_ref().unwrap().dirty);
    assert_eq!(recovery_copies(&dir), 0);
}

#[test]
fn a_revision_opens_from_a_private_copy_as_a_new_document() {
    let dir = scratch();
    let mut app = app(&dir);
    let copy = carbide_engine::fs::private_temp_file("a - First.pdf", &document(&["EARLIER"]).to_bytes().unwrap()).unwrap();
    app.open_copy(&copy);
    let doc = app.doc.as_ref().expect("the copy is open");
    assert_eq!(doc.title(), "a - First.pdf");
    // No file behind it: Save asks where to put it instead of writing into the temporary folder.
    assert!(doc.path.is_none() && doc.saved.is_none() && !doc.dirty);
    assert!(!copy.exists() && !copy.parent().unwrap().exists(), "the copy was left in the temporary folder");
}

/// `labels` protected with the open password `user` (none when empty) and the owner password
/// "owner pw", with copying not allowed.
fn protected(dir: &Path, name: &str, labels: &[&str], user: &str) -> PathBuf {
    let mut options = ProtectionOptions::new(user, "owner pw");
    options.allow_copying = false;
    let path = dir.join(name);
    std::fs::write(&path, document(labels).to_encrypted_bytes(&options).unwrap()).unwrap();
    path
}

fn file_key(bytes: &[u8], password: &str) -> Vec<u8> {
    let pdf = Pdf::from_bytes_with_password(bytes, password).unwrap();
    pdf.document().encryption_state.as_ref().expect("encrypted").file_encryption_key().to_vec()
}

#[test]
fn files_made_from_a_protected_document_keep_its_protection() {
    let dir = scratch();
    let source = protected(&dir, "locked.pdf", &["SECRET ONE", "SECRET TWO"], "open pw");
    let original = std::fs::read(&source).unwrap();
    let mut app = app(&dir);
    assert!(app.open_with_password(source.clone(), original.clone(), "open pw".into()));
    // Even after an edit is undone, which reloads the document from its history.
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.undo();

    let page = dir.join("page.pdf");
    app.extract_page_to(1, &page);
    let bytes = std::fs::read(&page).unwrap();
    assert!(matches!(Pdf::from_bytes(&bytes), Err(carbide_engine::Error::PasswordRequired)));
    assert_eq!(file_key(&bytes, "open pw"), file_key(&original, "owner pw"));
    assert!(!contains(&bytes, "SECRET"));

    // Exporting it unprotected asks first.
    app.export(Export::Text);
    assert!(matches!(app.dialog, Some(Dialog::ConfirmExport { export: Export::Text, .. })));
}

#[test]
fn merging_in_a_protected_file_asks_and_combining_keeps_a_shared_protection() {
    let dir = scratch();
    let plain_path = plain(&dir, "plain.pdf", &["PLAIN"]);
    let locked = protected(&dir, "locked.pdf", &["LOCKED"], "");
    let mut app = app(&dir);

    // Appending a protected file to an unprotected document would save its pages unprotected.
    app.open_path(&plain_path);
    app.merge_into_current(vec![Pdf::open(&locked).unwrap()], vec!["locked.pdf".into()], false);
    let Some(Dialog::ConfirmMerge { protected: names, keeps_protection, .. }) = &app.dialog else { panic!("merging did not ask") };
    assert_eq!((names.as_slice(), *keeps_protection), (["locked.pdf".to_string()].as_slice(), false));
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_count(), 1, "merged before the answer");
    app.dialog = None;
    app.merge_into_current(vec![Pdf::open(&locked).unwrap()], vec!["locked.pdf".into()], true);
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_count(), 2);
    assert!(!app.doc.as_ref().unwrap().is_protected());

    // Combining keeps the protection the files share, also when the new document is saved.
    app.combine(vec![Pdf::open(&plain_path).unwrap(), Pdf::open(&locked).unwrap()], false);
    assert!(app.doc.as_ref().unwrap().is_protected());
    let combined = dir.join("combined.pdf");
    app.write_to(&combined, None, AfterSave::Nothing);
    let bytes = std::fs::read(&combined).unwrap();
    assert!(!contains(&bytes, "LOCKED") && !contains(&bytes, "PLAIN"));
    assert_eq!(file_key(&bytes, "owner pw"), file_key(&std::fs::read(&locked).unwrap(), "owner pw"));

    // Files protected in different ways only combine without protection, when that is agreed to.
    let other = protected(&dir, "other.pdf", &["OTHER"], "");
    let tabs = app.tabs.len();
    app.combine(vec![Pdf::open(&locked).unwrap(), Pdf::open(&other).unwrap()], false);
    let Some(Dialog::ConfirmCombine { documents }) = app.dialog.take() else { panic!("combining did not ask") };
    assert_eq!(app.tabs.len(), tabs);
    app.combine(documents, true);
    assert_eq!(app.tabs.len(), tabs + 1);
    assert!(!app.doc.as_ref().unwrap().is_protected());
}
