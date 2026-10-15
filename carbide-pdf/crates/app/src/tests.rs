//! How the window's commands drive saving: the save question, what follows a save, background OCR
//! and Auto Recovery. Driven without a window; dialogs are answered the way their buttons do.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use carbide_engine::autosave::{AutoSaveSettings, AutoSaveStore, RowKind};
use carbide_engine::ocr::OcrReport;
use carbide_engine::{pdf_date_now, Pdf, ProtectionOptions};
use eframe::egui;

use crate::dialogs::{
    recovery_action, AfterSave, Dialog, Export, RecoveryAction, RecoveryQuestion,
};
use crate::ribbon::Cmd;
use crate::{CarbideApp, OcrJob, SignJob};

/// A folder of its own for one test.
fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "carbide-app-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
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
    std::fs::write(
        &path,
        working
            .save_revision(&base, "First", &pdf_date_now())
            .unwrap(),
    )
    .unwrap();
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
        .map(|entries| {
            entries
                .filter(|e| {
                    e.as_ref()
                        .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "asd"))
                })
                .count()
        })
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
        assert_eq!(
            std::fs::read(&a).unwrap(),
            before,
            "{how}: another document was written over a.pdf"
        );
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
    assert!(
        !contains(&written, "REMOVED-A"),
        "the deleted page is still in the file"
    );
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
    assert_eq!(
        Pdf::open(&b).unwrap().page_rotation(0).unwrap(),
        90,
        "b.pdf was left unsaved"
    );
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
    let report = || OcrReport {
        recognized_pages: vec![0],
        skipped_pages: vec![],
        lines: 1,
    };
    let start = |app: &mut CarbideApp| {
        let doc = app.doc.as_ref().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        app.ocr_job = Some(OcrJob {
            doc: doc.id,
            version: doc.version,
            receiver,
            result: None,
        });
        (sender, doc.pdf.clone())
    };

    // The page is deleted while OCR runs: the result must not bring it back.
    app.open_path(&a);
    let (sender, copy) = start(&mut app);
    app.edit_removing("Delete page", |p| p.delete_pages(&[1]));
    sender.send(Ok((copy, report()))).unwrap();
    app.poll_ocr();
    assert!(app.ocr_job.is_none());
    assert_eq!(
        app.doc.as_ref().unwrap().pdf.page_count(),
        1,
        "the deleted page came back"
    );
    assert!(app.doc.as_ref().unwrap().content_removed);

    // Another document is in front when OCR finishes: the result waits for its own.
    let (sender, mut copy) = start(&mut app);
    copy.rotate_pages(&[0], 180).unwrap();
    let a_index = app.active_tab.unwrap();
    app.open_path(&b);
    sender.send(Ok((copy, report()))).unwrap();
    app.poll_ocr();
    assert_eq!(
        app.doc.as_ref().unwrap().pdf.page_rotation(0).unwrap(),
        0,
        "b.pdf got a.pdf's result"
    );
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
        app.sign_job = Some(SignJob {
            doc: doc.id,
            version: doc.version,
            path: path.to_path_buf(),
            done,
            receiver,
            result: None,
        });
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
    assert!(
        !contains(&std::fs::read(&a).unwrap(), "STAMPED"),
        "an edited document's file was replaced"
    );

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
    std::fs::write(
        &protected,
        document(&["PROTECTED"])
            .to_encrypted_bytes(&options)
            .unwrap(),
    )
    .unwrap();
    let a = plain(&dir, "a.pdf", &["A"]);
    let mut app = app(&dir);
    let tick = |app: &mut CarbideApp| {
        overdue(app);
        app.autosave_tick();
    };

    app.open_path(&protected);
    assert!(app
        .doc
        .as_ref()
        .unwrap()
        .saved
        .as_ref()
        .unwrap()
        .is_protected());
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    tick(&mut app);
    assert_eq!(
        recovery_copies(&dir),
        0,
        "a protected document was copied unencrypted"
    );
    assert!(
        app.save_revision("v1").is_err(),
        "revisions of protected documents are refused"
    );

    app.open_path(&a);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    tick(&mut app);
    assert_eq!(recovery_copies(&dir), 1);
    // Saving a revision saves the document, so its recovery copy is no longer needed.
    app.save_revision("v1").unwrap();
    assert!(!app.doc.as_ref().unwrap().dirty);
    assert_eq!(recovery_copies(&dir), 0);
}

/// Makes every open document's Auto Recovery interval pass.
fn overdue(app: &mut CarbideApp) {
    let long_ago = Instant::now() - Duration::from_secs(24 * 3600);
    for doc in app.doc.iter_mut().chain(app.tabs.iter_mut().flatten()) {
        doc.autosaved_at = long_ago;
    }
}

#[test]
fn opening_a_document_never_delays_another_ones_recovery_copy() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["A"]);
    let mut app = app(&dir);
    app.open_path(&a);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    overdue(&mut app);
    // Opening other documents while a.pdf's interval passes leaves its own interval alone.
    for name in ["b.pdf", "c.pdf"] {
        app.open_path(&plain(&dir, name, &[name]));
    }
    app.autosave_tick();
    assert_eq!(recovery_copies(&dir), 1, "a.pdf got no recovery copy");
    // The new documents count from when they were opened.
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.autosave_tick();
    assert_eq!(
        recovery_copies(&dir),
        1,
        "c.pdf was copied before its interval passed"
    );
}

#[test]
fn a_revision_opens_from_a_private_copy_as_a_new_document() {
    let dir = scratch();
    let mut app = app(&dir);
    let copy = carbide_engine::fs::private_temp_file(
        "a - First.pdf",
        &document(&["EARLIER"]).to_bytes().unwrap(),
    )
    .unwrap();
    app.open_copy(&copy);
    let doc = app.doc.as_ref().expect("the copy is open");
    assert_eq!(doc.title(), "a - First.pdf");
    // No file behind it: Save asks where to put it instead of writing into the temporary folder.
    assert!(doc.path.is_none() && doc.saved.is_none() && !doc.dirty);
    assert!(
        !copy.exists() && !copy.parent().unwrap().exists(),
        "the copy was left in the temporary folder"
    );
}

/// `labels` protected with the open password `user` (none when empty) and the owner password
/// "owner pw", with copying not allowed.
fn protected(dir: &Path, name: &str, labels: &[&str], user: &str) -> PathBuf {
    let mut options = ProtectionOptions::new(user, "owner pw");
    options.allow_copying = false;
    let path = dir.join(name);
    std::fs::write(
        &path,
        document(labels).to_encrypted_bytes(&options).unwrap(),
    )
    .unwrap();
    path
}

fn file_key(bytes: &[u8], password: &str) -> Vec<u8> {
    let pdf = Pdf::from_bytes_with_password(bytes, password).unwrap();
    pdf.document()
        .encryption_state
        .as_ref()
        .expect("encrypted")
        .file_encryption_key()
        .to_vec()
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
    assert!(matches!(
        Pdf::from_bytes(&bytes),
        Err(carbide_engine::Error::PasswordRequired)
    ));
    assert_eq!(file_key(&bytes, "open pw"), file_key(&original, "owner pw"));
    assert!(!contains(&bytes, "SECRET"));

    // Exporting it unprotected asks first.
    app.export(Export::Text);
    assert!(matches!(
        app.dialog,
        Some(Dialog::ConfirmExport {
            export: Export::Text,
            ..
        })
    ));
}

