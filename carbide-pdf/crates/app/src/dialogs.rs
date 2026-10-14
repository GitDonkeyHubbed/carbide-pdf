//! Modal and floating dialogs.

use std::path::PathBuf;

use carbide_engine::autosave::{format_time, messages as autosave_messages, AutoSaveSettings, RecordId, RowKind};
use carbide_engine::fs::Existing;
use carbide_engine::imposition::{ImpositionOptions, Media, RegistrationStyle, PAPER_SIZES};
use carbide_engine::imposition_plan::{Binding, LayoutKind, PrintingOrder, Style};
use carbide_engine::reduce::{
    messages as reduce_messages, reduce_files, FileOutcome, OutputOptions, ReduceOptions, TargetFolder,
};
use carbide_engine::signature::{
    CertifyLevel, HttpTimestampSource, Identity, SignOptions, SignatureReport, SubFilter, TimestampSource, Validity,
};
use carbide_engine::{
    validate_label, BatesOptions, HiddenItem, PageAnchor, PageDiff, Pdf, ProtectionOptions, Rect, Revision, StandardFont,
    WatermarkOptions,
};
use eframe::egui;

use crate::CarbideApp;

pub struct WatermarkForm {
    doc: u64,
    text: String,
    size: f32,
    opacity: f32,
    angle: f32,
    color: [f32; 3],
    first_page_only: bool,
}

pub struct BatesForm {
    doc: u64,
    prefix: String,
    suffix: String,
    start: u64,
    digits: usize,
    anchor: PageAnchor,
    size: f32,
}

pub struct ProtectForm {
    doc: u64,
    user: String,
    owner: String,
    print: bool,
    copy: bool,
}

pub struct ImposeForm {
    doc: u64,
    options: ImpositionOptions,
    custom_media: (f32, f32),
    media_choice: usize,
    error: Option<String>,
}

pub struct ReduceForm {
    /// The document "OK" reduces, while the dialog is not working on a list of files.
    doc: Option<u64>,
    multi: bool,
    files: Vec<PathBuf>,
    slider: u8,
    same_folder: bool,
    folder: Option<PathBuf>,
    add_to_names: bool,
    prefix: String,
    suffix: String,
    overwrite: bool,
    /// With the same file names: replace the originals with the reduced copies.
    replace_originals: bool,
    results: Option<Vec<FileOutcome>>,
}

/// Everything the Sign / Time Stamp dialogs collect.
#[derive(Clone)]
pub struct SignRequest {
    /// The document it signs; nothing is signed once another one is in front.
    pub doc: u64,
    pub identity_path: Option<PathBuf>,
    pub password: String,
    pub cades: bool,
    pub certify: Option<CertifyLevel>,
    pub lock: bool,
    pub reason: String,
    pub location: String,
    pub contact: String,
    pub appearance: Option<(usize, Rect)>,
    pub tsa_url: String,
    pub tsa_user: String,
    pub tsa_password: String,
    pub document_timestamp: bool,
}

impl SignRequest {
    /// `Ok(None)` for a document time stamp, which has no signer identity.
    pub fn load_identity(&self) -> Result<Option<Identity>, String> {
        if self.document_timestamp {
            return Ok(None);
        }
        let path = self.identity_path.as_ref().ok_or("Choose a digital ID file first.")?;
        let bytes = std::fs::read(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let identity = if extension == "pem" {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            Identity::from_pem(&text, &text)
        } else {
            Identity::from_pkcs12(&bytes, &self.password)
        };
        identity.map(Some).map_err(|e| e.to_string())
    }

    pub fn timestamp_source(&self) -> Option<HttpTimestampSource> {
        let url = self.tsa_url.trim();
        (!url.is_empty()).then(|| HttpTimestampSource {
            url: url.to_string(),
            username: Some(self.tsa_user.clone()).filter(|u| !u.is_empty()),
            password: Some(self.tsa_password.clone()).filter(|p| !p.is_empty()),
        })
    }

    pub fn options<'a>(
        &'a self,
        identity: Option<&'a Identity>,
        authority: Option<&'a HttpTimestampSource>,
    ) -> SignOptions<'a> {
        let text = |s: &String| Some(s.trim().to_string()).filter(|t| !t.is_empty());
        SignOptions {
            identity,
            sub_filter: if self.document_timestamp {
                SubFilter::DocumentTimeStamp
            } else if self.cades {
                SubFilter::CadesDetached
            } else {
                SubFilter::Pkcs7Detached
            },
            reason: text(&self.reason),
            location: text(&self.location),
            contact: text(&self.contact),
            appearance: self.appearance,
            certify: self.certify,
            lock_document: self.lock,
            timestamp: authority.map(|a| a as &dyn TimestampSource),
            signing_time: None,
        }
    }
}

/// Files about to be written into a folder, some of whose names are already taken there.
pub enum Writing {
    /// An image of every page.
    Images { folder: PathBuf },
    /// One file per page range.
    Split { folder: PathBuf, ranges: String },
}

/// What a document is exported as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Export {
    Images,
    Text,
}

/// What is carried out once a save has written the file.
pub enum AfterSave {
    Nothing,
    /// Sign the saved file.
    Sign(Box<SignRequest>),
    /// Close the saved document's tab (the close prompt's Save).
    CloseTab,
    /// Save the other modified documents and quit (the quit prompt's Save All).
    Quit,
}

