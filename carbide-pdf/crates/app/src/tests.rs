//! How the window's commands drive saving: the save question, what follows a save, background OCR
//! and Auto Recovery. Driven without a window; dialogs are answered the way their buttons do.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use carbide_engine::autosave::{AutoSaveSettings, AutoSaveStore, RowKind};
use carbide_engine::ocr::OcrReport;
use carbide_engine::{pdf_date_now, Pdf, ProtectionOptions};
use eframe::egui;

use crate::dialogs::{recovery_action, AfterSave, Dialog, Export, RecoveryAction, RecoveryQuestion};
use crate::ribbon::Cmd;
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
    let Some(Dialog::RemoveHidden { doc, chosen }) = app.dialog.take() else { panic!("no dialog") };
    assert!(chosen.iter().any(|(item, on)| *item == carbide_engine::HiddenItem::Bookmarks && !on), "bookmarks are kept unless chosen");
    let items = chosen.iter().filter(|(_, on)| *on).map(|(item, _)| *item).collect();
    app.remove_hidden_information(doc, items);
    let Some(Dialog::Message(report)) = &app.dialog else { panic!("nothing was reported") };
    // The note, and the text box `document` draws its label with.
    assert!(report.contains("Comments, text boxes and markup: 2"), "{report}");
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
    assert_eq!(open.pdf.save_plan(open.saved.as_ref(), open.content_removed).unwrap(), carbide_engine::SavePlan::Unchanged);

    // Applying redactions offers to look for the same text elsewhere.
    let Ok(renderer) = carbide_engine::Renderer::global() else { return };
    app.renderer = Some(renderer);
    app.pending_redactions = vec![carbide_engine::Redaction { page: 0, rect: carbide_engine::Rect::new(60.0, 690.0, 200.0, 40.0) }];
    app.run(Cmd::ApplyRedactions);
    assert!(matches!(app.dialog, Some(Dialog::SuggestRemoveHidden { doc }) if doc == active_id(&app)));
}