#[test]
fn merging_in_a_protected_file_asks_and_combining_keeps_a_shared_protection() {
    let dir = scratch();
    let plain_path = plain(&dir, "plain.pdf", &["PLAIN"]);
    let locked = protected(&dir, "locked.pdf", &["LOCKED"], "");
    let mut app = app(&dir);

    // Appending a protected file to an unprotected document would save its pages unprotected.
    app.open_path(&plain_path);
    app.merge_into_current(
        vec![Pdf::open(&locked).unwrap()],
        vec!["locked.pdf".into()],
        false,
    );
    let Some(Dialog::ConfirmMerge {
        protected: names,
        keeps_protection,
        ..
    }) = &app.dialog
    else {
        panic!("merging did not ask")
    };
    assert_eq!(
        (names.as_slice(), *keeps_protection),
        (["locked.pdf".to_string()].as_slice(), false)
    );
    assert_eq!(
        app.doc.as_ref().unwrap().pdf.page_count(),
        1,
        "merged before the answer"
    );
    app.dialog = None;
    app.merge_into_current(
        vec![Pdf::open(&locked).unwrap()],
        vec!["locked.pdf".into()],
        true,
    );
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_count(), 2);
    assert!(!app.doc.as_ref().unwrap().is_protected());

    // Combining keeps the protection the files share, also when the new document is saved.
    app.combine(
        vec![Pdf::open(&plain_path).unwrap(), Pdf::open(&locked).unwrap()],
        false,
    );
    assert!(app.doc.as_ref().unwrap().is_protected());
    let combined = dir.join("combined.pdf");
    app.write_to(&combined, None, AfterSave::Nothing);
    let bytes = std::fs::read(&combined).unwrap();
    assert!(!contains(&bytes, "LOCKED") && !contains(&bytes, "PLAIN"));
    assert_eq!(
        file_key(&bytes, "owner pw"),
        file_key(&std::fs::read(&locked).unwrap(), "owner pw")
    );

    // Files protected in different ways only combine without protection, when that is agreed to.
    let other = protected(&dir, "other.pdf", &["OTHER"], "");
    let tabs = app.tabs.len();
    app.combine(
        vec![Pdf::open(&locked).unwrap(), Pdf::open(&other).unwrap()],
        false,
    );
    let Some(Dialog::ConfirmCombine { documents }) = app.dialog.take() else {
        panic!("combining did not ask")
    };
    assert_eq!(app.tabs.len(), tabs);
    app.combine(documents, true);
    assert_eq!(app.tabs.len(), tabs + 1);
    assert!(!app.doc.as_ref().unwrap().is_protected());
}

#[test]
fn hidden_information_is_removed_as_an_edit_and_offered_after_redacting() {
    let dir = scratch();
    let mut noted = document(&["PAGE TEXT"]);
    noted.add_note(0, 100.0, 100.0, "SECRET NOTE").unwrap();
    let path = dir.join("noted.pdf");
    std::fs::write(&path, noted.to_bytes().unwrap()).unwrap();
    let mut app = app(&dir);
    app.open_path(&path);

    app.run(Cmd::RemoveHidden);
    let Some(Dialog::RemoveHidden { doc, chosen }) = app.dialog.take() else {
        panic!("no dialog")
    };
    assert!(
        chosen
            .iter()
            .any(|(item, on)| *item == carbide_engine::HiddenItem::Bookmarks && !on),
        "bookmarks are kept unless chosen"
    );
    let items = chosen
        .iter()
        .filter(|(_, on)| *on)
        .map(|(item, _)| *item)
        .collect();
    app.remove_hidden_information(doc, items);
    let Some(Dialog::Message(report)) = &app.dialog else {
        panic!("nothing was reported")
    };
    // The note, and the text box `document` draws its label with.
    assert!(
        report.contains("Comments, text boxes and markup: 2"),
        "{report}"
    );
    let open = app.doc.as_ref().unwrap();
    assert!(open.dirty && open.content_removed);
    app.dialog = None;
    app.save();
    assert!(!contains(&std::fs::read(&path).unwrap(), "SECRET NOTE"));

    // Looking again finds nothing, and that is no edit: nothing to undo, and nothing for Save to
    // rewrite, which for a signed file would cost its signatures.
    let steps = app.doc.as_ref().unwrap().undo.len();
    app.remove_hidden_information(active_id(&app), carbide_engine::HiddenItem::ALL.to_vec());
    assert_eq!(app.status, "No hidden information was found.");
    let open = app.doc.as_ref().unwrap();
    assert!(!open.dirty && !open.content_removed && open.undo.len() == steps);
    assert_eq!(
        open.pdf
            .save_plan(open.saved.as_ref(), open.content_removed)
            .unwrap(),
        carbide_engine::SavePlan::Unchanged
    );

    // Applying redactions offers to look for the same text elsewhere.
    let Ok(renderer) = carbide_engine::Renderer::global() else {
        return;
    };
    app.renderer = Some(renderer);
    app.pending_redactions = vec![carbide_engine::Redaction {
        page: 0,
        rect: carbide_engine::Rect::new(60.0, 690.0, 200.0, 40.0),
    }];
    app.run(Cmd::ApplyRedactions);
    assert!(
        matches!(app.dialog, Some(Dialog::SuggestRemoveHidden { doc }) if doc == active_id(&app))
    );
}

#[test]
fn removing_hidden_information_also_rewrites_what_only_the_file_holds() {
    let dir = scratch();
    // A note taken off the page by an update appended later: the file still holds it.
    let mut noted = document(&["PAGE TEXT"]);
    noted
        .add_note(0, 100.0, 100.0, "SECRET-OLD-COMMENT")
        .unwrap();
    let base = noted.to_bytes().unwrap();
    let mut later = Pdf::from_bytes(&base).unwrap();
    let page = later.page_ids()[0];
    let annotations = later
        .document()
        .get_dictionary(page)
        .unwrap()
        .get(b"Annots")
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    // Only the text box `document` draws its label with stays.
    let kept: Vec<_> = annotations
        .into_iter()
        .filter(|a| {
            let id = a.as_reference().unwrap();
            later
                .document()
                .get_dictionary(id)
                .unwrap()
                .get(b"Subtype")
                .unwrap()
                .as_name()
                .unwrap()
                == b"FreeText"
        })
        .collect();
    later
        .document_mut()
        .get_dictionary_mut(page)
        .unwrap()
        .set("Annots", kept);
    let path = dir.join("history.pdf");
    std::fs::write(&path, later.incremental_update(&base).unwrap()).unwrap();
    assert!(contains(
        &std::fs::read(&path).unwrap(),
        "SECRET-OLD-COMMENT"
    ));

    let mut app = app(&dir);
    app.open_path(&path);
    app.run(Cmd::RemoveHidden);
    let Some(Dialog::RemoveHidden { doc, chosen }) = app.dialog.take() else {
        panic!("no dialog")
    };
    // Text boxes are kept, so that nothing but the earlier version is found.
    let items = chosen
        .iter()
        .filter(|(item, on)| *on && *item != carbide_engine::HiddenItem::Comments)
        .map(|(item, _)| *item)
        .collect();
    app.remove_hidden_information(doc, items);
    let Some(Dialog::Message(report)) = &app.dialog else {
        panic!("nothing was reported: {}", app.status)
    };
    assert!(
        report.contains(carbide_engine::sanitize::IN_FILE_LABEL),
        "{report}"
    );
    app.dialog = None;
    app.save();
    assert!(app.dialog.is_none(), "{:?}", app.status);
    let written = std::fs::read(&path).unwrap();
    assert!(
        !contains(&written, "SECRET-OLD-COMMENT"),
        "the earlier version is still in the file"
    );
    assert_eq!(
        Pdf::from_bytes(&written)
            .unwrap()
            .annotations(0)
            .unwrap()
            .len(),
        1,
        "the text box was kept"
    );
}

#[test]
fn split_refuses_ranges_that_would_write_one_part_twice() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["ONE", "TWO"]);
    let mut app = app(&dir);
    app.open_path(&a);
    let parts = dir.join("parts");
    std::fs::create_dir_all(&parts).unwrap();
    app.split_into(parts.clone(), "1,1", None);
    let Some(Dialog::Message(message)) = &app.dialog else {
        panic!("splitting did not refuse")
    };
    assert!(message.contains("a-pages-1-1.pdf twice"), "{message}");
    assert_eq!(
        std::fs::read_dir(&parts).unwrap().count(),
        0,
        "a part was written"
    );
}