/// The dialogs that act on a document, or show one, name it (see [`Dialog::document`]): they close
/// as soon as another document comes to the front, so that answering one can never act on a
/// document other than the one it was opened for.
pub enum Dialog {
    Sign(Box<SignRequest>),
    Signatures { doc: u64, reports: Vec<SignatureReport>, notice: String },
    Message(String),
    Password { path: PathBuf, bytes: Vec<u8>, input: String, failed: bool },
    /// Saving document `doc` to `path` has to write the whole file again, which costs its
    /// signatures or its saved revisions. `then` waits for the save. The question is dropped as
    /// soon as another document comes to the front.
    ConfirmRewrite {
        doc: u64,
        path: PathBuf,
        content_removed: bool,
        breaks_signatures: bool,
        drops_revisions: bool,
        then: AfterSave,
    },
    /// Writing files for document `doc` would replace the files `taken`.
    ExistingFiles { doc: u64, writing: Writing, taken: Vec<PathBuf> },
    /// Exporting the protected document `doc` gives files that are not protected.
    ConfirmExport { doc: u64, export: Export },
    /// Appending `others` (named `names`) to document `doc`: `protected` are protected in a way it
    /// does not share, so their pages would be saved with its protection (`keeps_protection`) or
    /// none.
    ConfirmMerge { doc: u64, others: Vec<Pdf>, names: Vec<String>, protected: Vec<String>, keeps_protection: bool },
    /// The files to combine are protected in different ways, so the result cannot keep them all.
    ConfirmCombine { documents: Vec<Pdf> },
    /// Remove Hidden Information from document `doc`, with each item and whether it is chosen.
    RemoveHidden { doc: u64, chosen: Vec<(HiddenItem, bool)> },
    /// Redactions were just applied to document `doc`: offer to remove its hidden information too.
    SuggestRemoveHidden { doc: u64 },
    Watermark(WatermarkForm),
    Bates(BatesForm),
    Protect(ProtectForm),
    Split { doc: u64, ranges: String },
    Note { doc: u64, page: usize, x: f32, y: f32, on_page: bool, text: String },
    SaveRevision { doc: u64, label: String, error: Option<String> },
    OpenRevision { doc: u64, items: Vec<Revision>, selected: Option<usize> },
    Impose(Box<ImposeForm>),
    Reduce(Box<ReduceForm>),
    Compare { title: String, diffs: Vec<PageDiff> },
    /// The Auto Recovery panel, with the question it is asking, if any.
    AutoRecovery { question: Option<RecoveryQuestion> },
    /// Saving the recovered document `doc` would replace `path`, the file it was recovered for.
    ReplaceOriginal { doc: u64, path: PathBuf, then: AfterSave },
    Preferences { enabled: bool, minutes: String, error: Option<&'static str> },
}

impl Dialog {
    /// The document a dialog acts on or shows, for a dialog that only makes sense while that
    /// document is in front.
    pub fn document(&self) -> Option<u64> {
        match self {
            Dialog::ConfirmRewrite { doc, .. }
            | Dialog::ExistingFiles { doc, .. }
            | Dialog::ConfirmExport { doc, .. }
            | Dialog::ConfirmMerge { doc, .. }
            | Dialog::RemoveHidden { doc, .. }
            | Dialog::SuggestRemoveHidden { doc }
            | Dialog::ReplaceOriginal { doc, .. }
            | Dialog::Signatures { doc, .. }
            | Dialog::Split { doc, .. }
            | Dialog::Note { doc, .. }
            | Dialog::SaveRevision { doc, .. }
            | Dialog::OpenRevision { doc, .. } => Some(*doc),
            Dialog::Sign(request) => Some(request.doc),
            Dialog::Watermark(form) => Some(form.doc),
            Dialog::Bates(form) => Some(form.doc),
            Dialog::Protect(form) => Some(form.doc),
            Dialog::Impose(form) => Some(form.doc),
            // Working on a list of files, it no longer acts on the document in front.
            Dialog::Reduce(form) => form.doc.filter(|_| !form.multi),
            _ => None,
        }
    }

    /// Remove Hidden Information for document `doc`, with the usual items chosen.
    pub fn remove_hidden(doc: u64) -> Dialog {
        Dialog::RemoveHidden { doc, chosen: HiddenItem::ALL.into_iter().map(|item| (item, item.chosen_by_default())).collect() }
    }

    pub fn watermark(doc: u64) -> Dialog {
        Dialog::Watermark(WatermarkForm {
            doc,
            text: "CONFIDENTIAL".into(),
            size: 72.0,
            opacity: 0.3,
            angle: 45.0,
            color: [0.6, 0.6, 0.6],
            first_page_only: false,
        })
    }

    pub fn bates(doc: u64) -> Dialog {
        Dialog::Bates(BatesForm {
            doc,
            prefix: String::new(),
            suffix: String::new(),
            start: 1,
            digits: 6,
            anchor: PageAnchor::BottomRight,
            size: 10.0,
        })
    }

    pub fn sign(doc: u64, appearance: Option<(usize, Rect)>, certify: bool) -> Dialog {
        Dialog::Sign(Box::new(SignRequest {
            doc,
            identity_path: None,
            password: String::new(),
            cades: false,
            certify: certify.then_some(CertifyLevel::FormFilling),
            lock: false,
            reason: String::new(),
            location: String::new(),
            contact: String::new(),
            appearance,
            tsa_url: String::new(),
            tsa_user: String::new(),
            tsa_password: String::new(),
            document_timestamp: false,
        }))
    }

    pub fn timestamp(doc: u64) -> Dialog {
        let Dialog::Sign(mut request) = Dialog::sign(doc, None, false) else { unreachable!() };
        request.document_timestamp = true;
        Dialog::Sign(request)
    }

    pub fn protect(doc: u64) -> Dialog {
        Dialog::Protect(ProtectForm { doc, user: String::new(), owner: String::new(), print: true, copy: true })
    }

    pub fn impose(doc: u64) -> Dialog {
        Dialog::Impose(Box::new(ImposeForm {
            doc,
            options: ImpositionOptions::default(),
            custom_media: (612.0, 792.0),
            media_choice: PAPER_SIZES.iter().position(|(n, _, _)| *n == "Letter").unwrap_or(0),
            error: None,
        }))
    }

    /// Reduce File Size for document `doc`, the file `current`.
    pub fn reduce(doc: Option<u64>, current: Option<PathBuf>) -> Dialog {
        Dialog::Reduce(Box::new(ReduceForm {
            doc,
            multi: false,
            files: current.into_iter().collect(),
            slider: carbide_engine::reduce::DEFAULT_IMAGE_FACTOR,
            same_folder: true,
            folder: None,
            // Next to the originals under new names: the originals are kept unless replacing
            // them is chosen.
            add_to_names: true,
            prefix: String::new(),
            suffix: carbide_engine::reduce::DEFAULT_SUFFIX.into(),
            overwrite: false,
            replace_originals: false,
            results: None,
        }))
    }

    pub fn auto_recovery() -> Dialog {
        Dialog::AutoRecovery { question: None }
    }

    pub fn preferences(settings: &AutoSaveSettings) -> Dialog {
        Dialog::Preferences { enabled: settings.enabled, minutes: settings.interval_minutes.to_string(), error: None }
    }
}

fn window<'a>(title: &str) -> egui::Window<'a> {
    egui::Window::new(title.to_string()).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
}

fn buttons(ui: &mut egui::Ui, ok: &str, ok_enabled: bool) -> (bool, bool) {
    let mut result = (false, false);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.add_enabled(ok_enabled, egui::Button::new(ok)).clicked() {
            result.0 = true;
        }
        if ui.button("Cancel").clicked() {
            result.1 = true;
        }
    });
    result
}

