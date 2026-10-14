//! The application shell, laid out after the original product's window: a quick-access strip and a
//! centred tab menu on top, document tabs, a ribbon of command groups, a Start screen with Quick
//! Launch and recent files, side panels, and a navigation bar along the bottom.
//!
//! Everything here is drawn with the painter so that spacing and colours stay under our control;
//! the actual work is done by the methods on `CarbideApp` that `run` dispatches to.

use std::path::{Path, PathBuf};

use carbide_engine::AnnotationSummary;
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, FontId, Frame, Key, Margin, Modifiers, Rect, Sense, Stroke, StrokeKind, Ui,
};

use crate::dialogs::{AfterSave, Dialog, Export};
use crate::icons::{self, Glyph};
use crate::ribbon::{self, Cmd, RibbonTab};
use crate::{paths, theme, CarbideApp, Tool};

const DISABLED: Color32 = Color32::from_gray(105);
const ZOOM_100_PERCENT: f32 = 96.0 / 72.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Manual,
    Width,
    Page,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidePanel {
    Pages,
    Comments,
    Signatures,
}

#[derive(Clone, PartialEq, Eq)]
enum StartView {
    Recent,
    Folder(PathBuf),
}

#[derive(Clone)]
struct StartRow {
    path: PathBuf,
    name: String,
    /// Second line under the name: "PDF" for files, the location for folders.
    subtitle: String,
    is_folder: bool,
    time: i64,
    size: Option<u64>,
}

/// View state that belongs to the shell rather than to a document.
pub struct ShellState {
    ribbon_tab: RibbonTab,
    pub fit: Fit,
    find: String,
    find_hover: bool,
    sidebar_open: bool,
    side_panel: SidePanel,
    start_view: StartView,
    folders_tab: bool,
    start_rows: Option<Vec<StartRow>>,
    page_text: String,
    /// The document whose close question is up. A document, not a tab: closing other tabs while
    /// the question is up moves the tabs along.
    pending_close: Option<u64>,
    locations: Vec<PathBuf>,
    comments: Option<((Option<usize>, u64), Vec<AnnotationSummary>)>,
    window_title: String,
    /// Asking what to do with unsaved documents before the window closes.
    quit_prompt: bool,
    /// Save All was chosen and is waiting for the answer to a save question.
    save_all_pending: bool,
    quit_confirmed: bool,
    /// The Add Location window, holding the cloud folders found on this computer.
    add_location: Option<Vec<(String, PathBuf)>>,
}

impl ShellState {
    pub fn new() -> Self {
        Self {
            ribbon_tab: RibbonTab::Home,
            fit: Fit::Width,
            find: String::new(),
            find_hover: false,
            sidebar_open: true,
            side_panel: SidePanel::Pages,
            start_view: StartView::Recent,
            folders_tab: false,
            start_rows: None,
            page_text: String::new(),
            pending_close: None,
            locations: paths::load_locations(),
            comments: None,
            window_title: String::new(),
            quit_prompt: false,
            save_all_pending: false,
            quit_confirmed: false,
            add_location: None,
        }
    }

    /// Carries on with Save All and quitting, after a save question was answered.
    pub fn resume_save_all(&mut self) {
        self.quit_prompt = true;
        self.save_all_pending = true;
    }

    /// Drops everything derived from the active document.
    pub fn forget_caches(&mut self) {
        self.comments = None;
        self.start_rows = None;
    }

    pub fn forget_start_rows(&mut self) {
        self.start_rows = None;
    }
}

// ---- small drawing helpers ---------------------------------------------------------------

fn text_width(ui: &Ui, text: &str, font: &FontId) -> f32 {
    ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x
}

fn elide(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let head: String = text.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

/// A square icon button drawn into `rect`. Returns whether it was clicked.
fn icon_button(ui: &mut Ui, rect: Rect, glyph: Glyph, id: impl std::hash::Hash + std::fmt::Debug, enabled: bool, selected: bool, tip: &str) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.id().with(id), sense);
    let hovered = enabled && response.hovered();
    if selected {
        ui.painter().rect_filled(rect.shrink(2.0), 4.0, theme::SIDEBAR_SELECTED);
    } else if hovered {
        ui.painter().rect_filled(rect.shrink(2.0), 4.0, Color32::from_gray(84));
    }
    let color = if enabled { theme::TEXT } else { DISABLED };
    icons::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), glyph, color);
    let clicked = enabled && response.clicked();
    if !tip.is_empty() {
        response.on_hover_text(tip);
    }
    clicked
}

/// A large ribbon button: icon over label. Returns whether it was clicked.
fn command_button(ui: &mut Ui, rect: Rect, cmd: Cmd, index: usize, enabled: bool, selected: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.id().with(("cmd", index)), sense);
    let hovered = enabled && response.hovered();
    if selected {
        ui.painter().rect_filled(rect, 5.0, theme::SIDEBAR_SELECTED);
    } else if hovered {
        ui.painter().rect_filled(rect, 5.0, Color32::from_gray(70));
    }
    let color = if enabled { theme::TEXT } else { DISABLED };
    icons::paint(ui.painter(), Rect::from_center_size(pos2(rect.center().x, rect.min.y + 20.0), vec2(26.0, 26.0)), cmd.glyph(), color);
    ui.painter().text(pos2(rect.center().x, rect.max.y - 11.0), Align2::CENTER_CENTER, cmd.label(), FontId::proportional(11.5), color);
    let clicked = enabled && response.clicked();
    let tip = cmd.tooltip();
    if !tip.is_empty() {
        response.on_hover_text(tip);
    }
    clicked
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes}B")
    } else if b < KB * KB {
        format!("{:.1}KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1}MB", b / KB / KB)
    } else {
        format!("{:.1}GB", b / KB / KB / KB)
    }
}