/// Simulates an earlier session that auto-saved `edited` copies of `originals` (a document never
/// saved for `None`) and then ended without closing them, as a crash does.
fn crashed_session(dir: &Path, originals: &[Option<&Path>]) {
    let mut store = AutoSaveStore::open(dir.join("AutoSave")).unwrap();
    for (i, original) in originals.iter().enumerate() {
        let id = store.begin_document(*original, 1);
        let mut edited = match original {
            Some(path) => Pdf::open(path).unwrap(),
            None => document(&["NEW DOCUMENT"]),
        };
        edited.rotate_pages(&[0], 90).unwrap();
        store.autosave(id, &mut edited, 2 + i as i64).unwrap();
    }
}

#[test]
fn a_recovered_document_is_unsaved_and_replaces_its_original_only_when_agreed() {
    let dir = scratch();
    let original = plain(&dir, "contract.pdf", &["ORIGINAL"]);
    crashed_session(&dir, &[Some(&original)]);
    let mut app = app(&dir);
    assert!(app.autosave.as_ref().unwrap().any_needs_save());
    let rows = app.autosave.as_ref().unwrap().panel_rows();
    let recovered = rows
        .iter()
        .find(|r| r.kind == RowKind::Recovered)
        .unwrap()
        .record;

    let mut question = None;
    assert!(!recovery_action(
        &mut app,
        &mut question,
        RecoveryAction::Open(recovered, RowKind::Recovered)
    ));
    let doc = app.doc.as_ref().expect("the recovered copy is open");
    assert_eq!(doc.title(), "contract.pdf (recovered)");
    assert!(
        doc.path.is_none() && doc.dirty,
        "it is shown as unsaved and has no file of its own"
    );
    assert_eq!(
        doc.file_name(),
        "contract.pdf",
        "Save As suggests the original's name"
    );
    assert_eq!(doc.pdf.page_rotation(0).unwrap(), 90);
    assert!(!std::env::temp_dir()
        .join("contract.pdf (recovered).pdf")
        .exists());

    // Save asks before replacing the original, and Cancel writes nothing.
    app.save();
    let Some(Dialog::ReplaceOriginal { doc, path, .. }) = &app.dialog else {
        panic!("saving did not ask")
    };
    let (doc, path) = (*doc, path.clone());
    assert_eq!(path, original);
    app.dialog = None;
    assert_eq!(Pdf::open(&original).unwrap().page_rotation(0).unwrap(), 0);
    assert!(
        app.save_needs_question(),
        "Save All has to ask about it too"
    );

    // Saving never goes into the recovery folder.
    let inside = dir.join("AutoSave").join("contract.pdf");
    app.write_to(&inside, None, AfterSave::Nothing);
    assert!(!inside.exists());
    app.dialog = None;

    // Agreed: written over the original, after which it is that file again, and its recovery
    // copy is gone.
    app.replace_original(doc, &path, AfterSave::Nothing);
    assert_eq!(Pdf::open(&original).unwrap().page_rotation(0).unwrap(), 90);
    let open = app.doc.as_ref().unwrap();
    assert_eq!(
        (open.path.as_deref(), open.title(), open.dirty),
        (Some(original.as_path()), "contract.pdf".to_string(), false)
    );
    assert!(open.recovered.is_none());
    assert!(app.autosave.as_ref().unwrap().panel_rows().is_empty());
    assert_eq!(recovery_copies(&dir), 0);
}

#[test]
fn replacing_the_original_leaves_no_other_tab_showing_the_old_file() {
    let dir = scratch();
    let original = plain(&dir, "contract.pdf", &["ORIGINAL"]);
    crashed_session(&dir, &[Some(&original)]);
    let mut app = app(&dir);
    let rows = app.autosave.as_ref().unwrap().panel_rows();
    let record = rows[0].record;
    let mut question = None;
    let rotation_on_disk = || Pdf::open(&original).unwrap().page_rotation(0).unwrap();

    // Both rows open: the file as saved, with an unsaved edit, and the recovered copy.
    recovery_action(
        &mut app,
        &mut question,
        RecoveryAction::Open(record, RowKind::Original),
    );
    let saved_tab = app.active_tab.unwrap();
    app.edit("Note", |p| {
        p.add_note(0, 100.0, 100.0, "EDITED IN THE OTHER TAB")
    });
    recovery_action(
        &mut app,
        &mut question,
        RecoveryAction::Open(record, RowKind::Recovered),
    );
    let recovered = active_id(&app);

    // Replacing the file would lose that edit, or have it saved back over the recovered work later.
    app.save();
    let Some(Dialog::ReplaceOriginal { doc, path, .. }) = app.dialog.take() else {
        panic!("saving did not ask")
    };
    app.replace_original(doc, &path, AfterSave::Nothing);
    let Some(Dialog::Message(message)) = app.dialog.take() else {
        panic!("replacing went ahead")
    };
    assert!(message.contains("also open in another tab"), "{message}");
    assert_eq!(rotation_on_disk(), 0);
    assert!(
        app.doc.as_ref().unwrap().saved.is_none(),
        "the recovered document was tied to the file anyway"
    );

    // Once that tab is saved, replacing goes ahead, and the tab showing the old file goes.
    app.activate(Some(saved_tab));
    app.save();
    let recovered_tab = (0..app.tabs.len())
        .find(|&i| app.doc_at(i).is_some_and(|d| d.id == recovered))
        .unwrap();
    app.activate(Some(recovered_tab));
    app.save();
    let Some(Dialog::ReplaceOriginal { doc, path, .. }) = app.dialog.take() else {
        panic!("saving did not ask")
    };
    app.replace_original(doc, &path, AfterSave::Nothing);
    assert!(app.dialog.is_none(), "{}", app.status);
    assert_eq!(rotation_on_disk(), 90);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(
        app.doc.as_ref().unwrap().path.as_deref(),
        Some(original.as_path())
    );

    // Nothing else can write the old state back over it.
    app.open_path(&original);
    assert_eq!(app.tabs.len(), 1);
    app.edit("Note", |p| p.add_note(0, 100.0, 100.0, "LATER"));
    app.save();
    assert_eq!(rotation_on_disk(), 90);
}

#[test]
fn recovered_work_is_deleted_only_after_confirming() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["A"]);
    crashed_session(&dir, &[Some(&a), None]);
    let mut app = app(&dir);
    let rows = app.autosave.as_ref().unwrap().panel_rows();
    let of_a = rows
        .iter()
        .find(|r| r.kind == RowKind::Recovered && r.name.starts_with("a.pdf"))
        .unwrap()
        .record;
    assert_eq!(recovery_copies(&dir), 2);

    // Delete asks, with the name of the document, and Cancel keeps the copy.
    let mut question = None;
    assert!(recovery_action(
        &mut app,
        &mut question,
        RecoveryAction::Delete(of_a, RowKind::Recovered)
    ));
    assert_eq!(
        question.as_ref().map(RecoveryQuestion::message),
        Some("Delete the recovered copy of a.pdf?".to_string())
    );
    assert_eq!(recovery_copies(&dir), 2);
    recovery_action(&mut app, &mut question, RecoveryAction::Cancel);
    assert!(question.is_none());
    assert_eq!(recovery_copies(&dir), 2);
    recovery_action(
        &mut app,
        &mut question,
        RecoveryAction::Delete(of_a, RowKind::Recovered),
    );
    recovery_action(&mut app, &mut question, RecoveryAction::Confirm);
    assert_eq!(recovery_copies(&dir), 1);

    // Clear all asks too.
    recovery_action(&mut app, &mut question, RecoveryAction::ClearAll);
    assert_eq!(question, Some(RecoveryQuestion::ClearAll));
    assert_eq!(
        question.as_ref().unwrap().message(),
        carbide_engine::autosave::messages::DELETE_ALL
    );
    assert_eq!(recovery_copies(&dir), 1);
    recovery_action(&mut app, &mut question, RecoveryAction::Confirm);
    assert_eq!(recovery_copies(&dir), 0);
    assert!(app.autosave.as_ref().unwrap().panel_rows().is_empty());
    assert!(a.exists(), "the user's own file is never touched");
}