/// Draws the active dialog and carries out its action. A dialog about a document that is no
/// longer in front goes without being shown.
pub fn show(app: &mut CarbideApp, ctx: &egui::Context) {
    let Some(mut dialog) = app.dialog.take() else { return };
    if dialog.document().is_some_and(|doc| app.doc.as_ref().map(|d| d.id) != Some(doc)) {
        return;
    }
    let mut open = true;

    match &mut dialog {
        Dialog::Message(text) => {
            window("Carbide PDF").show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(text.as_str());
                if ui.button("OK").clicked() {
                    open = false;
                }
            });
        }

        Dialog::Sign(request) => {
            let title = if request.document_timestamp {
                "Time Stamp Document"
            } else if request.certify.is_some() {
                "Certify Document"
            } else {
                "Sign Document"
            };
            window(title).show(ctx, |ui| {
                if !request.document_timestamp {
                    ui.horizontal(|ui| {
                        ui.label("Digital ID");
                        let shown = request.identity_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none chosen)".into());
                        ui.label(shown);
                        if ui.button("Browse...").clicked() {
                            if let Some(path) = rfd::FileDialog::new().add_filter("Digital IDs", &["p12", "pfx", "pem"]).pick_file() {
                                request.identity_path = Some(path);
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut request.password).password(true));
                    });
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut request.cades, false, "PKCS#7 - Detached");
                        ui.selectable_value(&mut request.cades, true, "CAdES-Equivalent");
                    });
                    egui::ComboBox::from_label("Certify")
                        .selected_text(match request.certify {
                            None => "Do not certify",
                            Some(CertifyLevel::NoChanges) => "No changes allowed",
                            Some(CertifyLevel::FormFilling) => "Allow filling forms and signing",
                            Some(CertifyLevel::Commenting) => "Allow filling forms, signing and commenting",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut request.certify, None, "Do not certify");
                            ui.selectable_value(&mut request.certify, Some(CertifyLevel::NoChanges), "No changes allowed");
                            ui.selectable_value(&mut request.certify, Some(CertifyLevel::FormFilling), "Allow filling forms and signing");
                            ui.selectable_value(&mut request.certify, Some(CertifyLevel::Commenting), "Allow filling forms, signing and commenting");
                        });
                    ui.checkbox(&mut request.lock, "Lock the document after signing");
                    for (label, value) in [
                        ("Reason", &mut request.reason),
                        ("Location", &mut request.location),
                        ("Contact Info", &mut request.contact),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(label);
                            ui.text_edit_singleline(value);
                        });
                    }
                    ui.label(match request.appearance {
                        Some((page, _)) => format!("Visible signature on page {}", page + 1),
                        None => "Invisible signature (use the Sign tool to draw a visible one)".to_string(),
                    });
                }
                ui.collapsing("Time Stamp Service", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Server URL");
                        ui.text_edit_singleline(&mut request.tsa_url);
                    });
                    ui.horizontal(|ui| {
                        ui.label("User");
                        ui.text_edit_singleline(&mut request.tsa_user);
                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut request.tsa_password).password(true));
                    });
                });
                ui.weak("The file is saved first; the signature is added as an update, so earlier signatures stay valid.");
                let ready = if request.document_timestamp {
                    !request.tsa_url.trim().is_empty()
                } else {
                    request.identity_path.is_some()
                };
                let (ok, cancel) = buttons(ui, "Sign", ready);
                if ok {
                    let request = (**request).clone();
                    app.sign_document(request);
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::Signatures { reports, notice, .. } => {
            window("Signatures").show(ctx, |ui| {
                ui.strong(notice.as_str());
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for report in reports.iter() {
                        ui.group(|ui| {
                            let color = match report.validity {
                                Validity::Valid => egui::Color32::from_rgb(20, 130, 40),
                                Validity::Invalid => egui::Color32::from_rgb(190, 30, 30),
                                _ => egui::Color32::from_rgb(170, 110, 0),
                            };
                            ui.colored_label(color, format!("{}: {}", report.field_name, report.status_line()));
                            if let Some(who) = &report.signer {
                                let role = if report.document_time_stamp { "Time stamp authority" } else { "Signer" };
                                ui.label(format!("{role}: {who}"));
                            }
                            if let Some(time) = time_stamp_time(report) {
                                ui.label(format!("Time stamp: {time}"));
                            }
                            if let Some(claimed) = &report.claimed_name {
                                ui.weak(format!("Name in signature (unverified): {claimed}"));
                            }
                            if let Some(reason) = &report.reason {
                                ui.label(format!("Reason: {reason}"));
                            }
                            if let Some(location) = &report.location {
                                ui.label(format!("Location: {location}"));
                            }
                            for line in report.description_lines() {
                                ui.label(format!("  {line}"));
                            }
                            if report.certifying {
                                ui.label("  Certifying signature");
                            }
                        });
                    }
                });
                if ui.button("Close").clicked() {
                    open = false;
                }
            });
        }

        Dialog::Password { path, bytes, input, failed } => {
            window("This document is password protected").show(ctx, |ui| {
                ui.label(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                ui.add(egui::TextEdit::singleline(input).password(true).hint_text("Password"));
                if *failed {
                    ui.colored_label(egui::Color32::RED, "That password did not work.");
                }
                let (unlock, cancel) = buttons(ui, "Unlock", !input.is_empty());
                if unlock {
                    // The bytes stay here for another try if the password is wrong.
                    if app.open_with_password(path.clone(), bytes.clone(), input.clone()) {
                        open = false;
                    } else {
                        *failed = true;
                    }
                }
                if cancel {
                    open = false;
                }
            });
        }

        Dialog::ConfirmRewrite { doc, path, content_removed, breaks_signatures, drops_revisions, then } => {
            window("Save").show(ctx, |ui| {
                ui.set_max_width(420.0);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ui.label(if *content_removed {
                    format!("Saving \"{name}\" removes the content you deleted or redacted permanently, by writing the whole file again.")
                } else {
                    format!("\"{name}\" cannot be updated in place, so saving writes the whole file again.")
                });
                if *breaks_signatures {
                    ui.label("Its existing signatures will no longer validate.");
                }
                if *drops_revisions {
                    ui.label("Its saved revisions will be dropped.");
                }
                let (ok, cancel) = buttons(ui, "Save", true);
                if ok {
                    let then = std::mem::replace(then, AfterSave::Nothing);
                    app.confirm_rewrite(*doc, &path.clone(), then);
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::ExistingFiles { doc, writing, taken } => {
            window("Replace Files?").show(ctx, |ui| {
                ui.set_max_width(440.0);
                let folder = match writing {
                    Writing::Images { folder } | Writing::Split { folder, .. } => folder.display().to_string(),
                };
                let name = |path: &PathBuf| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if let [only] = taken.as_slice() {
                    ui.label(format!("\"{}\" already exists in {folder}.", name(only)));
                } else {
                    ui.label(format!("{} files with these names already exist in {folder}:", taken.len()));
                    for path in taken.iter().take(6) {
                        ui.label(format!("  {}", name(path)));
                    }
                    if taken.len() > 6 {
                        ui.label(format!("  and {} more", taken.len() - 6));
                    }
                }
                ui.weak("Replace them, or keep them and give the new files numbered names.");
                ui.add_space(6.0);
                let mut choice = None;
                ui.horizontal(|ui| {
                    if ui.button("Replace").clicked() {
                        choice = Some(Existing::Replace);
                    }
                    if ui.button("Keep Both").clicked() {
                        choice = Some(Existing::KeepBoth);
                    }
                    if ui.button("Cancel").clicked() {
                        open = false;
                    }
                });
                if let Some(existing) = choice {
                    let writing = std::mem::replace(writing, Writing::Images { folder: PathBuf::new() });
                    app.continue_writing(*doc, writing, existing);
                    open = false;
                }
            });
        }

        Dialog::ConfirmExport { doc, export } => {
            window("Export").show(ctx, |ui| {
                ui.set_max_width(420.0);
                let what = match export {
                    Export::Images => "images",
                    Export::Text => "text",
                };
                let name = app.doc.as_ref().map(|d| d.title()).unwrap_or_default();
                ui.label(format!("\"{name}\" is password-protected, but the exported {what} will not be."));
                ui.label("Anyone who can open the exported files can read the content.");
                let (ok, cancel) = buttons(ui, "Export Anyway", true);
                if ok {
                    app.export_confirmed(*doc, *export);
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::ConfirmMerge { doc, others, names, protected, keeps_protection } => {
            window("Merge PDFs").show(ctx, |ui| {
                ui.set_max_width(440.0);
                let list = protected.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
                ui.label(format!("{list} {} password-protected.", if protected.len() == 1 { "is" } else { "are" }));
                ui.label(if *keeps_protection {
                    "Merged into this document, those pages are saved with this document's passwords and permissions instead."
                } else {
                    "This document is not protected, so merged into it those pages are saved without protection."
                });
                let (ok, cancel) = buttons(ui, "Merge", true);
                if ok {
                    if app.doc.as_ref().map(|d| d.id) == Some(*doc) {
                        app.merge_into_current(std::mem::take(others), std::mem::take(names), true);
                    }
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::ConfirmCombine { documents } => {
            window("Combine Files").show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label("These files are protected with different passwords or permissions, so one document cannot keep the protection of all of them.");
                ui.label("Combine them into a document without protection?");
                let (ok, cancel) = buttons(ui, "Combine Without Protection", true);
                if ok {
                    app.combine(std::mem::take(documents), true);
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::RemoveHidden { doc, chosen } => {
            window("Remove Hidden Information").show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label("Remove what the document holds besides what its pages show:");
                for (item, on) in chosen.iter_mut() {
                    ui.checkbox(on, item.label());
                }
                ui.weak("The document is written in full when it is saved, so nothing removed stays in the file.");
                let items: Vec<HiddenItem> = chosen.iter().filter(|(_, on)| *on).map(|(item, _)| *item).collect();
                let (ok, cancel) = buttons(ui, "Remove", !items.is_empty());
                if ok {
                    open = false;
                    app.remove_hidden_information(*doc, items);
                }
                open &= !cancel;
            });
        }

        Dialog::SuggestRemoveHidden { doc } => {
            window("Redaction").show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label("The marked content is gone from the pages.");
                ui.label(
                    "The same text can still be in the document's information, bookmarks, attachments, comments or \
                     other content the pages do not show. Remove hidden information now?",
                );
                let (ok, cancel) = buttons(ui, "Remove Hidden Information...", true);
                if ok {
                    let mut dialog = Dialog::remove_hidden(*doc);
                    // Bookmark titles often repeat what was redacted.
                    if let Dialog::RemoveHidden { chosen, .. } = &mut dialog {
                        chosen.iter_mut().for_each(|(_, on)| *on = true);
                    }
                    app.dialog = Some(dialog);
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::Watermark(f) => {
            window("Watermark").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Text");
                    ui.text_edit_singleline(&mut f.text);
                });
                ui.add(egui::Slider::new(&mut f.size, 12.0..=200.0).text("Size (pt)"));
                ui.add(egui::Slider::new(&mut f.opacity, 0.05..=1.0).text("Opacity"));
                ui.add(egui::Slider::new(&mut f.angle, -90.0..=90.0).text("Angle"));
                ui.horizontal(|ui| {
                    ui.label("Color");
                    ui.color_edit_button_rgb(&mut f.color);
                });
                ui.checkbox(&mut f.first_page_only, "First page only");
                ui.weak("The watermark is added to the page content. Edit > Undo reverts it before saving.");
                let (ok, cancel) = buttons(ui, "Apply", !f.text.trim().is_empty());
                if ok {
                    let mut options = WatermarkOptions::new(f.text.clone());
                    options.font = StandardFont::HelveticaBold;
                    options.font_size = f.size;
                    options.opacity = f.opacity;
                    options.rotation_degrees = f.angle;
                    options.color = f.color;
                    if f.first_page_only {
                        options.pages = Some(vec![0]);
                    }
                    app.edit("Watermark", move |pdf| pdf.apply_watermark(&options));
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::Bates(f) => {
            window("Bates Numbering").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Prefix");
                    ui.text_edit_singleline(&mut f.prefix);
                    ui.label("Suffix");
                    ui.text_edit_singleline(&mut f.suffix);
                });
                ui.horizontal(|ui| {
                    ui.label("Start at");
                    ui.add(egui::DragValue::new(&mut f.start).range(0..=999_999_999));
                    ui.label("Digits");
                    ui.add(egui::DragValue::new(&mut f.digits).range(1..=12));
                });
                egui::ComboBox::from_label("Position").selected_text(anchor_name(f.anchor)).show_ui(ui, |ui| {
                    for anchor in PageAnchor::ALL {
                        ui.selectable_value(&mut f.anchor, anchor, anchor_name(anchor));
                    }
                });
                let mut options = BatesOptions {
                    prefix: f.prefix.clone(),
                    suffix: f.suffix.clone(),
                    start_number: f.start,
                    digits: f.digits,
                    anchor: f.anchor,
                    font_size: f.size,
                    ..Default::default()
                };
                ui.monospace(format!("First page: {}", options.label(0)));
                let (ok, cancel) = buttons(ui, "Apply", true);
                if ok {
                    options.font_size = f.size;
                    app.edit("Bates numbering", move |pdf| pdf.apply_bates(&options));
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::Protect(f) => {
            window("Password-Protected Copy").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Password to open (optional)");
                    ui.add(egui::TextEdit::singleline(&mut f.user).password(true));
                });
                ui.horizontal(|ui| {
                    ui.label("Owner password (required)");
                    ui.add(egui::TextEdit::singleline(&mut f.owner).password(true));
                });
                ui.checkbox(&mut f.print, "Allow printing");
                ui.checkbox(&mut f.copy, "Allow copying text");
                ui.weak("The open document is not changed; an AES-256 encrypted copy is written.");
                let (ok, cancel) = buttons(ui, "Save...", !f.owner.is_empty());
                if ok {
                    if let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name("document-protected.pdf").save_file() {
                        let mut options = ProtectionOptions::new(f.user.clone(), f.owner.clone());
                        options.allow_printing = f.print;
                        options.allow_copying = f.copy;
                        let result = app.doc.as_mut().map(|d| d.pdf.save_encrypted(&path, &options));
                        match result {
                            Some(Ok(())) => app.status = format!("Saved protected copy {}", path.display()),
                            Some(Err(e)) => app.alert(format!("Could not protect the document: {e}")),
                            None => {}
                        }
                    }
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::Split { ranges, .. } => {
            window("Split by Page Ranges").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Ranges, e.g. 1-3, 4-6, 7");
                    ui.text_edit_singleline(ranges);
                });
                ui.weak("Each range becomes its own PDF in the folder you choose.");
                let (ok, cancel) = buttons(ui, "Split...", !ranges.trim().is_empty());
                if ok {
                    split_document(app, ranges);
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::Note { page, x, y, on_page, text, .. } => {
            window(if *on_page { "Add Text" } else { "Add Note" }).show(ctx, |ui| {
                ui.add(egui::TextEdit::multiline(text).desired_rows(3).desired_width(320.0));
                let (ok, cancel) = buttons(ui, "Add", !text.trim().is_empty());
                if ok {
                    let (page, x, y, on_page, text) = (*page, *x, *y, *on_page, text.clone());
                    app.edit("Add note", move |pdf| {
                        if on_page {
                            pdf.add_text_box(page, x, y, &text, 14.0)
                        } else {
                            pdf.add_note(page, x, y, &text)
                        }
                    });
                    open = false;
                }
                open &= !cancel;
            });
        }

        Dialog::SaveRevision { label, error, .. } => {
            window("Save Revision").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Label:");
                    ui.text_edit_singleline(label);
                });
                if let Some(message) = error {
                    ui.colored_label(egui::Color32::RED, message.as_str());
                }
                let (ok, cancel) = buttons(ui, "OK", true);
                if ok {
                    match validate_label(label) {
                        Err(message) => *error = Some(message.to_string()),
                        Ok(()) => match app.save_revision(label) {
                            Ok(()) => open = false,
                            Err(message) => *error = Some(message),
                        },
                    }
                }
                open &= !cancel;
            });
        }

        Dialog::OpenRevision { items, selected, .. } => {
            window("Open Revision").show(ctx, |ui| {
                egui::Grid::new("revisions").striped(true).show(ui, |ui| {
                    ui.strong("Label");
                    ui.strong("Time");
                    ui.end_row();
                    for (i, item) in items.iter().enumerate() {
                        let chosen = *selected == Some(i);
                        if ui.selectable_label(chosen, &item.label).clicked() {
                            *selected = Some(i);
                        }
                        ui.label(item.display_time());
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    // Open is available only while exactly one row is selected.
                    if ui.add_enabled(selected.is_some(), egui::Button::new("Open")).clicked() {
                        if let Some(i) = *selected {
                            let revision = items[i].clone();
                            app.open_revision(&revision);
                            open = false;
                        }
                    }
                    if ui.button("Close").clicked() {
                        open = false;
                    }
                });
            });
        }

        Dialog::Impose(form) => {
            window("Imposition").show(ctx, |ui| {
                impose_form(ui, form);
                if let Some(message) = &form.error {
                    ui.colored_label(egui::Color32::RED, message.as_str());
                }
                let (ok, cancel) = buttons(ui, "OK", true);
                if ok {
                    form.error = None;
                    if let Err(message) = run_imposition(app, form) {
                        form.error = Some(message);
                    } else {
                        open = false;
                    }
                }
                open &= !cancel;
            });
        }

        Dialog::Reduce(form) => {
            let keep = reduce_dialog(app, ctx, form);
            open = keep;
        }

        Dialog::Compare { title, diffs } => {
            window("Compare").show(ctx, |ui| {
                ui.label(title.as_str());
                let changed: Vec<&PageDiff> = diffs.iter().filter(|d| !d.is_identical()).collect();
                ui.strong(if changed.is_empty() {
                    "No text differences found.".to_string()
                } else {
                    format!("{} page(s) differ.", changed.len())
                });
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for diff in changed {
                        ui.label(egui::RichText::new(format!("Page {}", diff.page_number)).strong());
                        for line in &diff.removed {
                            ui.colored_label(egui::Color32::from_rgb(190, 30, 30), format!("- {line}"));
                        }
                        for line in &diff.added {
                            ui.colored_label(egui::Color32::from_rgb(20, 130, 40), format!("+ {line}"));
                        }
                    }
                });
                ui.weak("Text comparison only; layout and image changes are not detected.");
                if ui.button("Done").clicked() {
                    open = false;
                }
            });
        }

        Dialog::AutoRecovery { question } => {
            open = recovery_panel(app, ctx, question);
        }

        Dialog::ReplaceOriginal { doc, path, then } => {
            window("Save Recovered Document").show(ctx, |ui| {
                ui.set_max_width(440.0);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ui.label(format!("Save the recovered document over \"{name}\"?"));
                ui.label(format!("This replaces {} on disk with the recovered version.", path.display()));
                ui.add_space(6.0);
                let mut choice = None;
                ui.horizontal(|ui| {
                    if ui.button("Replace").clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Save As...").clicked() {
                        choice = Some(false);
                    }
                    if ui.button("Cancel").clicked() {
                        open = false;
                    }
                });
                if let Some(replace) = choice {
                    open = false;
                    let then = std::mem::replace(then, AfterSave::Nothing);
                    if replace {
                        app.replace_original(*doc, &path.clone(), then);
                    } else {
                        app.save_as_then(then);
                    }
                }
            });
        }

        Dialog::Preferences { enabled, minutes, error } => {
            window(autosave_messages::PREFERENCE_TITLE).show(ctx, |ui| {
                ui.checkbox(enabled, autosave_messages::PREFERENCE_CHECKBOX);
                ui.horizontal(|ui| {
                    ui.add_enabled(*enabled, egui::TextEdit::singleline(minutes).desired_width(50.0));
                    ui.label(autosave_messages::PREFERENCE_UNIT);
                });
                if let Some(message) = error {
                    ui.colored_label(egui::Color32::RED, *message);
                }
                let (ok, cancel) = buttons(ui, "OK", true);
                if ok {
                    match app.settings.apply_page(*enabled, minutes) {
                        Ok(()) => {
                            crate::paths::save_autosave_settings(&app.settings);
                            open = false;
                        }
                        Err(message) => *error = Some(message),
                    }
                }
                open &= !cancel;
            });
        }
    }

    // An action above may have opened a different dialog (an alert, say); that one wins.
    if open && app.dialog.is_none() {
        app.dialog = Some(dialog);
    }
}

fn anchor_name(anchor: PageAnchor) -> &'static str {
    match anchor {
        PageAnchor::TopLeft => "Top left",
        PageAnchor::TopCenter => "Top center",
        PageAnchor::TopRight => "Top right",
        PageAnchor::BottomLeft => "Bottom left",
        PageAnchor::BottomCenter => "Bottom center",
        PageAnchor::BottomRight => "Bottom right",
    }
}

fn split_document(app: &mut CarbideApp, ranges: &str) {
    let Some(doc) = app.doc.as_ref() else { return };
    if let Err(e) = Pdf::parse_ranges(ranges, doc.pdf.page_count()) {
        return app.alert(e.to_string());
    }
    let Some(folder) = rfd::FileDialog::new().set_title("Choose Folder").pick_folder() else { return };
    app.split_into(folder, ranges, None);
}

// ---- imposition ----------------------------------------------------------------------------

fn impose_form(ui: &mut egui::Ui, form: &mut ImposeForm) {
    let o = &mut form.options;
    let layouts = [
        (LayoutKind::Booklet2Up, "2-Up Booklet"),
        (LayoutKind::Booklet4Up, "4-Up Booklet"),
        (LayoutKind::Booklet8Up, "8-Up Booklet"),
        (LayoutKind::Sequential, "Sequential"),
        (LayoutKind::StepAndRepeat, "Step and Repeat"),
        (LayoutKind::CutStacks, "Cut Stacks"),
    ];
    egui::ComboBox::from_label("Layout")
        .selected_text(layouts.iter().find(|(l, _)| *l == o.plan.layout).map(|(_, n)| *n).unwrap_or(""))
        .show_ui(ui, |ui| {
            for (layout, name) in layouts {
                if ui.selectable_value(&mut o.plan.layout, layout, name).clicked() {
                    let (c, r) = match layout {
                        LayoutKind::Booklet2Up => (2, 1),
                        LayoutKind::Booklet4Up => (2, 2),
                        LayoutKind::Booklet8Up => (4, 2),
                        _ => (o.plan.columns.max(1), o.plan.rows.max(1)),
                    };
                    o.plan.columns = c;
                    o.plan.rows = r;
                }
            }
        });
    let booklet = matches!(o.plan.layout, LayoutKind::Booklet2Up | LayoutKind::Booklet4Up | LayoutKind::Booklet8Up);
    if booklet {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut o.plan.binding, Binding::Saddle, "Saddle-Stitching");
            ui.selectable_value(&mut o.plan.binding, Binding::Perfect, "Perfect Binding");
        });
        if o.plan.binding == Binding::Perfect {
            ui.horizontal(|ui| {
                ui.label("Sheets per fold");
                ui.add(egui::DragValue::new(&mut o.plan.sheets_per_fold).range(1..=64));
            });
        }
        if o.plan.layout != LayoutKind::Booklet2Up {
            egui::ComboBox::from_label("Style")
                .selected_text(match o.plan.style {
                    Style::Sheetwise => "Sheetwise",
                    Style::WorkAndTurn => "Work and Turn",
                    Style::WorkAndTumble => "Work and Tumble",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut o.plan.style, Style::Sheetwise, "Sheetwise");
                    ui.selectable_value(&mut o.plan.style, Style::WorkAndTurn, "Work and Turn");
                    ui.selectable_value(&mut o.plan.style, Style::WorkAndTumble, "Work and Tumble");
                });
        }
    } else if o.plan.layout != LayoutKind::Sequential {
        ui.horizontal(|ui| {
            ui.label("Columns");
            ui.add(egui::DragValue::new(&mut o.plan.columns).range(1..=20));
            ui.label("Rows");
            ui.add(egui::DragValue::new(&mut o.plan.rows).range(1..=20));
        });
    }
    ui.checkbox(&mut o.plan.double_sided, "Double-Sided");
    egui::ComboBox::from_label("Printing order")
        .selected_text(match o.plan.printing_order {
            PrintingOrder::Interleaved => "(front, back, front, back)",
            PrintingOrder::FrontsThenBacks => "All fronts, then all backs",
            PrintingOrder::FrontsThenBacksReversed => "All fronts, then all backs in reverse",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut o.plan.printing_order, PrintingOrder::Interleaved, "(front, back, front, back)");
            ui.selectable_value(&mut o.plan.printing_order, PrintingOrder::FrontsThenBacks, "All fronts, then all backs");
            ui.selectable_value(
                &mut o.plan.printing_order,
                PrintingOrder::FrontsThenBacksReversed,
                "All fronts, then all backs in reverse",
            );
        });

    ui.separator();
    // Media: built-in sizes, custom, or autosize.
    let media_names: Vec<String> = PAPER_SIZES
        .iter()
        .map(|(n, _, _)| n.to_string())
        .chain(["Custom size".to_string(), "Autosize".to_string()])
        .collect();
    egui::ComboBox::from_label("Media").selected_text(&media_names[form.media_choice.min(media_names.len() - 1)]).show_ui(ui, |ui| {
        for (i, name) in media_names.iter().enumerate() {
            ui.selectable_value(&mut form.media_choice, i, name);
        }
    });
    let custom_index = PAPER_SIZES.len();
    o.media = if form.media_choice < custom_index {
        Media::Named(PAPER_SIZES[form.media_choice].0.to_string())
    } else if form.media_choice == custom_index {
        ui.horizontal(|ui| {
            ui.label("Width (pt)");
            ui.add(egui::DragValue::new(&mut form.custom_media.0).range(72.0..=14400.0));
            ui.label("Height (pt)");
            ui.add(egui::DragValue::new(&mut form.custom_media.1).range(72.0..=14400.0));
        });
        Media::Custom { width: form.custom_media.0, height: form.custom_media.1 }
    } else {
        Media::Autosize
    };
    ui.checkbox(&mut o.auto_rotate, "Auto-Rotate");
    ui.checkbox(&mut o.fit_to_media, "Scale to fit the media");
    if !o.fit_to_media {
        ui.horizontal(|ui| {
            ui.label("Scale H %");
            ui.add(egui::DragValue::new(&mut o.scale_x_percent).range(1.0..=1000.0));
            ui.label("V %");
            ui.add(egui::DragValue::new(&mut o.scale_y_percent).range(1.0..=1000.0));
        });
    }
    ui.horizontal(|ui| {
        ui.label("Margins (pt) L");
        ui.add(egui::DragValue::new(&mut o.margins.left).range(0.0..=500.0));
        ui.label("R");
        ui.add(egui::DragValue::new(&mut o.margins.right).range(0.0..=500.0));
        ui.label("T");
        ui.add(egui::DragValue::new(&mut o.margins.top).range(0.0..=500.0));
        ui.label("B");
        ui.add(egui::DragValue::new(&mut o.margins.bottom).range(0.0..=500.0));
    });
    ui.horizontal(|ui| {
        ui.label("Gutter H");
        ui.add(egui::DragValue::new(&mut o.gutter_horizontal).range(0.0..=200.0));
        ui.label("V");
        ui.add(egui::DragValue::new(&mut o.gutter_vertical).range(0.0..=200.0));
        ui.label("Spread");
        ui.add(egui::DragValue::new(&mut o.spread).range(0.0..=200.0));
        ui.label("Creep");
        ui.add(egui::DragValue::new(&mut o.creep).speed(0.1).range(0.0..=50.0));
    });

    ui.separator();
    ui.horizontal(|ui| {
        ui.checkbox(&mut o.crop_marks, "Crop Marks");
        ui.checkbox(&mut o.inner_marks, "Inner Marks");
        ui.checkbox(&mut o.registration_marks, "Registration Marks");
    });
    if o.registration_marks {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut o.registration_style, RegistrationStyle::Cross, "Cross");
            ui.selectable_value(&mut o.registration_style, RegistrationStyle::Circle, "Circle");
        });
    }
    ui.checkbox(&mut o.page_info, "Description");
    ui.checkbox(&mut o.scale_large_pages, "Scale Large Pages");
    ui.checkbox(&mut o.flip_backs, "Flip the back side of each sheet");
    ui.horizontal(|ui| {
        if ui.button("Save Settings...").clicked() {
            if let Some(path) = rfd::FileDialog::new().set_file_name("imposition.ini").save_file() {
                if let Err(e) = carbide_engine::fs::write_replacing(&path, o.to_ini().as_bytes()) {
                    form.error = Some(format!("Could not save the settings: {e}"));
                }
            }
        }
        if ui.button("Load Settings...").clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| ImpositionOptions::from_ini(&t).map_err(|e| e.to_string())) {
                    Ok(loaded) => {
                        form.media_choice = match &loaded.media {
                            Media::Named(n) => PAPER_SIZES.iter().position(|(p, _, _)| p.eq_ignore_ascii_case(n)).unwrap_or(0),
                            Media::Custom { width, height } => {
                                form.custom_media = (*width, *height);
                                PAPER_SIZES.len()
                            }
                            _ => PAPER_SIZES.len() + 1,
                        };
                        *o = loaded;
                    }
                    Err(message) => form.error = Some(message),
                }
            }
        }
    });
}