fn format_time(secs: i64) -> String {
    use chrono::{DateTime, Local, Utc};
    let Some(utc) = DateTime::<Utc>::from_timestamp(secs, 0) else { return String::new() };
    let local = utc.with_timezone(&Local);
    if local.date_naive() == Local::now().date_naive() {
        local.format("%-I:%M %p").to_string()
    } else {
        local.format("%b %-d, %Y").to_string()
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn comment_kind(subtype: &str) -> &str {
    match subtype {
        "Text" => "Note",
        "FreeText" => "Text box",
        "StrikeOut" => "Strikeout",
        other => other,
    }
}

/// An answer to the question whether to save a document that is being closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseChoice {
    Save,
    Discard,
    Cancel,
}

// ---- commands ----------------------------------------------------------------------------

impl CarbideApp {
    pub fn cmd_enabled(&self, cmd: Cmd) -> bool {
        let doc = self.doc.as_ref();
        match cmd {
            Cmd::Open
            | Cmd::NewBlank
            | Cmd::Combine
            | Cmd::Reduce
            | Cmd::Protect
            | Cmd::AutoSaveSettings
            | Cmd::AutoRecovery
            | Cmd::TrustCertificate
            | Cmd::Quit => true,
            Cmd::Undo => doc.is_some_and(|d| !d.undo.is_empty()),
            Cmd::Redo => doc.is_some_and(|d| !d.redo.is_empty()),
            Cmd::ApplyRedactions => doc.is_some() && !self.pending_redactions.is_empty(),
            Cmd::Ocr => doc.is_some() && self.ocr_job.is_none(),
            Cmd::SignVisible | Cmd::SignInvisible | Cmd::TimeStamp => doc.is_some() && self.sign_job.is_none(),
            Cmd::OpenRevision => doc.is_some_and(|d| d.pdf.can_open_revision()),
            Cmd::FirstPage | Cmd::PrevPage => doc.is_some() && self.current_page > 0,
            Cmd::NextPage | Cmd::LastPage => doc.is_some_and(|d| self.current_page + 1 < d.pdf.page_count()),
            _ => doc.is_some(),
        }
    }

    fn cmd_selected(&self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::SelectTool => self.tool == Tool::Select,
            Cmd::Typewriter => self.tool == Tool::TextBox,
            Cmd::Note => self.tool == Tool::Note,
            Cmd::Highlight => self.tool == Tool::Highlight,
            Cmd::HighlightArea => self.tool == Tool::HighlightArea,
            Cmd::Underline => self.tool == Tool::Underline,
            Cmd::Strikeout => self.tool == Tool::StrikeOut,
            Cmd::MarkRedaction => self.tool == Tool::Redact,
            Cmd::SignVisible => self.tool == Tool::Sign,
            Cmd::ToggleSidebar => self.shell.sidebar_open,
            Cmd::FitWidth => self.shell.fit == Fit::Width,
            Cmd::FitPage => self.shell.fit == Fit::Page,
            _ => false,
        }
    }

    fn pick_and_open(&mut self) {
        if let Some(files) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).pick_files() {
            for file in files {
                self.open_path(&file);
            }
        }
    }

    fn goto_page(&mut self, page: usize) {
        let Some(doc) = self.doc.as_ref() else { return };
        let page = page.min(doc.pdf.page_count().saturating_sub(1));
        self.current_page = page;
        self.scroll_to = Some(page);
    }

    fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(0.25, 6.0);
        self.shell.fit = Fit::Manual;
    }

    /// Runs a ribbon command. Commands that need a document do nothing without one.
    pub fn run(&mut self, cmd: Cmd) {
        if !self.cmd_enabled(cmd) {
            return;
        }
        let page = self.current_page;
        // The document the dialogs opened below act on. Commands that need one are only enabled
        // with one in front.
        let doc = self.doc.as_ref().map(|d| d.id).unwrap_or_default();
        match cmd {
            Cmd::Open => self.pick_and_open(),
            Cmd::Save => self.save(),
            Cmd::SaveAs => self.save_as(),
            Cmd::Undo => self.undo(),
            Cmd::Redo => self.redo(),
            Cmd::SelectTool => self.tool = Tool::Select,
            Cmd::ToggleSidebar => self.shell.sidebar_open = !self.shell.sidebar_open,
            Cmd::NewBlank => self.new_blank(),
            Cmd::Combine => self.combine_files(),

            Cmd::PageInsert => self.edit("Insert page", move |p| p.insert_blank_page(page + 1, None)),
            Cmd::PageExtract => self.extract_current(),
            Cmd::PageDelete => self.edit_removing("Delete page", move |p| p.delete_pages(&[page])),
            Cmd::Merge => self.merge_files(),
            Cmd::Split => self.open_dialog(Dialog::Split { doc, ranges: "1-2".into() }),
            Cmd::Imposition => self.open_dialog(Dialog::impose(doc)),
            Cmd::RotateLeft => self.edit("Rotate left", move |p| p.rotate_pages(&[page], -90)),
            Cmd::RotateRight => self.edit("Rotate right", move |p| p.rotate_pages(&[page], 90)),
            Cmd::MoveUp => {
                self.edit("Move page up", move |p| if page > 0 { p.move_page(page, page - 1) } else { Ok(()) });
                if page > 0 {
                    self.goto_page(page - 1);
                }
            }
            Cmd::MoveDown => {
                self.edit("Move page down", move |p| if page + 1 < p.page_count() { p.move_page(page, page + 1) } else { Ok(()) });
                self.goto_page(page + 1);
            }

            Cmd::Typewriter => self.tool = Tool::TextBox,
            Cmd::Watermark => self.open_dialog(Dialog::watermark(doc)),
            Cmd::Bates => self.open_dialog(Dialog::bates(doc)),

            Cmd::ZoomIn => self.set_zoom(self.zoom * 1.2),
            Cmd::ZoomOut => self.set_zoom(self.zoom / 1.2),
            Cmd::FitWidth => self.shell.fit = Fit::Width,
            Cmd::FitPage => self.shell.fit = Fit::Page,
            Cmd::ActualSize => self.set_zoom(ZOOM_100_PERCENT),
            Cmd::FirstPage => self.goto_page(0),
            Cmd::PrevPage => self.goto_page(page.saturating_sub(1)),
            Cmd::NextPage => self.goto_page(page + 1),
            Cmd::LastPage => self.goto_page(usize::MAX),

            Cmd::Note => self.tool = Tool::Note,
            Cmd::Highlight => self.tool = Tool::Highlight,
            Cmd::HighlightArea => self.tool = Tool::HighlightArea,
            Cmd::Underline => self.tool = Tool::Underline,
            Cmd::Strikeout => self.tool = Tool::StrikeOut,

            Cmd::Reduce => {
                let current = self.doc.as_ref().and_then(|d| d.path.clone());
                self.open_dialog(Dialog::reduce(self.doc.as_ref().map(|d| d.id), current));
            }
            Cmd::Compare => self.compare_with(),
            Cmd::Ocr => self.start_ocr(),
            Cmd::ExportImages => self.export(Export::Images),
            Cmd::ExportText => self.export(Export::Text),
            Cmd::SaveRevision => self.open_dialog(Dialog::SaveRevision { doc, label: String::new(), error: None }),
            Cmd::OpenRevision => {
                let items = self.doc.as_ref().map(|d| d.pdf.revisions()).unwrap_or_default();
                self.open_dialog(Dialog::OpenRevision { doc, items, selected: None });
            }

            Cmd::MarkRedaction => self.tool = Tool::Redact,
            Cmd::ApplyRedactions => self.apply_redactions(),
            Cmd::RemoveHidden => self.open_dialog(Dialog::remove_hidden(doc)),
            Cmd::SignVisible => {
                self.tool = Tool::Sign;
                self.status = "Drag a rectangle on the page to place the signature.".into();
            }
            Cmd::SignInvisible => self.open_dialog(Dialog::sign(doc, None, false)),
            Cmd::Certify => self.open_dialog(Dialog::sign(doc, None, true)),
            Cmd::TimeStamp => self.open_dialog(Dialog::timestamp(doc)),
            Cmd::Verify => self.validate_signatures(),
            Cmd::Protect => {
                if self.doc.is_none() {
                    self.pick_and_open();
                }
                if let Some(doc) = self.doc.as_ref().map(|d| d.id) {
                    self.open_dialog(Dialog::protect(doc));
                }
            }
            Cmd::TrustCertificate => {
                if let Some(path) = rfd::FileDialog::new().add_filter("Certificates", &["pem", "crt", "cer", "der"]).pick_file() {
                    match paths::trust_certificate_file(&path) {
                        Ok(n) => self.status = format!("Added {n} trusted certificate(s)."),
                        Err(e) => self.alert(format!("Could not add the certificate: {e}")),
                    }
                }
            }

            Cmd::AutoSaveSettings => self.open_dialog(Dialog::preferences(&self.settings)),
            Cmd::AutoRecovery => self.open_dialog(Dialog::auto_recovery()),
            Cmd::Quit => {} // Handled by the window itself.
        }
    }

    pub fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let (new, open, save, close, redo, undo, zoom_in, zoom_out) = ctx.input_mut(|i| {
            let command = Modifiers::COMMAND;
            (
                i.consume_key(command, Key::N),
                i.consume_key(command, Key::O),
                i.consume_key(command, Key::S),
                i.consume_key(command, Key::W),
                !typing && i.consume_key(command | Modifiers::SHIFT, Key::Z),
                !typing && i.consume_key(command, Key::Z),
                i.consume_key(command, Key::Equals),
                i.consume_key(command, Key::Minus),
            )
        });
        if new {
            self.run(Cmd::NewBlank);
        }
        if open {
            self.run(Cmd::Open);
        }
        if save {
            self.run(Cmd::Save);
        }
        if close {
            if let Some(index) = self.active_tab {
                self.request_close(index);
            }
        }
        if redo {
            self.run(Cmd::Redo);
        }
        if undo {
            self.run(Cmd::Undo);
        }
        if zoom_in {
            self.run(Cmd::ZoomIn);
        }
        if zoom_out {
            self.run(Cmd::ZoomOut);
        }
    }

    // ---- tabs ------------------------------------------------------------------------------

    fn tab_label(&self, index: usize) -> (String, bool) {
        self.doc_at(index).map(|d| (d.title(), d.dirty)).unwrap_or_else(|| ("Untitled".into(), false))
    }

    /// Closes a tab, asking first when the document has unsaved changes.
    pub fn request_close(&mut self, index: usize) {
        if self.tab_label(index).1 {
            self.shell.pending_close = self.doc_at(index).map(|d| d.id);
        } else {
            self.close_tab(index);
        }
    }

    /// The document the close question is about, while it is up.
    pub fn pending_close(&self) -> Option<u64> {
        self.shell.pending_close
    }

    /// The "save changes?" question shown when a modified document is closed.
    pub fn close_prompt(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.shell.pending_close else { return };
        let Some(index) = self.tab_of(doc) else {
            self.shell.pending_close = None;
            return;
        };
        let title = self.tab_label(index).0;
        let mut choice = None;
        egui::Window::new("Carbide PDF").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(format!("Do you want to save the changes you made to \"{title}\"?"));
            ui.weak("Your changes will be lost if you don't save them.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(CloseChoice::Save);
                }
                if ui.button("Don't Save").clicked() {
                    choice = Some(CloseChoice::Discard);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(CloseChoice::Cancel);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            choice = Some(CloseChoice::Cancel);
        }
        if let Some(choice) = choice {
            self.answer_close(choice);
        }
    }

    /// Carries out the answer to the close question, on the document it is about, wherever its tab
    /// is now. Nothing happens once that document is no longer open.
    pub fn answer_close(&mut self, choice: CloseChoice) {
        let Some(doc) = self.shell.pending_close.take() else { return };
        let Some(index) = self.tab_of(doc) else { return };
        match choice {
            CloseChoice::Save => {
                self.activate(Some(index));
                // The tab closes once the document is saved, after the answer when saving asks
                // first. A cancelled Save As leaves it modified and open.
                self.save_then(AfterSave::CloseTab);
            }
            CloseChoice::Discard => self.close_tab(index),
            CloseChoice::Cancel => {}
        }
    }

    /// Everything that can pop up over the window: close and quit questions, Add Location.
    pub fn prompts(&mut self, ctx: &egui::Context) {
        self.close_prompt(ctx);
        self.quit_prompt_window(ctx);
        self.add_location_window(ctx);
    }

    /// Called every frame: intercepts the window's close button / Cmd+Q when work would be lost.
    pub fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.shell.quit_confirmed {
            return;
        }
        if (0..self.tabs.len()).any(|i| self.tab_label(i).1) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.shell.quit_prompt = true;
        } else {
            self.release_all_documents();
        }
    }

    /// Drops every open document, and with it its Auto Recovery record.
    fn release_all_documents(&mut self) {
        self.park_active();
        for slot in self.tabs.drain(..) {
            if let (Some(doc), Some(store)) = (slot, self.autosave.as_mut()) {
                if let Some(id) = doc.record {
                    let _ = store.document_closed(id);
                }
            }
        }
        self.active_tab = None;
    }

    fn quit_prompt_window(&mut self, ctx: &egui::Context) {
        if !self.shell.quit_prompt {
            return;
        }
        let dirty: Vec<String> = (0..self.tabs.len())
            .filter_map(|i| {
                let (title, modified) = self.tab_label(i);
                modified.then_some(title)
            })
            .collect();
        if dirty.is_empty() {
            self.shell.quit_prompt = false;
            // The last document was saved after answering its question: carry on quitting.
            if std::mem::take(&mut self.shell.save_all_pending) {
                self.finish_quit(ctx);
            }
            return;
        }
        if std::mem::take(&mut self.shell.save_all_pending) {
            return self.save_all_and_quit(ctx);
        }
        enum Choice {
            SaveAll,
            Discard,
            Cancel,
        }
        let mut choice = None;
        egui::Window::new("Carbide PDF").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            if let [only] = dirty.as_slice() {
                ui.label(format!("Do you want to save the changes you made to \"{only}\"?"));
            } else {
                ui.label(format!("{} documents have unsaved changes:", dirty.len()));
                for title in &dirty {
                    ui.label(format!("  {title}"));
                }
            }
            ui.weak("Your changes will be lost if you don't save them.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(if dirty.len() == 1 { "Save" } else { "Save All" }).clicked() {
                    choice = Some(Choice::SaveAll);
                }
                if ui.button("Don't Save").clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(Choice::Cancel);
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            choice = Some(Choice::Cancel);
        }
        match choice {
            Some(Choice::SaveAll) => self.save_all_and_quit(ctx),
            Some(Choice::Discard) => self.finish_quit(ctx),
            Some(Choice::Cancel) => self.shell.quit_prompt = false,
            None => {}
        }
    }

    /// Saves every modified document, then quits. Documents that save without a question go
    /// first; then the first one that has to ask does, and quitting carries on once it is answered.
    pub(crate) fn save_all_and_quit(&mut self, ctx: &egui::Context) {
        // Followed by document, not by tab: a save can close another tab on the same file.
        let modified: Vec<u64> =
            (0..self.tabs.len()).filter(|&i| self.tab_label(i).1).filter_map(|i| self.doc_at(i).map(|d| d.id)).collect();
        let mut asks = None;
        for id in modified {
            let Some(index) = self.tab_of(id) else { continue };
            self.activate(Some(index));
            if self.save_needs_question() {
                asks.get_or_insert(id);
            } else {
                self.save();
            }
        }
        // A save that failed has said why; the quit prompt stays up for another try.
        if self.dialog.is_some() {
            return;
        }
        if let Some(index) = asks.and_then(|id| self.tab_of(id)) {
            self.shell.quit_prompt = false;
            self.activate(Some(index));
            return self.save_then(AfterSave::Quit);
        }
        // A cancelled Save As leaves a document modified: keep the prompt up then.
        if !(0..self.tabs.len()).any(|i| self.tab_label(i).1) {
            self.finish_quit(ctx);
        }
    }

    fn finish_quit(&mut self, ctx: &egui::Context) {
        self.shell.quit_prompt = false;
        self.shell.quit_confirmed = true;
        self.release_all_documents();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn add_location_window(&mut self, ctx: &egui::Context) {
        let Some(found) = self.shell.add_location.clone() else { return };
        let mut add: Option<PathBuf> = None;
        let mut browse = false;
        let mut close = false;
        egui::Window::new("Add Location").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_max_width(480.0);
            if found.is_empty() {
                ui.label("No synced cloud folders were found on this computer.");
            } else {
                ui.label("Cloud folders found on this computer:");
                ui.add_space(4.0);
                for (label, path) in &found {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        ui.label(egui::RichText::new(elide(&path.display().to_string(), 46)).color(theme::TEXT_DIM));
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            let added = self.shell.locations.contains(path);
                            if ui.add_enabled(!added, egui::Button::new(if added { "Added" } else { "Add" })).clicked() {
                                add = Some(path.clone());
                            }
                        });
                    });
                }
            }
            ui.add_space(6.0);
            ui.add(egui::Label::new(
                egui::RichText::new(
                    "These folders are kept in sync by each service's own desktop app. Carbide PDF reads and \
                     saves files there like in any other folder; it does not sign in to the service.",
                )
                .color(theme::TEXT_DIM),
            ));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Choose Folder...").clicked() {
                    browse = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if browse {
            add = rfd::FileDialog::new().set_title("Add Location").pick_folder();
            close = add.is_some() || close;
        }
        if let Some(dir) = add {
            if !self.shell.locations.contains(&dir) {
                self.shell.locations.push(dir.clone());
                paths::save_locations(&self.shell.locations);
            }
            self.shell.start_view = StartView::Folder(dir);
            self.shell.start_rows = None;
            close = true;
        }
        if close {
            self.shell.add_location = None;
        }
    }

    // ---- layout ----------------------------------------------------------------------------

    pub fn draw_shell(&mut self, ui: &mut Ui) {
        self.sync_window_title(ui.ctx());
        let in_document = self.doc.is_some();

        egui::Panel::top("top-bar")
            .exact_size(40.0)
            .show_separator_line(false)
            .frame(Frame::new().fill(theme::TOP_BAR))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::top("document-tabs")
            .exact_size(30.0)
            .show_separator_line(false)
            .frame(Frame::new().fill(theme::TAB_STRIP))
            .show(ui, |ui| self.document_tabs(ui));
        if in_document {
            egui::Panel::top("ribbon")
                .exact_size(88.0)
                .show_separator_line(false)
                .frame(Frame::new().fill(theme::RIBBON))
                .show(ui, |ui| self.ribbon_strip(ui));
            if let Some(text) = self.notice.clone() {
                egui::Panel::top("notice")
                    .show_separator_line(false)
                    .frame(Frame::new().fill(theme::NOTICE).inner_margin(Margin::symmetric(12, 5)))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(text).color(Color32::from_rgb(190, 210, 255)));
                            if ui.small_button("Signatures").clicked() {
                                self.validate_signatures();
                            }
                        });
                    });
            }
        }
        egui::Panel::bottom("bottom-bar")
            .exact_size(30.0)
            .show_separator_line(false)
            .frame(Frame::new().fill(theme::BOTTOM_BAR))
            .show(ui, |ui| self.bottom_bar(ui));

        if in_document {
            if self.shell.sidebar_open {
                egui::Panel::left("sidebar")
                    .default_size(190.0)
                    .size_range(150.0..=360.0)
                    .resizable(true)
                    .show_separator_line(false)
                    .frame(Frame::new().fill(theme::SIDEBAR).inner_margin(Margin::symmetric(8, 6)))
                    .show(ui, |ui| self.sidebar(ui));
            }
            egui::CentralPanel::default().frame(Frame::new().fill(theme::CONTENT)).show(ui, |ui| self.page_view(ui));
        } else {
            egui::Panel::left("start-sidebar")
                .exact_size(260.0)
                .show_separator_line(false)
                .frame(Frame::new().fill(theme::SIDEBAR).inner_margin(Margin::symmetric(14, 26)))
                .show(ui, |ui| self.start_sidebar(ui));
            egui::CentralPanel::default()
                .frame(Frame::new().fill(theme::CONTENT).inner_margin(Margin::symmetric(20, 14)))
                .show(ui, |ui| self.start_main(ui));
        }
    }

    fn sync_window_title(&mut self, ctx: &egui::Context) {
        let title = match self.doc.as_ref() {
            Some(d) => format!("{}{} - Carbide PDF", d.title(), if d.dirty { " *" } else { "" }),
            None => "Start - Carbide PDF".to_string(),
        };
        if title != self.shell.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.shell.window_title = title;
        }
    }

    // ---- top bar: quick access, tab menu, Find a Tool ----------------------------------------

    fn top_bar(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let mut fired: Option<Cmd> = None;

        // Quick access strip.
        let quick = [
            (Glyph::Sidebar, Cmd::ToggleSidebar),
            (Glyph::Open, Cmd::Open),
            (Glyph::Save, Cmd::Save),
            (Glyph::Cursor, Cmd::SelectTool),
            (Glyph::Undo, Cmd::Undo),
            (Glyph::Redo, Cmd::Redo),
        ];
        let strip = Rect::from_min_size(pos2(rect.min.x + 8.0, rect.min.y + 6.0), vec2(quick.len() as f32 * 30.0 + 6.0, 28.0));
        ui.painter().rect_filled(strip, 5.0, Color32::from_rgb(70, 70, 70));
        for (i, (glyph, cmd)) in quick.iter().enumerate() {
            let r = Rect::from_min_size(pos2(strip.min.x + 3.0 + i as f32 * 30.0, strip.min.y + 1.0), vec2(30.0, 26.0));
            if icon_button(ui, r, *glyph, ("quick", i), self.cmd_enabled(*cmd), self.cmd_selected(*cmd), cmd.label()) {
                fired = Some(*cmd);
            }
        }

        // The tab menu, centred in the window.
        let font = FontId::proportional(13.5);
        let widths: Vec<f32> = RibbonTab::ALL.iter().map(|t| text_width(ui, t.label(), &font) + 22.0).collect();
        let total: f32 = widths.iter().sum();
        let left = (rect.center().x - total / 2.0).max(strip.max.x + 14.0);
        let cluster = Rect::from_min_size(pos2(left, rect.min.y + 6.0), vec2(total, 28.0));
        ui.painter().rect_filled(cluster, 5.0, Color32::from_rgb(52, 52, 52));
        ui.painter().rect_stroke(cluster, 5.0, Stroke::new(1.0, theme::HAIRLINE), StrokeKind::Inside);
        let mut x = cluster.min.x;
        for (i, tab) in RibbonTab::ALL.iter().enumerate() {
            let r = Rect::from_min_size(pos2(x, cluster.min.y), vec2(widths[i], cluster.height()));
            let response = ui.interact(r, ui.id().with(("ribbon-tab", i)), Sense::click());
            if self.shell.ribbon_tab == *tab {
                ui.painter().rect_filled(r.shrink(2.0), 4.0, theme::TAB_ACTIVE);
            } else if response.hovered() {
                ui.painter().rect_filled(r.shrink(2.0), 4.0, Color32::from_gray(68));
            }
            ui.painter().text(r.center(), Align2::CENTER_CENTER, tab.label(), font.clone(), theme::TEXT);
            if i + 1 < RibbonTab::ALL.len() {
                ui.painter().line_segment(
                    [pos2(r.max.x, r.min.y + 8.0), pos2(r.max.x, r.max.y - 8.0)],
                    Stroke::new(1.0, theme::HAIRLINE),
                );
            }
            if response.clicked() {
                self.shell.ribbon_tab = *tab;
            }
            x += widths[i];
        }

        // Find a Tool.
        let find_width = ((rect.max.x - 12.0) - (cluster.max.x + 14.0)).clamp(110.0, 280.0);
        let find = Rect::from_min_max(pos2(rect.max.x - 12.0 - find_width, rect.min.y + 6.0), pos2(rect.max.x - 12.0, rect.max.y - 6.0));
        ui.painter().rect_filled(find, 5.0, Color32::from_rgb(30, 30, 30));
        ui.painter().rect_stroke(find, 5.0, Stroke::new(1.0, theme::HAIRLINE), StrokeKind::Inside);
        icons::paint(
            ui.painter(),
            Rect::from_center_size(pos2(find.min.x + 15.0, find.center().y), vec2(14.0, 14.0)),
            Glyph::Search,
            theme::TEXT_DIM,
        );
        let edit_rect = Rect::from_center_size(pos2((find.min.x + 28.0 + find.max.x - 6.0) / 2.0, find.center().y), vec2(find.width() - 34.0, 20.0));
        let edit = egui::TextEdit::singleline(&mut self.shell.find)
            .hint_text("Find a Tool")
            .frame(Frame::NONE)
            .desired_width(edit_rect.width());
        let response = ui.put(edit_rect, edit);
        fired = fired.or(self.find_results(ui, &response, find));

        if let Some(cmd) = fired {
            self.run(cmd);
        }
    }

    /// The drop-down of matching commands under the Find a Tool box.
    fn find_results(&mut self, ui: &mut Ui, field: &egui::Response, anchor: Rect) -> Option<Cmd> {
        if field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.shell.find.clear();
            field.surrender_focus();
        }
        let query = self.shell.find.trim().to_lowercase();
        if query.is_empty() || !(field.has_focus() || self.shell.find_hover) {
            self.shell.find_hover = false;
            return None;
        }
        let matches: Vec<Cmd> = ribbon::all_commands().into_iter().filter(|c| c.label().to_lowercase().contains(&query)).take(8).collect();
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let mut chosen: Option<Cmd> = None;
        if enter {
            chosen = matches.iter().copied().find(|c| self.cmd_enabled(*c));
        }
        let area = egui::Area::new(egui::Id::new("find-results"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(anchor.min.x, anchor.max.y + 4.0))
            .show(ui.ctx(), |ui| {
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(anchor.width() - 16.0);
                    if matches.is_empty() {
                        ui.weak("No matching tool.");
                    }
                    for cmd in &matches {
                        let enabled = self.cmd_enabled(*cmd);
                        let button = egui::Button::new(cmd.label()).frame(false).min_size(vec2(ui.available_width(), 22.0));
                        if ui.add_enabled(enabled, button).clicked() {
                            chosen = Some(*cmd);
                        }
                    }
                });
            });
        self.shell.find_hover = area.response.contains_pointer();
        if chosen.is_some() {
            self.shell.find.clear();
            self.shell.find_hover = false;
        }
        chosen
    }

    // ---- document tabs ---------------------------------------------------------------------

    fn document_tabs(&mut self, ui: &mut Ui) {
        enum TabAction {
            Show(Option<usize>),
            Close(usize),
            New,
        }
        let rect = ui.max_rect();
        let font = FontId::proportional(14.0);
        let mut action: Option<TabAction> = None;
        let mut x = rect.min.x;

        // The permanent Start tab.
        let start = Rect::from_min_size(pos2(x, rect.min.y), vec2(126.0, rect.height()));
        let response = ui.interact(start, ui.id().with("start-tab"), Sense::click());
        if self.active_tab.is_none() {
            ui.painter().rect_filled(start, 0.0, theme::TAB_ACTIVE);
        } else if response.hovered() {
            ui.painter().rect_filled(start, 0.0, Color32::from_gray(100));
        }
        ui.painter().text(pos2(start.min.x + 12.0, start.center().y), Align2::LEFT_CENTER, "Start", font.clone(), theme::TEXT);
        if response.clicked() {
            action = Some(TabAction::Show(None));
        }
        x = start.max.x + 1.0;

        for index in 0..self.tabs.len() {
            let (title, dirty) = self.tab_label(index);
            let label = elide(&format!("{}{}", title, if dirty { " *" } else { "" }), 26);
            let width = (text_width(ui, &label, &font) + 52.0).clamp(120.0, 260.0);
            let tab = Rect::from_min_size(pos2(x, rect.min.y), vec2(width, rect.height()));
            let response = ui.interact(tab, ui.id().with(("doc-tab", index)), Sense::click());
            let active = self.active_tab == Some(index);
            if active {
                ui.painter().rect_filled(tab, 0.0, theme::TAB_ACTIVE);
            } else if response.hovered() {
                ui.painter().rect_filled(tab, 0.0, Color32::from_gray(100));
            }
            ui.painter().text(pos2(tab.min.x + 12.0, tab.center().y), Align2::LEFT_CENTER, label, font.clone(), theme::TEXT);
            let close = Rect::from_center_size(pos2(tab.max.x - 16.0, tab.center().y), vec2(20.0, 20.0));
            let close_response = ui.interact(close, ui.id().with(("doc-tab-close", index)), Sense::click());
            if close_response.hovered() {
                ui.painter().rect_filled(close, 4.0, Color32::from_gray(140));
            }
            icons::paint(ui.painter(), Rect::from_center_size(close.center(), vec2(10.0, 10.0)), Glyph::Close, theme::TEXT);
            if close_response.clicked() || (response.middle_clicked() && !close_response.hovered()) {
                action = Some(TabAction::Close(index));
            } else if response.clicked() {
                action = Some(TabAction::Show(Some(index)));
            }
            x = tab.max.x + 1.0;
        }

        // New tab.
        let plus = Rect::from_min_size(pos2(x + 2.0, rect.min.y), vec2(32.0, rect.height()));
        if icon_button(ui, plus, Glyph::Plus, "new-tab", true, false, "Open a document") {
            action = Some(TabAction::New);
        }

        match action {
            Some(TabAction::Show(index)) => self.activate(index),
            Some(TabAction::Close(index)) => self.request_close(index),
            Some(TabAction::New) => self.run(Cmd::Open),
            None => {}
        }
    }

    // ---- ribbon ----------------------------------------------------------------------------

    fn ribbon_strip(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        ui.painter().line_segment([pos2(rect.min.x, rect.max.y - 0.5), pos2(rect.max.x, rect.max.y - 0.5)], Stroke::new(1.0, theme::HAIRLINE));
        let groups = ribbon::groups(self.shell.ribbon_tab);
        if groups.is_empty() {
            ui.painter().text(
                pos2(rect.min.x + 16.0, rect.center().y),
                Align2::LEFT_CENTER,
                "Form fields are not part of Carbide PDF yet.",
                FontId::proportional(13.0),
                theme::TEXT_DIM,
            );
            return;
        }
        let label_font = FontId::proportional(11.0);
        let mut x = rect.min.x + 12.0;
        let mut index = 0usize;
        let mut fired: Option<Cmd> = None;
        for group in groups {
            let start = x;
            let mut buttons_width = 0.0;
            for cmd in group.commands {
                let width = (text_width(ui, cmd.label(), &FontId::proportional(11.5)) + 16.0).max(54.0);
                let r = Rect::from_min_size(pos2(x, rect.min.y + 5.0), vec2(width, 58.0));
                if command_button(ui, r, *cmd, index, self.cmd_enabled(*cmd), self.cmd_selected(*cmd)) {
                    fired = Some(*cmd);
                }
                index += 1;
                x += width + 2.0;
                buttons_width += width + 2.0;
            }
            let label_width = text_width(ui, group.name, &label_font) + 16.0;
            if label_width > buttons_width {
                x = start + label_width;
            }
            ui.painter().text(
                pos2((start + x) / 2.0 - 1.0, rect.max.y - 11.0),
                Align2::CENTER_CENTER,
                group.name,
                label_font.clone(),
                theme::TEXT_DIM,
            );
            x += 6.0;
            ui.painter().line_segment([pos2(x, rect.min.y + 8.0), pos2(x, rect.max.y - 8.0)], Stroke::new(1.0, theme::HAIRLINE));
            x += 10.0;
        }
        if let Some(cmd) = fired {
            self.run(cmd);
        }
    }

    // ---- bottom bar ------------------------------------------------------------------------

    fn bottom_bar(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let has_doc = self.doc.is_some();
        let page_count = self.doc.as_ref().map(|d| d.pdf.page_count()).unwrap_or(0);
        let mut fired: Option<Cmd> = None;

        // Controls cluster, centred.
        let width = 26.0 * 11.0 + 76.0 + 66.0 + 18.0;
        let mut x = (rect.center().x - width / 2.0).max(rect.min.x + 12.0);
        let top = rect.min.y + 2.0;
        let mut button = |ui: &mut Ui, x: &mut f32, glyph: Glyph, cmd: Cmd, tip: &str| {
            let r = Rect::from_min_size(pos2(*x, top), vec2(26.0, 26.0));
            *x += 26.0;
            if icon_button(ui, r, glyph, ("bottom", cmd), self.cmd_enabled(cmd), self.cmd_selected(cmd), tip) {
                fired = Some(cmd);
            }
        };
        let cluster_start = x;
        button(ui, &mut x, Glyph::FirstPage, Cmd::FirstPage, "First page");
        button(ui, &mut x, Glyph::PrevPage, Cmd::PrevPage, "Previous page");

        // Page number field.
        let field = Rect::from_min_size(pos2(x + 4.0, rect.min.y + 4.0), vec2(68.0, 22.0));
        x += 76.0;
        let field_id = ui.id().with("page-field");
        let editing = ui.ctx().memory(|m| m.has_focus(field_id));
        if !editing {
            self.shell.page_text = if has_doc { format!("{} / {}", self.current_page + 1, page_count) } else { String::new() };
        }
        let edit = egui::TextEdit::singleline(&mut self.shell.page_text)
            .id(field_id)
            .horizontal_align(Align::Center)
            .desired_width(field.width());
        let response = ui.add_enabled_ui(has_doc, |ui| ui.put(field, edit)).inner;
        if response.gained_focus() {
            self.shell.page_text.clear();
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            if let Ok(number) = self.shell.page_text.trim().parse::<usize>() {
                self.goto_page(number.saturating_sub(1));
            }
        }

        let mut button = |ui: &mut Ui, x: &mut f32, glyph: Glyph, cmd: Cmd, tip: &str| {
            let r = Rect::from_min_size(pos2(*x, top), vec2(26.0, 26.0));
            *x += 26.0;
            if icon_button(ui, r, glyph, ("bottom", cmd), self.cmd_enabled(cmd), self.cmd_selected(cmd), tip) {
                fired = Some(cmd);
            }
        };
        button(ui, &mut x, Glyph::NextPage, Cmd::NextPage, "Next page");
        button(ui, &mut x, Glyph::LastPage, Cmd::LastPage, "Last page");
        x += 10.0;
        button(ui, &mut x, Glyph::FitPage, Cmd::FitPage, "Fit page");
        button(ui, &mut x, Glyph::FitWidth, Cmd::FitWidth, "Fit width");
        button(ui, &mut x, Glyph::ActualSize, Cmd::ActualSize, "Actual size");
        x += 10.0;
        button(ui, &mut x, Glyph::ZoomOut, Cmd::ZoomOut, "Zoom out");
        let percent = Rect::from_min_size(pos2(x, rect.min.y), vec2(56.0, rect.height()));
        x += 56.0;
        let text = if has_doc { format!("{:.0}%", self.zoom * 100.0 / ZOOM_100_PERCENT) } else { String::new() };
        ui.painter().text(percent.center(), Align2::CENTER_CENTER, text, FontId::proportional(13.0), theme::TEXT);
        button(ui, &mut x, Glyph::ZoomIn, Cmd::ZoomIn, "Zoom in");

        // Status text on the left, clipped so it never runs under the controls.
        let status_clip = Rect::from_min_max(rect.min, pos2((cluster_start - 8.0).max(rect.min.x), rect.max.y));
        ui.painter().with_clip_rect(status_clip).text(
            pos2(rect.min.x + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            &self.status,
            FontId::proportional(12.5),
            Color32::from_gray(225),
        );
        if self.doc.as_ref().is_some_and(|d| d.dirty) {
            ui.painter().text(
                pos2(rect.max.x - 14.0, rect.center().y),
                Align2::RIGHT_CENTER,
                "Edited",
                FontId::proportional(12.5),
                Color32::from_gray(225),
            );
        }

        if let Some(cmd) = fired {
            self.run(cmd);
        }
    }

    // ---- document side panel ---------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut Ui) {
        let panels = [
            (SidePanel::Pages, Glyph::Document, "Pages"),
            (SidePanel::Comments, Glyph::Note, "Comments"),
            (SidePanel::Signatures, Glyph::Sign, "Signatures"),
        ];
        ui.horizontal(|ui| {
            for (i, (panel, glyph, tip)) in panels.iter().enumerate() {
                let (rect, _) = ui.allocate_exact_size(vec2(34.0, 28.0), Sense::hover());
                let selected = self.shell.side_panel == *panel;
                if icon_button(ui, rect, *glyph, ("side", i), true, selected, tip) {
                    self.shell.side_panel = *panel;
                }
            }
        });
        ui.add_space(4.0);
        match self.shell.side_panel {
            SidePanel::Pages => self.thumbnail_panel(ui),
            SidePanel::Comments => self.comments_panel(ui),
            SidePanel::Signatures => self.signatures_panel(ui),
        }
    }

    fn comments_panel(&mut self, ui: &mut Ui) {
        let Some(doc) = self.doc.as_ref() else { return };
        let key = (self.active_tab, doc.version);
        if self.shell.comments.as_ref().map(|(k, _)| *k) != Some(key) {
            self.shell.comments = Some((key, doc.pdf.annotation_summaries()));
        }
        let items: Vec<AnnotationSummary> = self.shell.comments.as_ref().map(|(_, v)| v.clone()).unwrap_or_default();
        if items.is_empty() {
            ui.weak("No comments in this document.");
            return;
        }
        let mut goto = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for item in &items {
                let header = format!("Page {} - {}", item.page + 1, comment_kind(&item.kind));
                let response = ui
                    .scope(|ui| {
                        ui.label(egui::RichText::new(header).strong());
                        if !item.contents.is_empty() {
                            ui.label(&item.contents);
                        }
                    })
                    .response
                    .interact(Sense::click());
                if response.clicked() {
                    goto = Some(item.page);
                }
                ui.separator();
            }
        });
        if let Some(page) = goto {
            self.goto_page(page);
        }
    }

    fn signatures_panel(&mut self, ui: &mut Ui) {
        let Some(doc) = self.doc.as_ref() else { return };
        if !doc.pdf.has_signatures() {
            ui.weak("This document has no signatures.");
            ui.add_space(6.0);
            ui.weak("Use Security > Sign to add one.");
            return;
        }
        if let Some(text) = &self.notice {
            ui.label(text);
        }
        ui.add_space(6.0);
        if ui.button("Validate All Signatures").clicked() {
            self.validate_signatures();
        }
    }

    // ---- start screen ----------------------------------------------------------------------

    fn start_sidebar(&mut self, ui: &mut Ui) {
        enum Pick {
            Recent,
            Folder(PathBuf),
            Remove(usize),
            Add,
        }
        let mut pick: Option<Pick> = None;
        let font = FontId::proportional(14.0);
        let row = |ui: &mut Ui, label: &str, selected: bool, id: usize| -> egui::Response {
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
            if selected {
                ui.painter().rect_filled(rect, 3.0, theme::SIDEBAR_SELECTED);
            } else if response.hovered() {
                ui.painter().rect_filled(rect, 3.0, Color32::from_gray(58));
            }
            let _ = id;
            ui.painter().text(pos2(rect.min.x + 26.0, rect.center().y), Align2::LEFT_CENTER, label, font.clone(), theme::TEXT);
            response
        };

        let recent_selected = self.shell.start_view == StartView::Recent;
        if row(ui, "Recent", recent_selected, 0).clicked() {
            pick = Some(Pick::Recent);
        }
        for (i, dir) in self.shell.locations.iter().enumerate() {
            let selected = self.shell.start_view == StartView::Folder(dir.clone());
            let response = row(ui, &elide(&paths::location_label(dir), 24), selected, i + 1);
            let response = response.on_hover_text(dir.display().to_string());
            if response.clicked() {
                pick = Some(Pick::Folder(dir.clone()));
            }
            response.context_menu(|ui| {
                if ui.button("Remove Location").clicked() {
                    pick = Some(Pick::Remove(i));
                    ui.close();
                }
            });
        }
        if row(ui, "Add Location", false, usize::MAX).clicked() {
            pick = Some(Pick::Add);
        }

        match pick {
            Some(Pick::Recent) => {
                self.shell.start_view = StartView::Recent;
                self.shell.start_rows = None;
            }
            Some(Pick::Folder(dir)) => {
                self.shell.start_view = StartView::Folder(dir);
                self.shell.start_rows = None;
            }
            Some(Pick::Remove(index)) => {
                let removed = self.shell.locations.remove(index);
                paths::save_locations(&self.shell.locations);
                if self.shell.start_view == StartView::Folder(removed) {
                    self.shell.start_view = StartView::Recent;
                }
                self.shell.start_rows = None;
            }
            Some(Pick::Add) => self.shell.add_location = Some(paths::cloud_locations()),
            None => {}
        }
    }

    fn start_rows(&mut self) -> Vec<StartRow> {
        if let Some(rows) = &self.shell.start_rows {
            return rows.clone();
        }
        let rows: Vec<StartRow> = match (&self.shell.start_view, self.shell.folders_tab) {
            (StartView::Recent, false) => self
                .recent
                .iter()
                .filter_map(|r| {
                    let size = std::fs::metadata(&r.path).ok()?.len();
                    Some(StartRow {
                        path: r.path.clone(),
                        name: file_name(&r.path),
                        subtitle: "PDF".into(),
                        is_folder: false,
                        time: r.opened,
                        size: Some(size),
                    })
                })
                .collect(),
            (StartView::Recent, true) => {
                let mut seen: Vec<StartRow> = Vec::new();
                for r in &self.recent {
                    let Some(dir) = r.path.parent() else { continue };
                    if seen.iter().any(|s| s.path == dir) || !dir.is_dir() {
                        continue;
                    }
                    seen.push(StartRow {
                        path: dir.to_path_buf(),
                        name: file_name(dir),
                        subtitle: dir.display().to_string(),
                        is_folder: true,
                        time: r.opened,
                        size: None,
                    });
                }
                seen
            }
            (StartView::Folder(dir), _) => {
                let mut rows: Vec<StartRow> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")))
                    .filter_map(|e| {
                        let meta = e.metadata().ok()?;
                        let time = meta
                            .modified()
                            .ok()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        Some(StartRow {
                            path: e.path(),
                            name: file_name(&e.path()),
                            subtitle: "PDF".into(),
                            is_folder: false,
                            time,
                            size: Some(meta.len()),
                        })
                    })
                    .collect();
                rows.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.name.cmp(&b.name)));
                rows
            }
        };
        self.shell.start_rows = Some(rows.clone());
        rows
    }

    fn start_main(&mut self, ui: &mut Ui) {
        let mut fired: Option<Cmd> = None;
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Quick Launch").size(19.0));
        ui.add_space(12.0);

        let launch = [
            (Glyph::Open, "Open", Cmd::Open),
            (Glyph::NewDocument, "Create", Cmd::NewBlank),
            (Glyph::Reduce, "Reduce File Size", Cmd::Reduce),
            (Glyph::Merge, "Combine All", Cmd::Combine),
            (Glyph::Lock, "Protect PDF", Cmd::Protect),
        ];
        let font = FontId::proportional(19.0);
        let (strip, _) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
        let mut x = strip.min.x + 4.0;
        for (i, (glyph, label, cmd)) in launch.iter().enumerate() {
            let width = 10.0 + 38.0 + 14.0 + text_width(ui, label, &font) + 10.0;
            let r = Rect::from_min_size(pos2(x, strip.min.y), vec2(width, strip.height()));
            let response = ui.interact(r, ui.id().with(("launch", i)), Sense::click());
            if response.hovered() {
                ui.painter().rect_filled(r, 6.0, Color32::from_gray(46));
            }
            icons::paint(ui.painter(), Rect::from_center_size(pos2(r.min.x + 29.0, r.center().y), vec2(34.0, 34.0)), *glyph, theme::TEXT);
            ui.painter().text(pos2(r.min.x + 62.0, r.center().y), Align2::LEFT_CENTER, *label, font.clone(), theme::TEXT);
            if response.clicked() {
                fired = Some(*cmd);
            }
            x += width + 36.0;
        }
        ui.add_space(22.0);

        // Recent File / Recent Folder tabs and Clear All.
        let in_recent = self.shell.start_view == StartView::Recent;
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        let tab_font = FontId::proportional(14.0);
        let mut tab_x = bar.min.x + 2.0;
        let heading = match &self.shell.start_view {
            StartView::Recent => None,
            StartView::Folder(dir) => Some(file_name(dir)),
        };
        if let Some(name) = heading {
            ui.painter().text(pos2(tab_x, bar.center().y), Align2::LEFT_CENTER, name, tab_font.clone(), theme::TEXT);
        } else {
            for (i, (label, folders)) in [("Recent File", false), ("Recent Folder", true)].iter().enumerate() {
                let width = text_width(ui, label, &tab_font) + 8.0;
                let r = Rect::from_min_size(pos2(tab_x, bar.min.y), vec2(width, bar.height()));
                let response = ui.interact(r, ui.id().with(("recent-tab", i)), Sense::click());
                let selected = self.shell.folders_tab == *folders;
                let color = if selected { theme::TEXT } else { theme::TEXT_DIM };
                ui.painter().text(pos2(r.min.x + 4.0, r.center().y - 2.0), Align2::LEFT_CENTER, *label, tab_font.clone(), color);
                if selected {
                    ui.painter().line_segment([pos2(r.min.x + 2.0, r.max.y - 3.0), pos2(r.max.x - 2.0, r.max.y - 3.0)], Stroke::new(2.0, theme::TEXT));
                }
                if response.clicked() && !selected {
                    self.shell.folders_tab = *folders;
                    self.shell.start_rows = None;
                }
                tab_x += width + 22.0;
            }
        }
        if in_recent && !self.recent.is_empty() {
            let clear = Rect::from_min_size(pos2(bar.max.x - 84.0, bar.min.y + 2.0), vec2(80.0, 26.0));
            let response = ui.interact(clear, ui.id().with("clear-all"), Sense::click());
            let fill = if response.hovered() { Color32::from_gray(96) } else { Color32::from_gray(72) };
            ui.painter().rect_filled(clear, 4.0, fill);
            ui.painter().text(clear.center(), Align2::CENTER_CENTER, "Clear All", FontId::proportional(13.0), theme::TEXT);
            if response.clicked() {
                self.recent.clear();
                paths::save_recent(&self.recent);
                self.shell.start_rows = None;
            }
        }
        ui.add_space(10.0);

        // Table header.
        let rows = self.start_rows();
        let time_title = if matches!(self.shell.start_view, StartView::Folder(_)) { "MODIFIED" } else { "OPENED" };
        let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        let head_font = FontId::proportional(12.0);
        let name_x = header.min.x + 128.0;
        let time_x = header.max.x - 330.0;
        let size_x = header.max.x - 170.0;
        for (x, title) in [(name_x, "NAME"), (time_x, time_title), (size_x, "SIZE")] {
            ui.painter().text(pos2(x, header.center().y), Align2::LEFT_CENTER, title, head_font.clone(), theme::TEXT_DIM);
        }

        if rows.is_empty() {
            ui.add_space(24.0);
            let message = match &self.shell.start_view {
                StartView::Recent => "No recent files yet. Use Open to choose a PDF.",
                StartView::Folder(_) => "There are no PDF files in this folder.",
            };
            ui.label(egui::RichText::new(message).color(theme::TEXT_DIM));
        }

        let mut open: Option<StartRow> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, row) in rows.iter().enumerate() {
                let (r, response) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::click());
                if response.hovered() {
                    ui.painter().rect_filled(r, 4.0, Color32::from_gray(40));
                }
                let glyph = if row.is_folder { Glyph::Folder } else { Glyph::Document };
                icons::paint(ui.painter(), Rect::from_center_size(pos2(r.min.x + 74.0, r.center().y), vec2(30.0, 30.0)), glyph, theme::TEXT);
                let name = elide(&row.name, 70);
                ui.painter().text(pos2(name_x, r.center().y - 9.0), Align2::LEFT_CENTER, name, FontId::proportional(15.0), theme::TEXT);
                let subtitle = elide(&row.subtitle, 90);
                ui.painter().text(pos2(name_x, r.center().y + 11.0), Align2::LEFT_CENTER, subtitle, FontId::proportional(12.0), theme::TEXT_DIM);
                ui.painter().text(pos2(time_x, r.center().y), Align2::LEFT_CENTER, format_time(row.time), FontId::proportional(13.5), theme::TEXT);
                if let Some(size) = row.size {
                    ui.painter().text(pos2(size_x, r.center().y), Align2::LEFT_CENTER, format_size(size), FontId::proportional(13.5), theme::TEXT);
                }
                let response = response.on_hover_text(row.path.display().to_string());
                if response.clicked() {
                    open = Some(row.clone());
                }
                let _ = i;
            }
        });

        if let Some(row) = open {
            if row.is_folder {
                self.shell.start_view = StartView::Folder(row.path);
                self.shell.folders_tab = false;
                self.shell.start_rows = None;
            } else {
                self.open_path(&row.path);
            }
        }
        if let Some(cmd) = fired {
            self.run(cmd);
        }
    }
}