#[test]
fn undo_cannot_go_back_past_a_saved_revision_or_a_signature() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["A"]);
    let mut app = app(&dir);
    app.open_path(&a);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.save_revision("v1").unwrap();
    assert!(!app.cmd_enabled(Cmd::Undo) && !app.cmd_enabled(Cmd::Redo));
    app.undo();
    let doc = app.doc.as_ref().unwrap();
    assert_eq!(
        (
            doc.pdf.page_rotation(0).unwrap(),
            doc.pdf.revisions().len(),
            doc.dirty
        ),
        (90, 1, false)
    );

    // A signature written into the file: an edit and a save before it can no longer be undone.
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.save();
    assert!(app.cmd_enabled(Cmd::Undo), "a plain save keeps the history");
    let doc = app.doc.as_ref().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    app.sign_job = Some(SignJob {
        doc: doc.id,
        version: doc.version,
        path: a.clone(),
        done: "Signed".into(),
        receiver,
        result: None,
    });
    sender
        .send(Ok(document(&["SIGNED"]).to_bytes().unwrap()))
        .unwrap();
    app.poll_sign();
    assert_eq!(app.status, "Signed");
    assert!(!app.cmd_enabled(Cmd::Undo));
    app.undo();
    assert!(contains(&app.doc.as_ref().unwrap().bytes, "SIGNED"));
}

#[test]
fn signatures_of_a_document_opened_with_its_password_can_be_checked() {
    let dir = scratch();
    let path = protected(&dir, "locked.pdf", &["LOCKED"], "user pw");
    let mut app = app(&dir);
    app.open_path(&path);
    let Some(Dialog::Password { bytes, .. }) = app.dialog.take() else {
        panic!("the password is asked for")
    };
    assert!(app.open_with_password(path, bytes, "user pw".into()));
    app.validate_signatures();
    match &app.dialog {
        Some(Dialog::Message(text)) => assert_eq!(text, "This document has no signatures."),
        _ => panic!("no answer about the signatures"),
    }
}

#[test]
fn page_textures_never_exceed_what_the_gpu_takes() {
    use crate::{max_texture_side, texture_size};
    let limit = 16_777_216u64;
    let within = |size: [u32; 2], max_side: u32, what: &str| {
        assert!(size[0] >= 1 && size[1] >= 1, "{what}: {size:?}");
        assert!(
            size[0] <= max_side && size[1] <= max_side,
            "{what}: {size:?} past {max_side}"
        );
        assert!(
            size[0] as u64 * size[1] as u64 <= limit + size[0] as u64,
            "{what}: {size:?} too many pixels"
        );
    };
    // egui says nothing: the usual 8192.
    assert_eq!(max_texture_side(0), 8192);
    assert_eq!(max_texture_side(16384), 16384);

    // US Legal at the largest zoom on a Retina display: 5000 x 8235 before, past 8192.
    let legal = texture_size((612.0, 1008.0), 612.0 * 6.0 * 2.0, 8192);
    within(legal, 8192, "legal");
    assert!(
        (legal[1] as f32 / legal[0] as f32 - 1008.0 / 612.0).abs() < 0.01,
        "the shape is kept: {legal:?}"
    );
    // A receipt-like page at Fit Width, a strip, and a GPU that takes only 2048.
    within(texture_size((612.0, 9000.0), 1904.0, 8192), 8192, "tall");
    within(
        texture_size((612.0, 1_000_000.0), 2000.0, 8192),
        8192,
        "very tall",
    );
    within(
        texture_size((1_000_000.0, 10.0), 2000.0, 8192),
        8192,
        "very wide",
    );
    within(
        texture_size((612.0, 792.0), 5000.0, 2048),
        2048,
        "small GPU",
    );
    // Pages and zooms that are not numbers, or nothing at all, still give a texture.
    within(texture_size((0.0, 0.0), 0.0, 8192), 8192, "empty page");
    within(
        texture_size((f32::NAN, 792.0), f32::INFINITY, 8192),
        8192,
        "not numbers",
    );
    // An ordinary page at an ordinary zoom is drawn at full resolution.
    assert_eq!(texture_size((612.0, 792.0), 1224.0, 8192), [1224, 1584]);
    // Thumbnails keep their usual size.
    assert_eq!(texture_size((612.0, 792.0), 220.0, 8192), [220, 285]);
}

#[test]
fn dialogs_about_a_document_close_when_another_comes_forward() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["A"]);
    let b = plain(&dir, "b.pdf", &["B"]);
    let mut app = app(&dir);
    app.open_path(&a);
    let a_id = active_id(&app);
    app.open_path(&b);
    let (a_index, b_index) = (app.tab_of(a_id).unwrap(), app.active_tab.unwrap());
    app.activate(Some(a_index));

    let area = Some((0, carbide_engine::Rect::new(10.0, 10.0, 100.0, 50.0)));
    let dialogs = vec![
        Dialog::sign(a_id, area, false),
        Dialog::timestamp(a_id),
        Dialog::watermark(a_id),
        Dialog::bates(a_id),
        Dialog::protect(a_id),
        Dialog::impose(a_id),
        Dialog::reduce(Some(a_id), Some(a.clone())),
        Dialog::Split {
            doc: a_id,
            ranges: "1".into(),
        },
        Dialog::Note {
            doc: a_id,
            page: 0,
            x: 72.0,
            y: 72.0,
            on_page: false,
            text: "note".into(),
        },
        Dialog::SaveRevision {
            doc: a_id,
            label: String::new(),
            error: None,
        },
        Dialog::OpenRevision {
            doc: a_id,
            items: Vec::new(),
            selected: None,
        },
        Dialog::Signatures {
            doc: a_id,
            reports: Vec::new(),
            notice: String::new(),
        },
    ];
    for dialog in dialogs {
        assert_eq!(dialog.document(), Some(a_id));
        app.dialog = Some(dialog);
        app.activate(Some(b_index));
        assert!(
            app.dialog.is_none(),
            "a dialog about a.pdf stayed up over b.pdf"
        );
        app.activate(Some(a_index));
    }

    // A dialog still around for a document no longer in front goes without being shown.
    app.activate(Some(b_index));
    app.dialog = Some(Dialog::watermark(a_id));
    crate::dialogs::show(&mut app, &egui::Context::default());
    assert!(app.dialog.is_none());

    // A signature asked for on one document never signs the one in front.
    let before = std::fs::read(&b).unwrap();
    let Dialog::Sign(request) = Dialog::sign(a_id, area, false) else {
        unreachable!()
    };
    app.sign_document(*request);
    assert_eq!(std::fs::read(&b).unwrap(), before);
    assert!(
        app.status.starts_with("Nothing was signed"),
        "{}",
        app.status
    );
}

#[test]
fn the_close_question_stays_with_its_document_when_other_tabs_close() {
    use crate::shell::CloseChoice;
    let dir = scratch();
    let mut app = app(&dir);
    for (name, dirty) in [("clean.pdf", false), ("x.pdf", true), ("y.pdf", true)] {
        app.open_path(&plain(&dir, name, &[name]));
        if dirty {
            app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
        }
    }
    let ids: Vec<u64> = (0..3).map(|i| app.doc_at(i).unwrap().id).collect();
    app.request_close(2);
    assert_eq!(app.pending_close(), Some(ids[2]));
    // The clean tab to its left closes while the question is up, so y.pdf moves to tab 1.
    app.close_tab(0);
    app.answer_close(CloseChoice::Discard);
    let open: Vec<u64> = (0..app.tabs.len())
        .map(|i| app.doc_at(i).unwrap().id)
        .collect();
    assert_eq!(open, vec![ids[1]], "the wrong document was closed");
    assert!(app.doc_at(0).unwrap().dirty, "x.pdf lost its changes");

    // Once the document is closed some other way, an answer does nothing.
    app.request_close(0);
    app.close_tab(0);
    app.open_path(&plain(&dir, "z.pdf", &["Z"]));
    app.answer_close(CloseChoice::Discard);
    assert_eq!(app.tabs.len(), 1);
}