fn run_imposition(app: &mut CarbideApp, form: &ImposeForm) -> Result<(), String> {
    form.options.validate().map_err(|e| e.to_string())?;
    let doc = app.doc.as_ref().ok_or("No document is open.")?;
    let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name("New Imposition.pdf").save_file() else {
        return Err("No output file was chosen.".into());
    };
    // The sheets are protected like the document.
    let mut imposed = doc.pdf.impose(&form.options, None).map_err(|e| e.to_string())?;
    let bytes = doc.derived_bytes(&mut imposed).map_err(|e| e.to_string())?;
    carbide_engine::fs::write_replacing(&path, &bytes).map_err(|e| e.to_string())?;
    app.spawn_window(&path);
    Ok(())
}

// ---- reduce --------------------------------------------------------------------------------

/// Returns `false` when the dialog should close.
fn reduce_dialog(app: &mut CarbideApp, ctx: &egui::Context, form: &mut ReduceForm) -> bool {
    let mut keep = true;
    if let Some(results) = &form.results {
        window("Conversion Warnings").show(ctx, |ui| {
            let warnings: Vec<&FileOutcome> = results.iter().filter(|r| r.warning.is_some()).collect();
            if warnings.is_empty() {
                ui.label(format!("{} file(s) reduced.", results.len()));
            }
            for outcome in warnings {
                ui.label(egui::RichText::new(outcome.input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).strong());
                ui.label(format!("    {}", outcome.warning.clone().unwrap_or_default()));
            }
            if ui.button("OK").clicked() {
                keep = false;
            }
        });
        return keep;
    }

    window("Reduce File Size").show(ctx, |ui| {
        if !form.multi {
            ui.group(|ui| {
                ui.label("Image Settings");
                ui.horizontal(|ui| {
                    ui.label("Small size");
                    ui.add(egui::Slider::new(&mut form.slider, 0..=100).show_value(true));
                    ui.label("High Quality");
                });
            });
            ui.weak(reduce_messages::SLIDER_NOTE);
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    keep = !reduce_current(app, form.slider);
                }
                if ui.button("Apply to multiple").clicked() {
                    form.multi = true;
                }
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
            return;
        }

        ui.label("Add documents to the list below.");
        egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
            let mut remove = None;
            for (i, file) in form.files.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                    if let Ok(meta) = std::fs::metadata(file) {
                        ui.weak(format_size(meta.len()));
                    }
                    if ui.small_button("Delete").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                form.files.remove(i);
            }
        });
        if ui.button("Add Files...").clicked() {
            if let Some(paths) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).pick_files() {
                for path in paths {
                    if !form.files.contains(&path) {
                        form.files.push(path);
                    }
                }
            }
        }
        ui.separator();
        ui.label("Target Folder");
        ui.radio_value(&mut form.same_folder, true, "Next to the source files");
        ui.horizontal(|ui| {
            ui.radio_value(&mut form.same_folder, false, "Specific folder");
            if !form.same_folder && ui.button("Browse").clicked() {
                if let Some(folder) = rfd::FileDialog::new().set_title("Choose an output folder").pick_folder() {
                    form.folder = Some(folder);
                }
            }
            if let Some(folder) = &form.folder {
                ui.weak(folder.display().to_string());
            }
        });
        ui.label("File Naming");
        ui.radio_value(&mut form.add_to_names, false, "Use the same file names");
        ui.radio_value(&mut form.add_to_names, true, "Add text to the file names");
        if form.add_to_names {
            ui.horizontal(|ui| {
                ui.label("Insert Before");
                ui.text_edit_singleline(&mut form.prefix);
                ui.label("file name");
                ui.label("Insert After");
                ui.text_edit_singleline(&mut form.suffix);
            });
        }
        if !form.add_to_names {
            ui.checkbox(&mut form.replace_originals, "Replace the original files");
        }
        ui.checkbox(&mut form.overwrite, "Overwrite existing files");
        ui.horizontal(|ui| {
            if ui.add_enabled(!form.files.is_empty(), egui::Button::new("OK")).clicked() {
                let output = OutputOptions {
                    folder: if form.same_folder {
                        TargetFolder::SameAsOriginal
                    } else {
                        TargetFolder::Specific(form.folder.clone().unwrap_or_default())
                    },
                    prefix: if form.add_to_names { form.prefix.clone() } else { String::new() },
                    suffix: if form.add_to_names { form.suffix.clone() } else { String::new() },
                    overwrite: form.overwrite,
                    replace_originals: form.replace_originals && !form.add_to_names,
                };
                form.results = Some(reduce_files(&form.files, &output, &ReduceOptions { image_factor: form.slider }));
            }
            if ui.button("Cancel").clicked() {
                keep = false;
            }
        });
    });
    keep
}