#[test]
fn removing_hidden_information_also_rewrites_what_only_the_file_holds() {
    let dir = scratch();
    // A note taken off the page by an update appended later: the file still holds it.
    let mut noted = document(&["PAGE TEXT"]);
    noted.add_note(0, 100.0, 100.0, "SECRET-OLD-COMMENT").unwrap();
    let base = noted.to_bytes().unwrap();
    let mut later = Pdf::from_bytes(&base).unwrap();
    let page = later.page_ids()[0];
    let annotations = later.document().get_dictionary(page).unwrap().get(b"Annots").unwrap().as_array().unwrap().clone();
    // Only the text box `document` draws its label with stays.
    let kept: Vec<_> = annotations
        .into_iter()
        .filter(|a| {
            let id = a.as_reference().unwrap();
            later.document().get_dictionary(id).unwrap().get(b"Subtype").unwrap().as_name().unwrap() == b"FreeText"
        })
        .collect();
    later.document_mut().get_dictionary_mut(page).unwrap().set("Annots", kept);
    let path = dir.join("history.pdf");
    std::fs::write(&path, later.incremental_update(&base).unwrap()).unwrap();
    assert!(contains(&std::fs::read(&path).unwrap(), "SECRET-OLD-COMMENT"));

    let mut app = app(&dir);
    app.open_path(&path);
    app.run(Cmd::RemoveHidden);
    let Some(Dialog::RemoveHidden { doc, chosen }) = app.dialog.take() else { panic!("no dialog") };
    // Text boxes are kept, so that nothing but the earlier version is found.
    let items = chosen.iter().filter(|(item, on)| *on && *item != carbide_engine::HiddenItem::Comments).map(|(item, _)| *item).collect();
    app.remove_hidden_information(doc, items);
    let Some(Dialog::Message(report)) = &app.dialog else { panic!("nothing was reported: {}", app.status) };
    assert!(report.contains(carbide_engine::sanitize::IN_FILE_LABEL), "{report}");
    app.dialog = None;
    app.save();
    assert!(app.dialog.is_none(), "{:?}", app.status);
    let written = std::fs::read(&path).unwrap();
    assert!(!contains(&written, "SECRET-OLD-COMMENT"), "the earlier version is still in the file");
    assert_eq!(Pdf::from_bytes(&written).unwrap().annotations(0).unwrap().len(), 1, "the text box was kept");
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
    let recovered = rows.iter().find(|r| r.kind == RowKind::Recovered).unwrap().record;

    let mut question = None;
    assert!(!recovery_action(&mut app, &mut question, RecoveryAction::Open(recovered, RowKind::Recovered)));
    let doc = app.doc.as_ref().expect("the recovered copy is open");
    assert_eq!(doc.title(), "contract.pdf (recovered)");
    assert!(doc.path.is_none() && doc.dirty, "it is shown as unsaved and has no file of its own");
    assert_eq!(doc.file_name(), "contract.pdf", "Save As suggests the original's name");
    assert_eq!(doc.pdf.page_rotation(0).unwrap(), 90);
    assert!(!std::env::temp_dir().join("contract.pdf (recovered).pdf").exists());

    // Save asks before replacing the original, and Cancel writes nothing.
    app.save();
    let Some(Dialog::ReplaceOriginal { doc, path, .. }) = &app.dialog else { panic!("saving did not ask") };
    let (doc, path) = (*doc, path.clone());
    assert_eq!(path, original);
    app.dialog = None;
    assert_eq!(Pdf::open(&original).unwrap().page_rotation(0).unwrap(), 0);
    assert!(app.save_needs_question(), "Save All has to ask about it too");

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
    assert_eq!((open.path.as_deref(), open.title(), open.dirty), (Some(original.as_path()), "contract.pdf".to_string(), false));
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
    recovery_action(&mut app, &mut question, RecoveryAction::Open(record, RowKind::Original));
    let saved_tab = app.active_tab.unwrap();
    app.edit("Note", |p| p.add_note(0, 100.0, 100.0, "EDITED IN THE OTHER TAB"));
    recovery_action(&mut app, &mut question, RecoveryAction::Open(record, RowKind::Recovered));
    let recovered = active_id(&app);

    // Replacing the file would lose that edit, or have it saved back over the recovered work later.
    app.save();
    let Some(Dialog::ReplaceOriginal { doc, path, .. }) = app.dialog.take() else { panic!("saving did not ask") };
    app.replace_original(doc, &path, AfterSave::Nothing);
    let Some(Dialog::Message(message)) = app.dialog.take() else { panic!("replacing went ahead") };
    assert!(message.contains("also open in another tab"), "{message}");
    assert_eq!(rotation_on_disk(), 0);
    assert!(app.doc.as_ref().unwrap().saved.is_none(), "the recovered document was tied to the file anyway");

    // Once that tab is saved, replacing goes ahead, and the tab showing the old file goes.
    app.activate(Some(saved_tab));
    app.save();
    let recovered_tab = (0..app.tabs.len()).find(|&i| app.doc_at(i).is_some_and(|d| d.id == recovered)).unwrap();
    app.activate(Some(recovered_tab));
    app.save();
    let Some(Dialog::ReplaceOriginal { doc, path, .. }) = app.dialog.take() else { panic!("saving did not ask") };
    app.replace_original(doc, &path, AfterSave::Nothing);
    assert!(app.dialog.is_none(), "{}", app.status);
    assert_eq!(rotation_on_disk(), 90);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.doc.as_ref().unwrap().path.as_deref(), Some(original.as_path()));

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
    let of_a = rows.iter().find(|r| r.kind == RowKind::Recovered && r.name.starts_with("a.pdf")).unwrap().record;
    assert_eq!(recovery_copies(&dir), 2);

    // Delete asks, with the name of the document, and Cancel keeps the copy.
    let mut question = None;
    assert!(recovery_action(&mut app, &mut question, RecoveryAction::Delete(of_a, RowKind::Recovered)));
    assert_eq!(question.as_ref().map(RecoveryQuestion::message), Some("Delete the recovered copy of a.pdf?".to_string()));
    assert_eq!(recovery_copies(&dir), 2);
    recovery_action(&mut app, &mut question, RecoveryAction::Cancel);
    assert!(question.is_none());
    assert_eq!(recovery_copies(&dir), 2);
    recovery_action(&mut app, &mut question, RecoveryAction::Delete(of_a, RowKind::Recovered));
    recovery_action(&mut app, &mut question, RecoveryAction::Confirm);
    assert_eq!(recovery_copies(&dir), 1);

    // Clear all asks too.
    recovery_action(&mut app, &mut question, RecoveryAction::ClearAll);
    assert_eq!(question, Some(RecoveryQuestion::ClearAll));
    assert_eq!(question.as_ref().unwrap().message(), carbide_engine::autosave::messages::DELETE_ALL);
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
    assert_eq!((doc.pdf.page_rotation(0).unwrap(), doc.pdf.revisions().len(), doc.dirty), (90, 1, false));

    // A signature written into the file: an edit and a save before it can no longer be undone.
    app.edit("Rotate", |p| p.rotate_pages(&[0], 90));
    app.save();
    assert!(app.cmd_enabled(Cmd::Undo), "a plain save keeps the history");
    let doc = app.doc.as_ref().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    app.sign_job = Some(SignJob { doc: doc.id, version: doc.version, path: a.clone(), done: "Signed".into(), receiver, result: None });
    sender.send(Ok(document(&["SIGNED"]).to_bytes().unwrap())).unwrap();
    app.poll_sign();
    assert_eq!(app.status, "Signed");
    assert!(!app.cmd_enabled(Cmd::Undo));
    app.undo();
    assert!(contains(&app.doc.as_ref().unwrap().bytes, "SIGNED"));
}