#[test]
fn a_password_question_is_never_replaced_by_what_comes_after_it() {
    let dir = scratch();
    let locked = protected(&dir, "locked.pdf", &["LOCKED"], "user pw");
    let original = plain(&dir, "contract.pdf", &["ORIGINAL"]);
    crashed_session(&dir, &[Some(&original)]);
    let mut app = app(&dir);

    // Started with a protected file while recovered work waits: the password is asked for first.
    app.launch(Some(crate::Launch::File(locked)));
    assert!(matches!(app.dialog, Some(Dialog::Password { .. })));
    // What background work says, and a dialog a command opens, wait behind it as well.
    app.alert("Recognizing text failed.".into());
    app.run(Cmd::AutoSaveSettings);
    assert!(
        matches!(app.dialog, Some(Dialog::Password { .. })),
        "the password question was replaced"
    );

    // Unlocking the file brings up the rest, one after another.
    let Some(Dialog::Password { path, bytes, .. }) = app.dialog.take() else {
        unreachable!()
    };
    assert!(app.open_with_password(path, bytes, "user pw".into()));
    app.next_dialog();
    assert!(matches!(app.dialog, Some(Dialog::AutoRecovery { .. })));
    app.dialog = None;
    app.next_dialog();
    assert!(
        matches!(&app.dialog, Some(Dialog::Message(text)) if text == "Recognizing text failed.")
    );
    app.dialog = None;
    app.next_dialog();
    assert!(matches!(app.dialog, Some(Dialog::Preferences { .. })));

    // A waiting question about a document goes when another document comes forward.
    let doc = active_id(&app);
    app.dialog = Some(Dialog::Password {
        path: dir.join("other.pdf"),
        bytes: Vec::new(),
        input: String::new(),
        failed: false,
    });
    app.queue_dialog(Dialog::ConfirmExport {
        doc,
        export: Export::Text,
    });
    app.new_blank();
    app.dialog = None;
    app.next_dialog();
    assert!(
        app.dialog.is_none(),
        "a question about a document no longer in front came back"
    );
}

#[test]
fn marked_redactions_follow_the_pages_they_were_drawn_on() {
    use carbide_engine::{PageGeometry, Rect, Redaction};
    let dir = scratch();
    let labels = ["P1", "P2", "P3", "P4", "P5", "P6"];
    let mut app = app(&dir);
    app.open_path(&plain(&dir, "six.pdf", &labels));
    let rect = Rect::new(100.0, 600.0, 200.0, 50.0);
    app.pending_redactions.push(Redaction { page: 4, rect });
    let marks = |app: &CarbideApp| {
        app.pending_redactions
            .iter()
            .map(|r| (r.page, r.rect))
            .collect::<Vec<_>>()
    };

    // Deleting a page before it, and inserting one, move the mark along.
    app.current_page = 1;
    app.run(Cmd::PageDelete);
    assert_eq!(marks(&app), vec![(3, rect)]);
    app.current_page = 0;
    app.run(Cmd::PageInsert);
    assert_eq!(marks(&app), vec![(4, rect)]);
    // Moving the marked page, or a page past it, keeps the mark on the same content.
    app.current_page = 4;
    app.run(Cmd::MoveDown);
    assert_eq!(marks(&app), vec![(5, rect)]);
    app.current_page = 4;
    app.run(Cmd::MoveDown);
    assert_eq!(marks(&app), vec![(4, rect)]);

    // Turning the page turns the mark with it: on a 612 x 792 page, a quarter turn clockwise
    // puts (x, y) at (y, 612 - x).
    app.current_page = 4;
    app.run(Cmd::RotateRight);
    assert_eq!(marks(&app), vec![(4, Rect::new(600.0, 312.0, 50.0, 200.0))]);
    // The same as the engine maps the page: each corner lands on a corner of the turned mark.
    let (upright, turned) = (
        PageGeometry::new(0.0, 0.0, 612.0, 792.0, 0),
        PageGeometry::new(0.0, 0.0, 612.0, 792.0, 90),
    );
    for (x, y) in [(100.0, 600.0), (300.0, 650.0)] {
        let (ux, uy) = upright.map_point(x, y);
        let (tx, ty) = turned.user_to_display(ux, uy);
        assert!(
            [600.0, 650.0].contains(&tx.round()) && [312.0, 512.0].contains(&ty.round()),
            "({x}, {y}) -> ({tx}, {ty})"
        );
    }
    app.run(Cmd::RotateLeft);
    assert_eq!(marks(&app), vec![(4, rect)]);

    // Deleting the marked page takes the mark away.
    app.run(Cmd::PageDelete);
    assert!(marks(&app).is_empty());

    // Without a renderer nothing is applied, and the marks are kept.
    app.pending_redactions.push(Redaction { page: 0, rect });
    app.run(Cmd::ApplyRedactions);
    assert_eq!(marks(&app), vec![(0, rect)]);
}

#[test]
fn commands_that_would_do_nothing_are_disabled_and_unchanged_documents_stay_unchanged() {
    let dir = scratch();
    let mut app = app(&dir);
    // Reduce from the Start screen works on a list of files: there is no document to reduce.
    app.run(Cmd::Reduce);
    assert!(matches!(&app.dialog, Some(Dialog::Reduce(form)) if form.lists_files()));
    app.dialog = None;

    app.open_path(&plain(&dir, "two.pdf", &["ONE", "TWO"]));
    app.current_page = 0;
    assert!(!app.cmd_enabled(Cmd::MoveUp) && app.cmd_enabled(Cmd::MoveDown));
    app.current_page = 1;
    assert!(app.cmd_enabled(Cmd::MoveUp) && !app.cmd_enabled(Cmd::MoveDown));
    // Without PDFium, the commands that need it are off.
    for cmd in [Cmd::Ocr, Cmd::Compare, Cmd::ExportImages, Cmd::ExportText] {
        assert!(!app.cmd_enabled(cmd), "{cmd:?}");
    }
    // An edit that changes nothing is no edit: no undo step and nothing to save.
    assert!(!app.edit("Move page", |p| p.move_page(1, 1)));
    let doc = app.doc.as_ref().unwrap();
    assert!(!doc.dirty && doc.undo.is_empty());
    assert!(!app.cmd_enabled(Cmd::Undo));

    app.open_path(&plain(&dir, "one.pdf", &["ONLY"]));
    assert!(
        !app.cmd_enabled(Cmd::PageDelete),
        "a document keeps at least one page"
    );
}

#[test]
fn quitting_from_outside_the_window_asks_about_unsaved_documents() {
    use crate::quit::may_terminate;
    // What applicationShouldTerminate: answers: go ahead unless something is unsaved and the
    // question has not been answered.
    assert!(may_terminate(false, false));
    assert!(!may_terminate(true, false));
    assert!(may_terminate(true, true));

    let ctx = egui::Context::default();
    let dir = scratch();
    let mut app = app(&dir);
    app.open_path(&plain(&dir, "a.pdf", &["A"]));
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.open_path(&plain(&dir, "b.pdf", &["B"]));
    assert!(app.has_unsaved_changes());
    // The question goes up, and nothing is closed or saved yet.
    app.on_system_quit(&ctx);
    assert!(app.quit_prompt_shown());
    assert_eq!(app.tabs.len(), 2);

    // With nothing unsaved the documents are let go and the window closes.
    let dir = scratch();
    let mut clean = crate::tests::app(&dir);
    clean.open_path(&plain(&dir, "c.pdf", &["C"]));
    clean.on_system_quit(&ctx);
    assert!(clean.tabs.is_empty() && !clean.quit_prompt_shown());
}

#[test]
fn ending_without_the_quit_question_keeps_unsaved_work_recoverable() {
    let dir = scratch();
    let mut app = app(&dir);
    app.open_path(&plain(&dir, "a.pdf", &["A"]));
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.open_path(&plain(&dir, "b.pdf", &["B"]));
    app.release_on_exit();
    assert!(app.tabs.is_empty() && app.doc.is_none());
    assert_eq!(
        recovery_copies(&dir),
        1,
        "the unsaved document has no recovery copy"
    );
}