/// Single-document flow: choose the output path, reduce, report. Returns `true` when finished.
fn reduce_current(app: &mut CarbideApp, slider: u8) -> bool {
    let Some(doc) = app.doc.as_ref() else { return true };
    // The reduced copy is not encrypted, and its suggested name is the protected original's.
    if doc.saved.as_ref().is_some_and(|s| s.is_protected()) {
        app.alert("Reduce File Size is not available for password-protected documents.".into());
        return true;
    }
    // A new name by default, so the original is only replaced when the user picks it.
    let stem = doc.title().strip_suffix(".pdf").map(str::to_owned).unwrap_or_else(|| doc.title());
    let name = format!("{stem}{}.pdf", carbide_engine::reduce::DEFAULT_SUFFIX);
    let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name(name).save_file() else {
        return false;
    };
    let before = doc.saved.as_ref().map(|s| s.bytes().len());
    let mut copy = doc.pdf.clone();
    match copy.reduce(&ReduceOptions { image_factor: slider }, before) {
        Ok((bytes, report)) => match carbide_engine::fs::write_replacing(&path, &bytes) {
            Ok(()) => {
                app.status = format!(
                    "Reduced {} -> {} bytes ({} images re-encoded, {} streams merged).",
                    report.size_before, report.size_after, report.images_recompressed, report.streams_merged
                );
                true
            }
            Err(e) => {
                app.alert(format!("{}\n{e}", reduce_messages::SAVE_ERROR));
                true
            }
        },
        Err(e) => {
            app.alert(e.to_string());
            true
        }
    }
}