#[test]
fn pages_of_any_size_are_laid_out_and_drawn_within_limits() {
    use crate::{
        layout_size, texture_size, thumbnail_size, thumbnails_fit, MAX_LAYOUT_SIDE,
        THUMBNAIL_CACHE_PIXELS, THUMBNAIL_WIDTH,
    };
    // Sizes a file can give, and some it cannot once it is read, are laid out as numbers.
    for page in [
        (f32::INFINITY, f32::INFINITY),
        (f32::NAN, 792.0),
        (0.0, 0.0),
        (-5.0, 792.0),
        (1.2e38, 792.0),
        (612.0, 14_400.0),
    ] {
        let (w, h) = layout_size(page);
        assert!(
            (1.0..=MAX_LAYOUT_SIDE).contains(&w) && (1.0..=MAX_LAYOUT_SIDE).contains(&h),
            "{page:?}: {w} x {h}"
        );
        let thumbnail = thumbnail_size(page);
        assert!(
            thumbnail.x.is_finite() && thumbnail.y.is_finite(),
            "{page:?}: {thumbnail:?}"
        );
        assert!(
            thumbnail.x <= THUMBNAIL_WIDTH && thumbnail.y <= 4.0 * THUMBNAIL_WIDTH,
            "{page:?}: {thumbnail:?}"
        );
        let [tw, th] = texture_size((w, h), thumbnail.x * 2.0, 8192);
        assert!(
            tw * th <= 4 * 1_000_000,
            "{page:?}: a thumbnail of {tw} x {th}"
        );
    }
    let letter = thumbnail_size((612.0, 792.0));
    assert!(
        letter.x == THUMBNAIL_WIDTH && (letter.y - THUMBNAIL_WIDTH * 792.0 / 612.0).abs() < 0.01,
        "{letter:?}"
    );
    // Thumbnails kept for redrawing hold no more than their share of memory.
    assert!(thumbnails_fit(std::iter::repeat_n([220, 285], 200)));
    assert!(!thumbnails_fit(std::iter::repeat_n(
        [220, 285],
        THUMBNAIL_CACHE_PIXELS / (220 * 285) + 1
    )));

    // A document of such pages, and one whose page draws itself, shown at every zoom.
    let Ok(renderer) = carbide_engine::Renderer::global() else {
        return;
    };
    let dir = scratch();
    let mut pdf = document(&["LETTER"]);
    pdf.insert_blank_page(0, Some((612.0, 792.0))).unwrap();
    pdf.insert_blank_page(2, Some((612.0, 14_400.0))).unwrap();
    pdf.insert_blank_page(3, Some((1_000_000.0, 1_000_000.0)))
        .unwrap();
    let page = pdf.page_ids()[0];
    let doc = pdf.document_mut();
    let form = doc.new_object_id();
    let dict = lopdf::dictionary! { "Subtype" => "Form", "Resources" => lopdf::dictionary! { "XObject" => lopdf::dictionary! { "X" => form } } };
    doc.objects.insert(
        form,
        lopdf::Object::Stream(lopdf::Stream::new(dict, b"/X Do /X Do".to_vec())),
    );
    let draw = doc.add_object(lopdf::Stream::new(
        lopdf::Dictionary::new(),
        b"/X Do".to_vec(),
    ));
    let page = doc.get_dictionary_mut(page).unwrap();
    page.set(
        "Resources",
        lopdf::dictionary! { "XObject" => lopdf::dictionary! { "X" => form } },
    );
    page.set("Contents", draw);
    let path = dir.join("sizes.pdf");
    std::fs::write(&path, pdf.to_bytes().unwrap()).unwrap();
    let mut app = app(&dir);
    app.renderer = Some(renderer);
    app.open_path(&path);
    let ctx = egui::Context::default();
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 800.0),
        )),
        max_texture_side: Some(4096),
        ..Default::default()
    };
    for zoom in [
        None,
        Some(crate::shell::MIN_ZOOM),
        Some(crate::shell::MAX_ZOOM),
    ] {
        if let Some(zoom) = zoom {
            app.zoom = zoom;
            app.shell.fit = crate::shell::Fit::Manual;
        }
        for page in 0..4 {
            app.current_page = page;
            app.scroll_to = Some(page);
            // Laying out and scrolling take a few frames.
            for _ in 0..3 {
                ctx.run_ui(input(), |ui| app.draw_shell(ui))
                    .drop_without_applying_deltas();
            }
        }
    }
    let largest = app
        .textures
        .values()
        .chain(app.thumbs.values())
        .map(|t| t.size())
        .max_by_key(|[w, h]| w * h)
        .unwrap();
    assert!(
        largest[0] <= 4096 && largest[1] <= 4096 && largest[0] * largest[1] <= 16 << 20,
        "{largest:?}"
    );
    // The page that draws itself is not rendered: PDFium would never finish it. The others are.
    let version = app.doc.as_ref().unwrap().version;
    assert_eq!(app.drawable.get(&(version, 0)), Some(&false));
    assert_eq!(app.drawable.get(&(version, 1)), Some(&true));
    let pages: Vec<usize> = app
        .thumbs
        .keys()
        .map(|&(_, page)| page)
        .chain(app.textures.keys().map(|&(_, page, _)| page))
        .collect();
    assert!(!pages.contains(&0) && pages.contains(&1), "{pages:?}");
}

#[test]
fn notes_and_signatures_waiting_in_a_dialog_follow_the_pages() {
    use carbide_engine::Rect;
    let dir = scratch();
    let mut app = app(&dir);
    app.open_path(&plain(&dir, "five.pdf", &["P1", "P2", "P3", "P4", "P5"]));
    let doc = active_id(&app);
    let note = |app: &CarbideApp| match &app.dialog {
        Some(Dialog::Note { page, x, y, .. }) => Some((*page, *x, *y)),
        _ => None,
    };
    // A note being written for the fourth page, while the first is deleted from the ribbon.
    app.dialog = Some(Dialog::Note {
        doc,
        page: 3,
        x: 100.0,
        y: 600.0,
        on_page: false,
        text: "for P4".into(),
    });
    app.current_page = 0;
    app.run(Cmd::PageDelete);
    assert_eq!(
        note(&app),
        Some((2, 100.0, 600.0)),
        "the note would go on P5"
    );
    // Turned with its page, it stays on the same spot of the page.
    app.current_page = 2;
    app.run(Cmd::RotateRight);
    assert_eq!(note(&app), Some((2, 600.0, 512.0)));
    // Moving another page past it moves it along.
    app.current_page = 1;
    app.run(Cmd::MoveDown);
    assert_eq!(note(&app), Some((1, 600.0, 512.0)));
    // Deleting its page closes it.
    app.current_page = 1;
    app.run(Cmd::PageDelete);
    assert_eq!(note(&app), None);
    assert!(app.dialog.is_none());

    // A visible signature waiting behind another dialog moves too, and goes with its page.
    let area = Rect::new(10.0, 10.0, 100.0, 50.0);
    app.dialog = Some(Dialog::Message("first".into()));
    app.queue_dialog(Dialog::sign(doc, Some((2, area)), false));
    app.current_page = 0;
    app.run(Cmd::PageInsert);
    app.dialog = None;
    app.next_dialog();
    assert!(
        matches!(&app.dialog, Some(Dialog::Sign(request)) if request.appearance == Some((3, area)))
    );
    app.current_page = 3;
    app.run(Cmd::PageDelete);
    assert!(app.dialog.is_none());

    // Undo changes the pages in ways that cannot be followed: such dialogs close.
    app.dialog = Some(Dialog::Note {
        doc,
        page: 0,
        x: 72.0,
        y: 72.0,
        on_page: true,
        text: String::new(),
    });
    app.undo();
    assert!(app.dialog.is_none());
    app.dialog = Some(Dialog::watermark(doc));
    app.redo();
    assert!(
        matches!(app.dialog, Some(Dialog::Watermark { .. })),
        "a dialog that places nothing stays"
    );
}

#[test]
fn commands_that_could_only_refuse_are_disabled() {
    let dir = scratch();
    let mut app = app(&dir);
    app.open_path(&plain(&dir, "a.pdf", &["A"]));
    // Zooming stops at the ends of its range.
    app.zoom = crate::shell::MAX_ZOOM;
    assert!(!app.cmd_enabled(Cmd::ZoomIn) && app.cmd_enabled(Cmd::ZoomOut));
    app.zoom = crate::shell::MIN_ZOOM;
    assert!(app.cmd_enabled(Cmd::ZoomIn) && !app.cmd_enabled(Cmd::ZoomOut));
    // While a signature waits for its time stamp, no other signature can be written first.
    for cmd in [
        Cmd::SignVisible,
        Cmd::SignInvisible,
        Cmd::Certify,
        Cmd::TimeStamp,
        Cmd::SaveRevision,
    ] {
        assert!(app.cmd_enabled(cmd), "{cmd:?}");
    }
    let doc = app.doc.as_ref().unwrap();
    let (_sender, receiver) = std::sync::mpsc::channel();
    let path = dir.join("a.pdf");
    app.sign_job = Some(SignJob {
        doc: doc.id,
        version: doc.version,
        path,
        done: String::new(),
        receiver,
        result: None,
    });
    for cmd in [
        Cmd::SignVisible,
        Cmd::SignInvisible,
        Cmd::Certify,
        Cmd::TimeStamp,
    ] {
        assert!(!app.cmd_enabled(cmd), "{cmd:?}");
    }
    app.sign_job = None;

    // Password-protected documents are neither signed nor given saved revisions.
    let locked = protected(&dir, "locked.pdf", &["LOCKED"], "open pw");
    let bytes = std::fs::read(&locked).unwrap();
    assert!(app.open_with_password(locked, bytes, "open pw".into()));
    for cmd in [
        Cmd::SignVisible,
        Cmd::SignInvisible,
        Cmd::Certify,
        Cmd::TimeStamp,
        Cmd::SaveRevision,
    ] {
        assert!(!app.cmd_enabled(cmd), "{cmd:?}");
    }
}

#[test]
fn save_all_carries_on_with_another_dialog_on_screen() {
    let dir = scratch();
    let a = with_revision(&dir, "a.pdf", &["KEEP", "REMOVED-A"]);
    let b = plain(&dir, "b.pdf", &["B"]);
    let mut app = app(&dir);
    app.open_path(&a);
    app.edit_removing("Delete page", |p| p.delete_pages(&[1]));
    app.open_path(&b);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    let ctx = egui::Context::default();
    app.on_system_quit(&ctx);
    app.run(Cmd::AutoSaveSettings);
    assert!(matches!(app.dialog, Some(Dialog::Preferences { .. })));
    app.save_all_and_quit(&ctx);
    assert_eq!(
        Pdf::open(&b).unwrap().page_rotation(0).unwrap(),
        90,
        "b.pdf was left unsaved"
    );
    let (_, path) = question(&app).expect("a.pdf asks");
    assert_eq!(path, a);
}

/// `labels` written to `name` with a signature field whose signature does not match the file: any
/// document with a signature has a notice, and this one needs no signing key.
fn with_signature(dir: &Path, name: &str, labels: &[&str]) -> PathBuf {
    use lopdf::{dictionary, Object, StringFormat};
    let mut pdf = document(labels);
    let page = pdf.page_id(0).unwrap();
    let doc = pdf.document_mut();
    let value = doc.add_object(dictionary! {
        "Type" => "Sig", "Filter" => "Adobe.PPKLite", "SubFilter" => "adbe.pkcs7.detached",
        "ByteRange" => vec![0.into(), 10.into(), 20.into(), 30.into()],
        "Contents" => Object::String(vec![0; 16], StringFormat::Hexadecimal),
    });
    let field = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "FT" => "Sig", "T" => Object::string_literal("Signature1"),
        "V" => value, "P" => page, "Rect" => vec![0.into(), 0.into(), 0.into(), 0.into()],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![Object::Reference(field)]);
    doc.catalog_mut().unwrap().set(
        "AcroForm",
        dictionary! { "Fields" => vec![Object::Reference(field)] },
    );
    let path = dir.join(name);
    std::fs::write(&path, pdf.to_bytes().unwrap()).unwrap();
    path
}

#[test]
fn signature_results_say_when_unsaved_changes_are_not_covered() {
    let dir = scratch();
    let signed = with_signature(&dir, "signed.pdf", &["SIGNED"]);
    let other = plain(&dir, "other.pdf", &["OTHER"]);
    let mut app = app(&dir);
    app.open_path(&signed);
    let summary = app.notice.clone().expect("a signed document has a notice");
    assert_eq!(app.shown_notice(), Some(summary.clone()));
    let unsaved = format!("{summary} {}", crate::UNSAVED_CHANGES_NOT_SIGNED);

    // Every way the shown document comes to differ from its file says so.
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    assert_eq!(app.shown_notice().as_ref(), Some(&unsaved), "after an edit");
    app.undo();
    assert_eq!(app.shown_notice().as_ref(), Some(&unsaved), "after undo");
    app.redo();
    assert_eq!(app.shown_notice().as_ref(), Some(&unsaved), "after redo");
    let (sender, receiver) = std::sync::mpsc::channel();
    let doc = app.doc.as_ref().unwrap();
    app.ocr_job = Some(OcrJob {
        doc: doc.id,
        version: doc.version,
        receiver,
        result: None,
    });
    let mut recognized = doc.pdf.clone();
    recognized
        .add_text_box(0, 72.0, 600.0, "RECOGNIZED", 12.0)
        .unwrap();
    sender.send(Ok((recognized, OcrReport::default()))).unwrap();
    app.poll_ocr();
    assert!(contains(&app.doc.as_ref().unwrap().bytes, "RECOGNIZED"));
    assert_eq!(app.shown_notice().as_ref(), Some(&unsaved), "after OCR");

    // So do the results of checking the signatures.
    app.validate_signatures();
    match app.dialog.take() {
        Some(Dialog::Signatures { notice, .. }) => {
            assert_eq!(app.about_saved_file(&notice), unsaved)
        }
        _ => panic!("the signatures were not shown"),
    }

    // Another document in front has its own notice (none); coming back brings this one back.
    app.open_path(&other);
    assert_eq!(app.shown_notice(), None);
    app.activate(Some(0));
    assert_eq!(
        app.shown_notice().as_ref(),
        Some(&unsaved),
        "after switching tabs"
    );

    // Saved, the notice is about what is shown again.
    app.save();
    assert!(question(&app).is_none() && !app.doc.as_ref().unwrap().dirty);
    let saved = app.shown_notice().expect("still signed");
    assert!(
        !saved.contains(crate::UNSAVED_CHANGES_NOT_SIGNED),
        "{saved}"
    );
    assert_eq!(Some(saved), app.notice);
}