fn format_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1 << 20 => format!("{:.2} KB", b as f64 / 1024.0),
        b if b < 1 << 30 => format!("{:.2} MB", b as f64 / (1 << 20) as f64),
        b => format!("{:.2} GB", b as f64 / (1u64 << 30) as f64),
    }
}

// ---- auto recovery -------------------------------------------------------------------------

/// A question the Auto Recovery panel asks before it deletes recovered work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryQuestion {
    /// Delete the recovered copy of the document `name`.
    Delete { record: RecordId, name: String },
    /// Delete every recovered document.
    ClearAll,
}

impl RecoveryQuestion {
    pub fn message(&self) -> String {
        match self {
            RecoveryQuestion::Delete { name, .. } => autosave_messages::delete_recovered(name),
            RecoveryQuestion::ClearAll => autosave_messages::DELETE_ALL.to_string(),
        }
    }
}

/// What the user does in the Auto Recovery panel.
#[derive(Clone, Debug)]
pub enum RecoveryAction {
    Open(RecordId, RowKind),
    Delete(RecordId, RowKind),
    ClearAll,
    /// Yes to the panel's question.
    Confirm,
    /// No to the panel's question.
    Cancel,
}

/// Carries out `action` in the Auto Recovery panel, whose pending question is `question`. Deleting
/// a recovered copy, or all of them, only asks; it happens on `Confirm`. Returns whether the panel
/// stays open.
pub fn recovery_action(app: &mut CarbideApp, question: &mut Option<RecoveryQuestion>, action: RecoveryAction) -> bool {
    match action {
        RecoveryAction::Open(record, RowKind::Original) => {
            let path = app.autosave.as_ref().and_then(|s| s.record(record)).and_then(|r| r.original.as_ref()).map(|o| o.path.clone());
            match path {
                Some(path) if path.exists() => {
                    app.open_path(&path);
                    return false;
                }
                _ => {
                    if let Some(store) = app.autosave.as_mut() {
                        let _ = store.remove_original(record);
                    }
                    app.alert("The original file no longer exists.".into());
                }
            }
        }
        RecoveryAction::Open(record, RowKind::Recovered) => {
            app.open_recovered(record);
            return false;
        }
        // The original row only lists the file; the file itself is never touched.
        RecoveryAction::Delete(record, RowKind::Original) => {
            if let Some(store) = app.autosave.as_mut() {
                let _ = store.remove_original(record);
            }
        }
        RecoveryAction::Delete(record, RowKind::Recovered) => {
            let name = app.autosave.as_ref().and_then(|s| s.record(record)).map(|r| r.display_name()).unwrap_or_default();
            *question = Some(RecoveryQuestion::Delete { record, name });
        }
        RecoveryAction::ClearAll => *question = Some(RecoveryQuestion::ClearAll),
        RecoveryAction::Confirm => match question.take() {
            Some(RecoveryQuestion::Delete { record, .. }) => {
                if let Some(store) = app.autosave.as_mut() {
                    let _ = store.remove_recovered(record);
                }
            }
            Some(RecoveryQuestion::ClearAll) => {
                if let Some(store) = app.autosave.as_mut() {
                    let _ = store.remove_all_loaded();
                }
            }
            None => {}
        },
        RecoveryAction::Cancel => *question = None,
    }
    true
}