#[test]
fn protecting_the_open_file_keeps_it_protected_from_then_on() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["PRIVATE WORDS"]);
    let mut app = app(&dir);
    app.open_path(&a);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    let options = ProtectionOptions::new("open pw", "owner pw");
    app.protect_to(&a, &options);
    let written = std::fs::read(&a).unwrap();
    assert!(matches!(
        Pdf::from_bytes(&written),
        Err(carbide_engine::Error::PasswordRequired)
    ));
    let key = file_key(&written, "owner pw");
    // The document is that file now, the edit made before included.
    let doc = app.doc.as_ref().unwrap();
    assert!(doc.is_protected() && !doc.dirty);
    assert_eq!(doc.saved.as_ref().unwrap().bytes(), written.as_slice());
    assert_eq!(doc.pdf.page_rotation(0).unwrap(), 90);

    // Later edits get no unencrypted recovery copy, and Save and Save As keep the protection.
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    overdue(&mut app);
    app.autosave_tick();
    assert_eq!(
        recovery_copies(&dir),
        0,
        "an unencrypted recovery copy was written"
    );
    app.save();
    let saved = std::fs::read(&a).unwrap();
    assert!(
        matches!(
            Pdf::from_bytes(&saved),
            Err(carbide_engine::Error::PasswordRequired)
        ),
        "saved without protection"
    );
    assert_eq!(file_key(&saved, "open pw"), key);
    assert_eq!(
        Pdf::from_bytes_with_password(&saved, "open pw")
            .unwrap()
            .page_rotation(0)
            .unwrap(),
        180
    );
    let b = dir.join("b.pdf");
    app.write_to(&b, None, AfterSave::Nothing);
    assert_eq!(file_key(&std::fs::read(&b).unwrap(), "open pw"), key);

    // A file open in another tab is only written over when that tab has nothing unsaved, and the
    // tab then closes rather than go on showing the old file.
    let c = plain(&dir, "c.pdf", &["C"]);
    let c_before = std::fs::read(&c).unwrap();
    app.open_path(&c);
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.activate(Some(0));
    app.protect_to(&c, &options);
    assert_eq!(
        std::fs::read(&c).unwrap(),
        c_before,
        "a file with unsaved changes in another tab was replaced"
    );
    app.dialog = None;
    app.activate(Some(1));
    app.save();
    app.activate(Some(0));
    let tabs = app.tabs.len();
    app.protect_to(&c, &options);
    assert!(matches!(
        Pdf::from_bytes(&std::fs::read(&c).unwrap()),
        Err(carbide_engine::Error::PasswordRequired)
    ));
    assert_eq!(app.tabs.len(), tabs - 1);

    // Nothing goes into the recovery folder.
    let private = dir.join("AutoSave").join("copy.pdf");
    app.protect_to(&private, &options);
    assert!(!private.exists());

    // A protected document protected again over its own file goes on with the new passwords.
    let locked = protected(&dir, "locked.pdf", &["LOCKED"], "old pw");
    assert!(app.open_with_password(
        locked.clone(),
        std::fs::read(&locked).unwrap(),
        "old pw".into()
    ));
    app.protect_to(&locked, &ProtectionOptions::new("new pw", "new owner"));
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.save();
    let saved = std::fs::read(&locked).unwrap();
    assert!(
        Pdf::from_bytes_with_password(&saved, "old pw").is_err(),
        "the old password still opens it"
    );
    assert_eq!(
        Pdf::from_bytes_with_password(&saved, "new pw")
            .unwrap()
            .page_rotation(0)
            .unwrap(),
        90
    );
}

/// The heading the Signatures dialog on screen shows, if it is on screen.
fn signatures_heading(app: &CarbideApp) -> Option<String> {
    match &app.dialog {
        Some(Dialog::Signatures { notice, .. }) => Some(app.about_saved_file(notice)),
        _ => None,
    }
}

#[test]
fn signature_results_on_screen_follow_edits_and_close_when_the_file_is_written() {
    let dir = scratch();
    let signed = with_signature(&dir, "signed.pdf", &["SIGNED"]);
    let mut app = app(&dir);
    app.open_path(&signed);

    // Checked with nothing unsaved, then edited with the results still up: they say the edit is
    // not covered.
    app.validate_signatures();
    let summary = signatures_heading(&app).expect("the signatures are shown");
    assert!(
        !summary.contains(crate::UNSAVED_CHANGES_NOT_SIGNED),
        "{summary}"
    );
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    assert_eq!(
        signatures_heading(&app),
        Some(format!("{summary} {}", crate::UNSAVED_CHANGES_NOT_SIGNED))
    );

    // Saving writes a new file, which the results are not about: they close, waiting ones too.
    app.validate_signatures();
    app.queued_dialogs.push_back(Dialog::Signatures {
        doc: active_id(&app),
        reports: Vec::new(),
        notice: summary,
    });
    app.save();
    assert!(!app.doc.as_ref().unwrap().dirty);
    assert!(signatures_heading(&app).is_none());
    assert!(!app
        .queued_dialogs
        .iter()
        .any(|d| matches!(d, Dialog::Signatures { .. })));
}

#[test]
fn files_made_from_a_document_never_replace_an_open_one() {
    let dir = scratch();
    let a = plain(&dir, "a.pdf", &["ONE", "TWO", "THREE"]);
    let b = plain(&dir, "b.pdf", &["B"]);
    let (a_before, b_before) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    let mut app = app(&dir);
    app.open_path(&b);
    app.open_path(&a);

    // Its own file, and that of another tab, are refused; the documents stay what is on disk.
    for target in [&a, &b] {
        app.extract_page_to(0, target);
        assert!(
            matches!(app.dialog.take(), Some(Dialog::Message(m)) if m.contains("is open")),
            "{}",
            target.display()
        );
        let options = carbide_engine::imposition::ImpositionOptions::default();
        assert!(
            app.impose_to(&options, target)
                .is_err_and(|m| m.contains("is open")),
            "{}",
            target.display()
        );
    }
    assert_eq!(std::fs::read(&a).unwrap(), a_before);
    assert_eq!(std::fs::read(&b).unwrap(), b_before);
    assert_eq!(app.doc.as_ref().unwrap().pdf.page_count(), 3);

    // Split parts that would replace an open file are refused too.
    let part = dir.join("a-pages-1-1.pdf");
    std::fs::write(&part, &b_before).unwrap();
    app.open_path(&part);
    app.activate(Some(1));
    app.split_into(
        dir.clone(),
        "1",
        Some(carbide_engine::fs::Existing::Replace),
    );
    assert!(matches!(app.dialog.take(), Some(Dialog::Message(m)) if m.contains("is open")));
    assert_eq!(std::fs::read(&part).unwrap(), b_before);

    // Anywhere else is written.
    let c = dir.join("c.pdf");
    app.extract_page_to(1, &c);
    assert_eq!(Pdf::open(&c).unwrap().page_count(), 1);
}

#[test]
fn finding_no_hidden_information_keeps_the_undone_steps_to_redo() {
    let dir = scratch();
    let path = plain(&dir, "a.pdf", &["A"]);
    let mut app = app(&dir);
    app.open_path(&path);
    assert!(app.edit("Rotate", |pdf| pdf.rotate_pages(&[0], 90)));
    app.run(Cmd::Undo);
    assert_eq!(app.doc.as_ref().unwrap().redo.len(), 1);

    // Nothing to find: the document, its dirty state and the step to redo stay as they were.
    let before = app.doc.as_ref().unwrap().bytes.clone();
    let dirty = app.doc.as_ref().unwrap().dirty;
    app.remove_hidden_information(
        active_id(&app),
        vec![carbide_engine::HiddenItem::Thumbnails],
    );
    assert_eq!(app.status, "No hidden information was found.");
    let open = app.doc.as_ref().unwrap();
    assert_eq!(open.redo.len(), 1, "the step to redo was lost");
    assert!(open.bytes == before && open.dirty == dirty);

    app.run(Cmd::Redo);
    assert_eq!(app.doc.as_ref().unwrap().geometries[0].rotation, 90);
}

#[test]
fn a_signature_is_not_written_over_a_file_another_program_changed() {
    let dir = scratch();
    let mut app = app(&dir);
    let path = plain(&dir, "a.pdf", &["ORIGINAL"]);
    app.open_path(&path);
    let doc = app.doc.as_ref().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    app.sign_job = Some(SignJob {
        doc: doc.id,
        version: doc.version,
        path: path.clone(),
        done: "Signed".into(),
        receiver,
        result: None,
    });

    // The file is replaced while the authority answers: it stays as the other program left it.
    let other = document(&["OTHER PROGRAM"]).to_bytes().unwrap();
    std::fs::write(&path, &other).unwrap();
    sender
        .send(Ok(document(&["STAMPED"]).to_bytes().unwrap()))
        .unwrap();
    app.poll_sign();
    assert!(app.sign_job.is_none());
    assert!(
        std::fs::read(&path).unwrap() == other,
        "a file another program changed was replaced"
    );
    assert!(
        matches!(&app.dialog, Some(Dialog::Message(m)) if m.contains("changed by another program"))
    );
}