/// Returns `false` when the panel should close.
fn recovery_panel(app: &mut CarbideApp, ctx: &egui::Context, question: &mut Option<RecoveryQuestion>) -> bool {
    let mut keep = true;
    let mut action: Option<RecoveryAction> = None;

    if let Some(asked) = question.clone() {
        window(autosave_messages::PANEL_TITLE).show(ctx, |ui| {
            ui.set_max_width(400.0);
            ui.label(asked.message());
            ui.weak("This cannot be undone.");
            let (ok, cancel) = buttons(ui, "Delete", true);
            if ok {
                action = Some(RecoveryAction::Confirm);
            }
            if cancel {
                action = Some(RecoveryAction::Cancel);
            }
        });
        return match action {
            Some(action) => recovery_action(app, question, action),
            None => keep,
        };
    }

    let rows = app.autosave.as_ref().map(|s| s.panel_rows()).unwrap_or_default();
    window(autosave_messages::PANEL_TITLE).show(ctx, |ui| {
        if rows.is_empty() {
            ui.label("There are no recovered documents.");
        }
        egui::Grid::new("recovery").striped(true).show(ui, |ui| {
            ui.strong("Document");
            ui.strong("Last saved by");
            ui.strong("Saved");
            ui.end_row();
            for row in &rows {
                ui.label(&row.name).on_hover_text(row.tooltip);
                ui.label(row.last_saved);
                ui.label(format_time(row.save_time));
                if ui.button("Open").clicked() {
                    action = Some(RecoveryAction::Open(row.record, row.kind));
                }
                if ui.button("Delete").clicked() {
                    action = Some(RecoveryAction::Delete(row.record, row.kind));
                }
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(!rows.is_empty(), egui::Button::new(autosave_messages::CLEAR_ALL)).clicked() {
                action = Some(RecoveryAction::ClearAll);
            }
            if ui.button("Close").clicked() {
                keep = false;
            }
        });
    });
    match action {
        Some(action) => recovery_action(app, question, action) && keep,
        None => keep,
    }
}

/// The time a document time stamp gives, in local time.
fn time_stamp_time(report: &SignatureReport) -> Option<String> {
    let time = report.signing_time.as_deref().filter(|_| report.document_time_stamp)?;
    Some(carbide_engine::revisions::pdf_date_to_local(time)).filter(|t| !t.is_empty())
}
