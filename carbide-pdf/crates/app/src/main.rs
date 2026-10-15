//! Carbide PDF: desktop application (egui).

mod dialogs;
mod icons;
mod paths;
mod quit;
mod ribbon;
mod shell;
#[cfg(test)]
mod tests;
mod theme;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use carbide_engine::autosave::{now_unix, AutoSaveSettings, AutoSaveStore, RecordId};
use carbide_engine::fs::Existing;
use carbide_engine::{
    open_revision_bytes, pdf_date_now, revision_display_name, Error as EngineError, MarkupStyle, Pdf, PageGeometry,
    Protection, Rect, Redaction, Renderer, Revision, SavePlan, SavedFile,
};
use dialogs::{AfterSave, Dialog, Export, Writing};
use eframe::egui;

const MAX_UNDO: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Highlight,
    /// Highlights exactly the dragged rectangle instead of snapping to text lines.
    HighlightArea,
    Underline,
    StrikeOut,
    Note,
    TextBox,
    Redact,
    Sign,
}

/// An open document and everything derived from its current state.
pub struct OpenDoc {
    /// Tells this document apart from every other one opened in this session, whichever tab it is in.
    pub id: u64,
    pub pdf: Pdf,
    /// Serialized current state, used for rendering and text extraction.
    pub bytes: Vec<u8>,
    pub path: Option<PathBuf>,
    /// The file as last read from or written to disk, with the password that opened it: what Save,
    /// Save Revision and Sign build on.
    pub saved: Option<SavedFile>,
    /// An edit since the last save took out content the file on disk still holds (redaction,
    /// deleted pages, anything that dropped objects of the file). Save then never appends.
    pub content_removed: bool,
    pub version: u64,
    pub record: Option<RecordId>,
    pub dirty: bool,
    /// When this document's Auto Recovery copy was last considered: each document keeps its own
    /// interval, counted from when it was opened, so opening one never delays another's copy.
    autosaved_at: Instant,
    /// What a document without a file is called (an earlier revision, say); `None` for Untitled.
    pub name: Option<String>,
    /// Set for a document opened from an Auto Recovery copy, until it is saved.
    pub recovered: Option<Recovered>,
    pub geometries: Vec<PageGeometry>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    // View state kept while the document sits in a background tab.
    view_page: usize,
    view_redactions: Vec<Redaction>,
    view_notice: Option<String>,
}

/// Where a document opened from an Auto Recovery copy came from. It has no file of its own: Save
/// writes over the original, once the user agrees, or asks where to put it.
pub struct Recovered {
    /// The file it was being edited from; `None` for a document that was never saved.
    pub original: Option<PathBuf>,
    /// The record it was recovered from, which goes once the document is saved.
    pub record: RecordId,
}

/// A point in the edit history.
struct Snapshot {
    bytes: Vec<u8>,
    content_removed: bool,
}

impl OpenDoc {
    fn new(mut pdf: Pdf, path: Option<PathBuf>, saved: Option<SavedFile>) -> Result<Self, EngineError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let bytes = pdf.to_bytes()?;
        let geometries = pdf.page_geometries();
        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            pdf,
            bytes,
            path,
            saved,
            content_removed: false,
            version: 0,
            record: None,
            dirty: false,
            autosaved_at: Instant::now(),
            name: None,
            recovered: None,
            geometries,
            undo: Vec::new(),
            redo: Vec::new(),
            view_page: 0,
            view_redactions: Vec::new(),
            view_notice: None,
        })
    }

    /// Re-derives bytes and geometry after the PDF object changed.
    fn refresh(&mut self) -> Result<(), EngineError> {
        self.bytes = self.pdf.to_bytes()?;
        self.geometries = self.pdf.page_geometries();
        self.version += 1;
        Ok(())
    }

    /// Forgets the edit history. After a signature or a revision is written into the file, going
    /// back past it would take it out of the document again, and the next save would drop it.
    fn forget_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Replaces the document with `bytes`, a serialized state of it from its edit history. That
    /// serialization is unencrypted, so the protection it is written with is carried over.
    fn restore(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        let mut pdf = Pdf::from_bytes(bytes)?;
        pdf.set_protection(self.pdf.protection());
        self.pdf = pdf;
        self.refresh()
    }

    /// The protection a new file made from this document gets (that of its file, or the one it
    /// carries when it has none yet), and the password that opened it.
    pub fn protection(&self) -> (Option<Protection>, Option<String>) {
        let protection = match &self.saved {
            Some(saved) => saved.protection().cloned(),
            None => self.pdf.protection(),
        };
        (protection, self.saved.as_ref().and_then(|s| s.password()).map(str::to_owned))
    }

    pub fn is_protected(&self) -> bool {
        self.protection().0.is_some()
    }

    /// The bytes of `part`, a new document made from this one, protected like this document.
    pub fn derived_bytes(&self, part: &mut Pdf) -> Result<Vec<u8>, EngineError> {
        let (protection, password) = self.protection();
        part.set_protection(protection);
        part.to_file_bytes(password.as_deref())
    }

    /// Whether the document lost content the file on disk holds: `explicit` for edits known to
    /// remove content, otherwise found by comparing with the file.
    fn removes_content(&self, explicit: bool) -> bool {
        explicit || self.saved.as_ref().is_some_and(|saved| saved.removed_from(&self.pdf))
    }

    /// Makes `bytes`, just written to disk, the file this document is based on and reloads the
    /// document from it, so that what is shown is what is on disk.
    fn adopt_saved(&mut self, bytes: Vec<u8>) -> Result<(), EngineError> {
        let password = self.saved.as_ref().and_then(|s| s.password()).map(str::to_owned);
        let (pdf, saved) = SavedFile::open(bytes, password)?;
        self.pdf = pdf;
        self.saved = Some(saved);
        self.dirty = false;
        self.content_removed = false;
        // History entries recorded removals against the old file; the new one is compared afresh.
        for snapshot in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            snapshot.content_removed = false;
        }
        self.refresh()
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| "Untitled".into())
    }

    /// The name Save As suggests: that of the file a recovered document was recovered for, or else
    /// the title, as a PDF file name.
    pub fn file_name(&self) -> String {
        let recovered_for = self.recovered.as_ref().and_then(|r| r.original.as_ref()).and_then(|p| p.file_name());
        let name = match recovered_for {
            Some(name) => name.to_string_lossy().into_owned(),
            None => self.title().trim_end_matches(" (recovered)").to_string(),
        };
        if name.to_ascii_lowercase().ends_with(".pdf") {
            name
        } else {
            format!("{name}.pdf")
        }
    }
}

type OcrResult = Result<(Pdf, carbide_engine::ocr::OcrReport), String>;

/// Text recognition running on a copy of one document.
struct OcrJob {
    /// The document and the version of it the copy was taken from. The result replaces the
    /// document, so it is only used while the document is still exactly that version.
    doc: u64,
    version: u64,
    receiver: std::sync::mpsc::Receiver<OcrResult>,
    /// Received, and waiting for its document to come back to the front.
    result: Option<OcrResult>,
}

/// A signature that waits for a time stamp authority, made on a copy of one document.
struct SignJob {
    /// The document and the version of it that was signed. The result replaces its file, so it is
    /// only written while the document is still exactly that version.
    doc: u64,
    version: u64,
    path: PathBuf,
    /// The status line once the signed file is written.
    done: String,
    receiver: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    /// Received, and waiting for its document to come back to the front.
    result: Option<Result<Vec<u8>, String>>,
}

/// How an edit changed the pages (see [`CarbideApp::follow_page_change`]).
#[derive(Clone, Copy, Debug)]
pub enum PageChange {
    /// A page was inserted so that it is now at this index.
    Inserted(usize),
    Deleted(usize),
    /// A page was taken out at `from` and put back so that it is at `to`.
    Moved { from: usize, to: usize },
    /// A page was turned a quarter turn.
    Turned { page: usize, clockwise: bool },
}

#[derive(Clone, Copy)]
struct DragState {
    page: usize,
    start: (f32, f32),
    current: (f32, f32),
}

enum Action {
    SignArea { page: usize, rect: Rect },
    Markup { page: usize, rect: Rect, style: MarkupStyle, snap: bool },
    MarkRedaction { page: usize, rect: Rect },
    NotePrompt { page: usize, x: f32, y: f32, on_page: bool },
    Visible(usize),
}

pub struct CarbideApp {
    renderer: Option<&'static Renderer>,
    renderer_error: Option<String>,
    pub doc: Option<OpenDoc>,
    pub tool: Tool,
    zoom: f32,
    pub status: String,
    /// The dialog on screen. Others wait in `queued_dialogs` (see [`CarbideApp::queue_dialog`]).
    pub dialog: Option<Dialog>,
    queued_dialogs: VecDeque<Dialog>,
    textures: HashMap<(u64, usize, u32), egui::TextureHandle>,
    thumbs: HashMap<(u64, usize), egui::TextureHandle>,
    /// Whether each page of the active document, by version, can be rendered safely (see
    /// [`Pdf::check_drawable`]).
    drawable: HashMap<(u64, usize), bool>,
    drag: Option<DragState>,
    pub pending_redactions: Vec<Redaction>,
    pub current_page: usize,
    scroll_to: Option<usize>,
    pub autosave: Option<AutoSaveStore>,
    pub settings: AutoSaveSettings,
    /// A running (or finished, not yet applied) OCR job.
    ocr_job: Option<OcrJob>,
    /// A signature waiting for (or holding, not yet written) its time stamp.
    sign_job: Option<SignJob>,
    /// The summary of the signatures of the active document's file, as last checked. The notice
    /// bar shows it through [`CarbideApp::shown_notice`].
    pub notice: Option<String>,
    /// One slot per open document, in tab order. The active document lives in `doc`, and its slot
    /// here is `None` until it is parked behind another tab.
    tabs: Vec<Option<OpenDoc>>,
    /// Index into `tabs`; `None` shows the Start screen.
    active_tab: Option<usize>,
    recent: Vec<paths::RecentFile>,
    shell: shell::ShellState,
}

impl CarbideApp {
    fn new(cc: &eframe::CreationContext<'_>, launch: Option<Launch>) -> Self {
        theme::apply(&cc.egui_ctx);
        guard_quitting(&cc.egui_ctx);
        let (renderer, renderer_error) = match Renderer::global() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let autosave = AutoSaveStore::open(paths::autosave_dir()).ok();
        let mut app = Self::with_parts(renderer, renderer_error, autosave, paths::load_autosave_settings());
        app.launch(launch);
        app
    }

    /// Opens what the window was started with, then offers the documents an earlier session left
    /// for recovery. The recovery panel waits behind a question the opening asked (a password).
    fn launch(&mut self, launch: Option<Launch>) {
        match launch {
            Some(Launch::File(path)) => self.open_path(&path),
            Some(Launch::Copy(path)) => self.open_copy(&path),
            None => {}
        }
        if self.autosave.as_ref().is_some_and(|s| s.any_needs_save()) {
            self.queue_dialog(Dialog::auto_recovery());
        }
    }

    /// The application state without a window, so it can also be driven by tests.
    fn with_parts(
        renderer: Option<&'static Renderer>,
        renderer_error: Option<String>,
        autosave: Option<AutoSaveStore>,
        settings: AutoSaveSettings,
    ) -> Self {
        Self {
            renderer,
            renderer_error,
            doc: None,
            tool: Tool::Select,
            zoom: 1.25,
            status: "Open a PDF, or drop one on the window.".into(),
            dialog: None,
            queued_dialogs: VecDeque::new(),
            textures: HashMap::new(),
            thumbs: HashMap::new(),
            drawable: HashMap::new(),
            drag: None,
            pending_redactions: Vec::new(),
            current_page: 0,
            scroll_to: None,
            autosave,
            settings,
            ocr_job: None,
            sign_job: None,
            notice: None,
            tabs: Vec::new(),
            active_tab: None,
            recent: paths::load_recent(),
            shell: shell::ShellState::new(),
        }
    }

    // ---- documents ---------------------------------------------------------------------

    pub fn open_path(&mut self, path: &Path) {
        // A file that is already open is brought forward instead of opened twice.
        let wanted = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let open_at = (0..self.tabs.len()).find(|&i| {
            self.doc_at(i)
                .and_then(|d| d.path.as_ref())
                .is_some_and(|p| p.canonicalize().unwrap_or_else(|_| p.clone()) == wanted)
        });
        if let Some(index) = open_at {
            return self.activate(Some(index));
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => return self.alert(format!("Could not read {}: {e}", path.display())),
        };
        match Pdf::from_bytes(&bytes) {
            Ok(pdf) => {
                let saved = SavedFile::new(bytes, &pdf, None);
                self.install(pdf, Some(path.to_path_buf()), Some(saved));
            }
            Err(EngineError::PasswordRequired) => {
                self.open_dialog(Dialog::Password { path: path.to_path_buf(), bytes, input: String::new(), failed: false });
            }
            Err(e) => self.alert(format!("Could not open {}: {e}", path.display())),
        }
    }

    /// Opens `path`, a private temporary copy another window wrote (an earlier revision), as a new
    /// document that has no file yet: Save asks where to put it, so nothing is ever saved into the
    /// temporary folder. The copy is removed once it has been read.
    pub fn open_copy(&mut self, path: &Path) {
        let bytes = std::fs::read(path);
        if carbide_engine::fs::is_private_temp_file(path) {
            let _ = carbide_engine::fs::remove_private_temp_file(path);
        }
        let opened = bytes.map_err(EngineError::from).and_then(|bytes| Pdf::from_bytes(&bytes));
        match opened {
            Ok(pdf) => {
                self.install(pdf, None, None);
                if let Some(doc) = self.doc.as_mut() {
                    doc.name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    self.status = format!("{} - {} page(s)", doc.title(), doc.pdf.page_count());
                }
            }
            Err(e) => self.alert(format!("Could not open the copy: {e}")),
        }
    }

    /// Opens the Auto Recovery copy of `record` as a document with unsaved changes, tied to the file
    /// it was recovered for. It has no file of its own, so nothing is ever saved into the recovery
    /// or temporary folder: Save asks to replace the original, or where to put a document that never
    /// had one. A copy that is already open is brought forward instead.
    pub fn open_recovered(&mut self, record: RecordId) {
        let open_at = (0..self.tabs.len()).find(|&i| self.doc_at(i).and_then(|d| d.recovered.as_ref()).is_some_and(|r| r.record == record));
        if let Some(index) = open_at {
            return self.activate(Some(index));
        }
        let Some(store) = self.autosave.as_ref() else { return };
        let original = store.record(record).and_then(|r| r.original.as_ref()).map(|o| o.path.clone());
        let (pdf, title) = match store.open_recovered(record, None) {
            Ok(opened) => opened,
            Err(e) => return self.alert(e.to_string()),
        };
        self.install(pdf, None, None);
        let Some(doc) = self.doc.as_mut() else { return };
        doc.name = Some(title);
        doc.dirty = true;
        // Kept for recovery under the original's name, should this session end unexpectedly too.
        if let (Some(store), Some(id)) = (self.autosave.as_mut(), doc.record) {
            let _ = store.document_closed(id);
            doc.record = Some(store.begin_document(original.as_deref(), now_unix()));
        }
        doc.recovered = Some(Recovered { original, record });
        self.status = format!("{} - not saved yet", doc.title());
    }

    /// Opens a protected file. The password is used exactly as typed, spaces included.
    pub fn open_with_password(&mut self, path: PathBuf, bytes: Vec<u8>, password: String) -> bool {
        match SavedFile::open(bytes, Some(password)) {
            Ok((pdf, saved)) => {
                self.install(pdf, Some(path), Some(saved));
                true
            }
            Err(_) => false,
        }
    }

    /// Opens `pdf` in a new tab and makes it the active document.
    fn install(&mut self, pdf: Pdf, path: Option<PathBuf>, saved: Option<SavedFile>) {
        match OpenDoc::new(pdf, path.clone(), saved) {
            Ok(mut doc) => {
                if let Some(store) = self.autosave.as_mut() {
                    doc.record = Some(store.begin_document(path.as_deref(), now_unix()));
                }
                self.park_active();
                self.status = format!("{} - {} page(s)", doc.title(), doc.pdf.page_count());
                self.notice = signature_notice(&doc);
                self.doc = Some(doc);
                self.tabs.push(None);
                self.active_tab = Some(self.tabs.len() - 1);
                self.reset_view_caches();
                self.pending_redactions.clear();
                self.current_page = 0;
                self.scroll_to = Some(0);
                if let Some(path) = path {
                    self.push_recent(&path);
                }
            }
            Err(e) => self.alert(format!("Could not read the document: {e}")),
        }
    }

    fn reset_view_caches(&mut self) {
        self.forget_page_images();
        self.drag = None;
        self.shell.forget_caches();
    }

    /// Drops the page images and what is known about drawing the pages, after the document changed.
    fn forget_page_images(&mut self) {
        self.textures.clear();
        self.thumbs.clear();
        self.drawable.clear();
    }

    /// The document in tab `index`, whether it is the active one or parked.
    fn doc_at(&self, index: usize) -> Option<&OpenDoc> {
        if self.active_tab == Some(index) {
            self.doc.as_ref()
        } else {
            self.tabs.get(index).and_then(|slot| slot.as_ref())
        }
    }

    /// The tab that shows document `id`.
    fn tab_of(&self, id: u64) -> Option<usize> {
        (0..self.tabs.len()).find(|&i| self.doc_at(i).is_some_and(|d| d.id == id))
    }

    /// Moves the active document back into its tab slot, together with its view state.
    fn park_active(&mut self) {
        let Some(index) = self.active_tab else { return };
        let Some(mut doc) = self.doc.take() else { return };
        self.drop_document_question();
        doc.view_page = self.current_page;
        doc.view_redactions = std::mem::take(&mut self.pending_redactions);
        doc.view_notice = self.notice.take();
        self.tabs[index] = Some(doc);
        self.active_tab = None;
    }

    /// A pending question about the active document (whether to save, replace files, ...) belongs
    /// to it, and so does a dialog that acts on it. Whenever that document is put away (another one
    /// opened, created or brought forward, or this one closed) they go, waiting ones included, so
    /// answering one can never act on a different document.
    fn drop_document_question(&mut self) {
        if self.dialog.as_ref().is_some_and(|d| d.document().is_some()) {
            self.dialog = None;
        }
        self.queued_dialogs.retain(|d| d.document().is_none());
        self.next_dialog();
    }

    /// Shows `dialog` in place of the one on screen, as a command does. A password question is
    /// never replaced: the file it would open would be dropped. `dialog` waits behind it instead.
    pub fn open_dialog(&mut self, dialog: Dialog) {
        if matches!(self.dialog, Some(Dialog::Password { .. })) {
            self.queued_dialogs.push_back(dialog);
        } else {
            self.dialog = Some(dialog);
        }
    }

    /// Shows `dialog` once every dialog before it has closed: now, when none is on screen. What
    /// background work and launch have to say waits like this, so it never takes the place of a
    /// dialog the user is answering.
    pub fn queue_dialog(&mut self, dialog: Dialog) {
        if self.dialog.is_none() {
            self.dialog = Some(dialog);
        } else {
            self.queued_dialogs.push_back(dialog);
        }
    }

    /// Brings up the next waiting dialog when none is on screen.
    pub(crate) fn next_dialog(&mut self) {
        if self.dialog.is_none() {
            self.dialog = self.queued_dialogs.pop_front();
        }
    }

    /// Shows tab `index`, or the Start screen for `None`.
    pub fn activate(&mut self, index: Option<usize>) {
        if self.active_tab == index {
            return;
        }
        self.park_active();
        if let Some(i) = index {
            if let Some(mut doc) = self.tabs.get_mut(i).and_then(Option::take) {
                self.current_page = doc.view_page;
                self.pending_redactions = std::mem::take(&mut doc.view_redactions);
                self.notice = doc.view_notice.take();
                self.status = format!("{} - {} page(s)", doc.title(), doc.pdf.page_count());
                self.scroll_to = Some(self.current_page);
                self.doc = Some(doc);
                self.active_tab = Some(i);
            }
        }
        self.reset_view_caches();
    }

    /// Closes a tab without asking; the caller handles unsaved changes.
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let was_active = self.active_tab == Some(index);
        let doc = if was_active {
            self.drop_document_question();
            self.active_tab = None;
            self.pending_redactions.clear();
            self.notice = None;
            self.doc.take()
        } else {
            self.tabs[index].take()
        };
        self.tabs.remove(index);
        if let Some(active) = self.active_tab.filter(|&a| a > index) {
            self.active_tab = Some(active - 1);
        }
        // The closed document's recovery record goes away with it.
        if let (Some(doc), Some(store)) = (doc, self.autosave.as_mut()) {
            if let Some(id) = doc.record {
                let _ = store.document_closed(id);
            }
        }
        self.reset_view_caches();
        if was_active {
            let next = (!self.tabs.is_empty()).then(|| index.min(self.tabs.len() - 1));
            self.activate(next);
        }
    }

    fn push_recent(&mut self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.recent.retain(|r| r.path != path);
        self.recent.insert(0, paths::RecentFile { path, opened: paths::now_secs() });
        self.recent.truncate(paths::MAX_RECENT);
        paths::save_recent(&self.recent);
        self.shell.forget_start_rows();
    }

    /// A new untitled one-page document.
    pub fn new_blank(&mut self) {
        let mut pdf = Pdf::new();
        match pdf.insert_blank_page(0, None) {
            Ok(()) => {
                self.install(pdf, None, None);
                if let Some(doc) = self.doc.as_mut() {
                    doc.dirty = true;
                }
            }
            Err(e) => self.alert(format!("Could not create a document: {e}")),
        }
    }

    /// Joins several files into a new untitled document.
    pub fn combine_files(&mut self) {
        let Some(files) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_title("Choose the files to combine").pick_files() else {
            return;
        };
        let mut documents = Vec::new();
        for file in files {
            match Pdf::open(&file) {
                Ok(p) => documents.push(p),
                Err(e) => return self.alert(format!("{}: {e}", file.display())),
            }
        }
        self.combine(documents, false);
    }

    /// Joins `documents` into a new untitled document. It keeps the protection the protected ones
    /// share; when they are protected in different ways the user is asked whether to combine them
    /// without protection, which `unprotected` says was agreed to.
    pub fn combine(&mut self, mut documents: Vec<Pdf>, unprotected: bool) {
        if documents.is_empty() {
            return;
        }
        if unprotected {
            documents.iter_mut().for_each(|d| d.set_protection(None));
        }
        let refs: Vec<&Pdf> = documents.iter().collect();
        if Pdf::shared_protection(&refs).is_err() {
            self.open_dialog(Dialog::ConfirmCombine { documents });
            return;
        }
        match Pdf::merge(&refs) {
            Ok(pdf) => {
                let protected = pdf.is_protected();
                self.install(pdf, None, None);
                if let Some(doc) = self.doc.as_mut() {
                    doc.dirty = true;
                }
                if protected {
                    self.status = "Combined. The new document keeps the password protection of the files.".into();
                }
            }
            Err(e) => self.alert(format!("Could not combine the files: {e}")),
        }
    }

    pub fn alert(&mut self, message: String) {
        self.status = message.lines().next().unwrap_or_default().to_string();
        self.queue_dialog(Dialog::Message(message));
    }

    /// Runs an edit on the open document with undo support, and returns whether it was made.
    pub fn edit(&mut self, label: &str, f: impl FnOnce(&mut Pdf) -> Result<(), EngineError>) -> bool {
        self.run_edit(label, false, f)
    }

    /// Runs an edit that takes content out of the document, such as applying redactions or
    /// deleting pages. The next Save then writes a clean file instead of appending to the old one.
    pub fn edit_removing(&mut self, label: &str, f: impl FnOnce(&mut Pdf) -> Result<(), EngineError>) -> bool {
        self.run_edit(label, true, f)
    }

    /// Moves what is placed on pages along with an edit that changed the pages, so that each still
    /// sits on what it was placed on: the marked redactions, and the page and spot of a note or a
    /// visible signature waiting in a dialog. One on a deleted page goes, one on a moved page moves,
    /// one on a turned page turns with it. `before` is the page geometry from before the edit.
    pub fn follow_page_change(&mut self, change: PageChange, before: &[PageGeometry]) {
        let marks = std::mem::take(&mut self.pending_redactions);
        self.pending_redactions = marks
            .into_iter()
            .filter_map(|Redaction { page, rect }| {
                let (page, rect) = follow(change, before, page, rect)?;
                Some(Redaction { page, rect })
            })
            .collect();
        let dialogs = self.dialog.take().into_iter().chain(std::mem::take(&mut self.queued_dialogs));
        let kept: VecDeque<Dialog> = dialogs.filter_map(|dialog| dialog.follow(|page, rect| follow(change, before, page, rect))).collect();
        self.queued_dialogs = kept;
        self.next_dialog();
    }

    /// Drops the dialogs that place something on a page, after the pages changed in a way that
    /// cannot be followed (going back or forward through the edit history).
    fn drop_placed_dialogs(&mut self) {
        let dialogs = self.dialog.take().into_iter().chain(std::mem::take(&mut self.queued_dialogs));
        self.queued_dialogs = dialogs.filter(|dialog| !dialog.places_on_page()).collect();
        self.next_dialog();
    }

    /// Runs an edit, and returns whether it was made. One that changed nothing (moving a page to
    /// where it is, say) is not recorded: no undo step, nothing to save. An edit that removes
    /// content always counts, since what it removed can be held by the file alone.
    fn run_edit(&mut self, label: &str, removes: bool, f: impl FnOnce(&mut Pdf) -> Result<(), EngineError>) -> bool {
        let Some(doc) = self.doc.as_mut() else { return false };
        let snapshot = Snapshot { bytes: doc.bytes.clone(), content_removed: doc.content_removed };
        let version = doc.version;
        // Compared as objects: writing a file numbers its cross-reference stream afresh each time.
        let before = (!removes).then(|| doc.pdf.document().objects.clone());
        match f(&mut doc.pdf).and_then(|_| doc.refresh()) {
            Ok(()) if before.as_ref() == Some(&doc.pdf.document().objects) => {
                doc.version = version;
                self.status = format!("{label}: nothing changed.");
                false
            }
            Ok(()) => {
                doc.undo.push(snapshot);
                if doc.undo.len() > MAX_UNDO {
                    doc.undo.remove(0);
                }
                doc.redo.clear();
                doc.content_removed = doc.content_removed || doc.removes_content(removes);
                doc.dirty = true;
                self.status = format!("{label} done.");
                self.forget_page_images();
                true
            }
            Err(e) => {
                // Put the document back exactly as it was.
                let _ = doc.restore(&snapshot.bytes);
                let message = format!("{label} failed: {e}");
                self.alert(message);
                false
            }
        }
    }

    fn undo(&mut self) {
        self.step_history(true);
    }

    fn redo(&mut self) {
        self.step_history(false);
    }

    /// Moves one step back (`undo`) or forward through the document's edit history.
    fn step_history(&mut self, undo: bool) {
        let Some(doc) = self.doc.as_mut() else { return };
        let target = if undo { doc.undo.pop() } else { doc.redo.pop() };
        let Some(snapshot) = target else { return };
        let current = Snapshot { bytes: doc.bytes.clone(), content_removed: doc.content_removed };
        match doc.restore(&snapshot.bytes) {
            Ok(()) => {
                doc.content_removed = doc.removes_content(snapshot.content_removed);
                if undo {
                    doc.redo.push(current);
                } else {
                    doc.undo.push(current);
                }
                doc.dirty = true;
                self.status = if undo { "Undid last change." } else { "Redid last change." }.into();
                self.forget_page_images();
                self.pending_redactions.clear();
                self.drop_placed_dialogs();
            }
            Err(e) => self.alert(format!("Could not {}: {e}", if undo { "undo" } else { "redo" })),
        }
    }

    /// Writes the document to `path` and makes the written file its new base.
    ///
    /// A normal save writes the whole document again, so content that was removed or replaced is
    /// gone from the file. Only a signed file or one with saved revisions gets an appended update
    /// instead, to keep those valid, and only when nothing was removed since the last save; when
    /// something was, the user is asked first. A protected file stays encrypted exactly as it was.
    ///
    /// `confirmed` names the document whose rewrite the user agreed to (it invalidates signatures
    /// or drops saved revisions); an answer about any other document saves nothing. `then` is
    /// carried out once the file is written.
    pub fn write_to(&mut self, path: &Path, confirmed: Option<u64>, then: AfterSave) {
        if self.is_private_location(path) {
            return self.alert(format!(
                "{} is in a folder Carbide PDF keeps for itself (recovery or temporary copies), so nothing was saved there. Choose another location.",
                path.display()
            ));
        }
        if self.doc.as_ref().is_some_and(|doc| confirmed.is_some_and(|id| id != doc.id)) {
            self.status = "Nothing was saved: the question was about a document that is no longer in front.".into();
            return;
        }
        if self.refuse_over_unsaved_tab(path) {
            return;
        }
        let others = self.other_tabs_on(path);
        let Some(doc) = self.doc.as_mut() else { return };
        let plan = match doc.pdf.save_plan(doc.saved.as_ref(), doc.content_removed) {
            Ok(plan) => plan,
            Err(e) => return self.alert(format!("Could not save {}: {e}", path.display())),
        };
        if let SavePlan::FullRewrite { breaks_signatures, drops_revisions } = plan {
            if plan.needs_confirmation() && confirmed.is_none() {
                let question = Dialog::ConfirmRewrite {
                    doc: doc.id,
                    path: path.to_path_buf(),
                    content_removed: doc.content_removed,
                    breaks_signatures,
                    drops_revisions,
                    then,
                };
                return self.open_dialog(question);
            }
        }
        let in_place = doc.path.as_deref() == Some(path);
        let result = doc.pdf.save_bytes(plan, doc.saved.as_ref()).and_then(|bytes| {
            if !(plan == SavePlan::Unchanged && in_place) {
                carbide_engine::fs::write_replacing(path, &bytes)?;
            }
            Ok(bytes)
        });
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(e) => return self.alert(format!("Could not save {}: {e}", path.display())),
        };
        doc.path = Some(path.to_path_buf());
        let reloaded = if plan == SavePlan::Unchanged && in_place {
            doc.dirty = false;
            doc.content_removed = false;
            Ok(())
        } else {
            doc.adopt_saved(bytes)
        };
        self.after_writing(path);
        // The other tabs on the file have nothing unsaved (checked above) and show the old file:
        // this document is that file now.
        for index in others.into_iter().rev() {
            self.close_tab(index);
        }
        self.status = format!("Saved {}", path.display());
        if let Err(e) = reloaded {
            return self.alert(format!("Saved {}, but could not read it back: {e}", path.display()));
        }
        match then {
            AfterSave::Nothing => {}
            AfterSave::Sign(request) => self.sign_document(*request),
            AfterSave::CloseTab => {
                if let Some(index) = self.active_tab {
                    self.close_tab(index);
                }
            }
            AfterSave::Quit => self.shell.resume_save_all(),
        }
    }

    /// Answers the save question about document `doc` with yes.
    pub fn confirm_rewrite(&mut self, doc: u64, path: &Path, then: AfterSave) {
        self.write_to(path, Some(doc), then);
    }

    /// Everything that follows writing the active document to `path`: what is shown and checked
    /// is the file on disk now, and its Auto Recovery copies are no longer needed, including the one
    /// a recovered document was opened from.
    fn after_writing(&mut self, path: &Path) {
        let Some(doc) = self.doc.as_mut() else { return };
        if let (Some(store), Some(id)) = (self.autosave.as_mut(), doc.record) {
            let _ = store.document_saved(id);
            doc.record = Some(store.begin_document(Some(path), now_unix()));
        }
        if let Some(recovered) = doc.recovered.take() {
            doc.name = None;
            if let Some(store) = self.autosave.as_mut() {
                let _ = store.remove_record(recovered.record);
            }
        }
        self.forget_page_images();
        self.shell.forget_caches();
        self.notice = signature_notice(self.doc.as_ref().expect("checked above"));
    }

    fn save(&mut self) {
        self.save_then(AfterSave::Nothing);
    }

    /// Saves the active document over its file (or asks where to, for a new one), then does
    /// `then`. A recovered document asks first whether to replace the file it was recovered for.
    /// Nothing follows when the user cancels.
    pub fn save_then(&mut self, then: AfterSave) {
        let Some(doc) = self.doc.as_ref() else { return };
        if let Some(path) = doc.path.clone() {
            return self.write_to(&path, None, then);
        }
        match doc.recovered.as_ref().and_then(|r| r.original.clone()).filter(|p| p.exists()) {
            Some(original) => self.open_dialog(Dialog::ReplaceOriginal { doc: doc.id, path: original, then }),
            None => self.save_as_then(then),
        }
    }

    /// Answers yes to replacing `path`, the file the recovered document `doc` was recovered for:
    /// it is saved over it the way Save saves, against the file as it is on disk now.
    pub fn replace_original(&mut self, doc: u64, path: &Path, then: AfterSave) {
        if self.doc.as_ref().is_none_or(|d| d.id != doc) {
            self.status = "Nothing was saved: the question was about a document that is no longer in front.".into();
            return;
        }
        if self.refuse_over_unsaved_tab(path) {
            return;
        }
        let Some(open) = self.doc.as_mut() else { return };
        let read = std::fs::read(path).map_err(EngineError::from).and_then(|bytes| SavedFile::open(bytes, None));
        match read {
            Ok((_, saved)) => {
                open.saved = Some(saved);
                self.write_to(path, None, then);
            }
            Err(e) => self.alert(format!(
                "{} could not be read ({e}), so the recovered document was not saved over it. Use Save As instead.",
                path.display()
            )),
        }
    }

    /// Says so and returns true when `path` is open in another tab with unsaved changes. Writing
    /// over it would leave that tab showing what is no longer on disk, and its next save would put
    /// that back over what was written.
    fn refuse_over_unsaved_tab(&mut self, path: &Path) -> bool {
        let unsaved = self.other_tabs_on(path).into_iter().any(|i| self.doc_at(i).is_some_and(|d| d.dirty));
        if unsaved {
            self.alert(format!(
                "{} is also open in another tab, with changes that are not saved. Save or close that tab first, \
                 or use Save As to keep both.",
                path.display()
            ));
        }
        unsaved
    }

    /// The tabs other than the active one whose document is the file `path`, in tab order.
    fn other_tabs_on(&self, path: &Path) -> Vec<usize> {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let wanted = canonical(path);
        (0..self.tabs.len())
            .filter(|&i| Some(i) != self.active_tab)
            .filter(|&i| self.doc_at(i).and_then(|d| d.path.as_deref()).is_some_and(|p| canonical(p) == wanted))
            .collect()
    }

    /// True for a place Save never writes to: the Auto Recovery folder, or a private temporary
    /// folder the app made.
    fn is_private_location(&self, path: &Path) -> bool {
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let folder = path.parent().filter(|p| !p.as_os_str().is_empty()).map(canonical);
        let recovery = self.autosave.as_ref().map(|s| canonical(s.directory()));
        carbide_engine::fs::is_private_temp_file(path) || matches!((folder, recovery), (Some(f), Some(r)) if f.starts_with(&r))
    }

    fn save_as(&mut self) {
        self.save_as_then(AfterSave::Nothing);
    }

    pub fn save_as_then(&mut self, then: AfterSave) {
        let name = self.doc.as_ref().map(OpenDoc::file_name).unwrap_or_else(|| "Untitled.pdf".into());
        if let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name(name).save_file() {
            self.write_to(&path, None, then);
        }
    }

    /// True when saving the active document has to ask first.
    pub fn save_needs_question(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| match &d.path {
            Some(_) => d.pdf.save_plan(d.saved.as_ref(), d.content_removed).is_ok_and(|plan| plan.needs_confirmation()),
            None => d.recovered.as_ref().is_some_and(|r| r.original.as_ref().is_some_and(|p| p.exists())),
        })
    }

    // ---- revisions and recovery --------------------------------------------------------

    pub fn save_revision(&mut self, label: &str) -> Result<(), String> {
        let doc = self.doc.as_mut().ok_or("No document is open.")?;
        let (Some(path), Some(saved)) = (doc.path.clone(), doc.saved.as_ref()) else {
            return Err("Save the document first, then save a revision.".into());
        };
        if saved.is_protected() {
            return Err("Revisions are not available for password-protected documents.".into());
        }
        // The base is the file as last written; pending edits are folded into the update. After a
        // removal the old file would keep the removed content in the earlier revision, so the base
        // is a clean rewrite instead, which is only done without asking when that loses nothing.
        let base = if doc.content_removed {
            let plan = doc.pdf.save_plan(Some(saved), true).map_err(|e| e.to_string())?;
            if plan.needs_confirmation() {
                return Err("Content was removed since the last save. Save the document first, then save a revision.".into());
            }
            doc.pdf.save_bytes(plan, Some(saved)).map_err(|e| e.to_string())?
        } else {
            saved.bytes().to_vec()
        };
        let updated = doc.pdf.save_revision(&base, label, &pdf_date_now()).map_err(|e| e.to_string())?;
        carbide_engine::fs::write_replacing(&path, &updated).map_err(|e| e.to_string())?;
        let reloaded = doc.adopt_saved(updated);
        doc.forget_history();
        self.after_writing(&path);
        reloaded.map_err(|e| format!("The revision was saved, but the file could not be read back: {e}"))?;
        self.status = format!("Saved revision {label:?}.");
        Ok(())
    }

    /// Opens a revision of the current file in a separate window.
    pub fn open_revision(&mut self, revision: &Revision) {
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(path) = doc.path.clone() else { return };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return self.alert(format!("Could not read {}: {e}", path.display())),
        };
        match open_revision_bytes(&bytes, revision) {
            Ok(prefix) => {
                // Handed to the new window as a private copy that it removes once read; the window
                // shows it as a new document, so its Save never writes into the temporary folder.
                let name = format!("{}.pdf", revision_display_name(&doc.title(), revision).replace(['/', ':'], "-"));
                match carbide_engine::fs::private_temp_file(&name, &prefix) {
                    Ok(copy) => self.spawn_copy_window(&copy),
                    Err(e) => self.alert(format!("Could not open the revision: {e}")),
                }
            }
            Err(e) => self.alert(e.to_string()),
        }
    }

    pub fn spawn_window(&mut self, path: &Path) {
        match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg(path).spawn()) {
            Ok(_) => self.status = format!("Opened {} in a new window.", path.display()),
            Err(e) => self.alert(format!("Could not open a new window: {e}")),
        }
    }

    /// Opens `copy`, a private temporary file, in a new window as a new document (see `open_copy`).
    fn spawn_copy_window(&mut self, copy: &Path) {
        let spawned = std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg(COPY_FLAG).arg(copy).spawn());
        let name = copy.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match spawned {
            Ok(_) => self.status = format!("Opened {name} in a new window."),
            Err(e) => {
                let _ = carbide_engine::fs::remove_private_temp_file(copy);
                self.alert(format!("Could not open a new window: {e}"));
            }
        }
    }

    pub fn renderer(&self) -> Option<&'static Renderer> {
        self.renderer
    }

    // ---- auto save ---------------------------------------------------------------------

    /// True when some open document has unsaved changes.
    pub fn has_unsaved_changes(&self) -> bool {
        self.doc.iter().chain(self.tabs.iter().flatten()).any(|d| d.dirty)
    }

    /// Lets go of every document as the app ends without its window being closed. A document with
    /// unsaved changes, which only gets here when something ended the app without the quit
    /// question, gets a last Auto Recovery copy and keeps its record, so it is offered next time.
    fn release_on_exit(&mut self) {
        self.park_active();
        let Some(store) = self.autosave.as_mut() else { return };
        for doc in self.tabs.drain(..).flatten() {
            let Some(id) = doc.record else { continue };
            let mut copy = doc.pdf.clone();
            let kept = doc.dirty && !doc.is_protected() && store.autosave(id, &mut copy, now_unix()).is_ok();
            if !kept {
                let _ = store.document_closed(id);
            }
        }
        self.active_tab = None;
    }

    /// Writes the Auto Recovery copy of each document whose own interval has passed.
    fn autosave_tick(&mut self) {
        if !self.settings.enabled {
            return;
        }
        let interval = self.settings.interval();
        let Some(store) = self.autosave.as_mut() else { return };
        let mut saved = 0;
        let mut failure = None;
        for doc in self.doc.iter_mut().chain(self.tabs.iter_mut().flatten()) {
            if doc.autosaved_at.elapsed() < interval {
                continue;
            }
            doc.autosaved_at = Instant::now();
            let Some(id) = doc.record else { continue };
            // Its recovery copy would hold the content unencrypted.
            if doc.is_protected() {
                continue;
            }
            if carbide_engine::autosave::can_do_autosave(doc.dirty, false) {
                let mut copy = doc.pdf.clone();
                match store.autosave(id, &mut copy, now_unix()) {
                    Ok(()) => saved += 1,
                    Err(e) => failure = Some(e.to_string()),
                }
            }
        }
        if let Some(e) = failure {
            self.status = format!("Auto Recovery failed: {e}");
        } else if saved > 0 {
            self.status = "Auto Recovery copy saved.".into();
        }
    }

    // ---- UI ----------------------------------------------------------------------------

    fn thumbnail_panel(&mut self, ui: &mut egui::Ui) {
        let (Some(doc), Some(renderer)) = (self.doc.as_ref(), self.renderer) else { return };
        let version = doc.version;
        let count = doc.pdf.page_count();
        let bytes = &doc.bytes;
        let geometries = &doc.geometries;
        let drawable = &mut self.drawable;
        let mut goto: Option<usize> = None;
        let mut shown = std::collections::HashSet::new();
        let max_side = max_texture_side(ui.ctx().input(|i| i.max_texture_side));
        egui::ScrollArea::vertical().show(ui, |ui| {
            for page in 0..count {
                let (w, h) = layout_size(geometries.get(page).map(|g| g.display_size()).unwrap_or((612.0, 792.0)));
                let size = thumbnail_size((w, h));
                let (column, response) = ui.allocate_exact_size(egui::vec2(THUMBNAIL_WIDTH, size.y), egui::Sense::click());
                let rect = egui::Rect::from_center_size(column.center(), size);
                if ui.is_rect_visible(rect) {
                    let key = (version, page);
                    shown.insert(key);
                    if !self.thumbs.contains_key(&key) && page_drawable(drawable, doc, page) {
                        let [tw, th] = texture_size((w, h), size.x * ui.ctx().pixels_per_point(), max_side);
                        let texture = renderer
                            .render_page_fitting(bytes, page, tw, th)
                            .ok()
                            .and_then(|image| page_texture(ui.ctx(), format!("thumb-{version}-{page}"), &image, max_side));
                        if let Some(texture) = texture {
                            self.thumbs.insert(key, texture);
                        }
                    }
                    if let Some(texture) = self.thumbs.get(&key) {
                        ui.painter().image(
                            texture.id(),
                            rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    } else {
                        ui.painter().rect_filled(rect, 0.0, egui::Color32::WHITE);
                    }
                    let outline = if page == self.current_page { theme::ACCENT } else { egui::Color32::GRAY };
                    ui.painter().rect_stroke(rect, 0.0, egui::Stroke::new(1.5, outline), egui::StrokeKind::Outside);
                }
                ui.label(format!("{}", page + 1));
                if response.clicked() {
                    goto = Some(page);
                }
            }
        });
        // Thumbnails scrolled out of view go once they hold more than their share of memory.
        if !thumbnails_fit(self.thumbs.values().map(|texture| texture.size())) {
            self.thumbs.retain(|key, _| shown.contains(key));
        }
        if let Some(page) = goto {
            self.scroll_to = Some(page);
            self.current_page = page;
        }
    }

    fn page_view(&mut self, ui: &mut egui::Ui) {
        let Some(renderer) = self.renderer else {
            ui.centered_and_justified(|ui| {
                ui.label(self.renderer_error.clone().unwrap_or_else(|| "PDF rendering is unavailable.".into()));
            });
            return;
        };
        if self.doc.is_none() {
            return;
        }

        // Fit modes and pinch / ctrl-scroll zoom adjust the zoom before the pages are laid out.
        let pinch = ui.ctx().input(|i| i.zoom_delta());
        if (pinch - 1.0).abs() > 1e-3 && ui.rect_contains_pointer(ui.max_rect()) {
            self.zoom = (self.zoom * pinch).clamp(shell::MIN_ZOOM, shell::MAX_ZOOM);
            self.shell.fit = shell::Fit::Manual;
        }
        if self.shell.fit != shell::Fit::Manual {
            if let Some(g) = self.doc.as_ref().and_then(|d| d.geometries.get(self.current_page)) {
                let (w, h) = layout_size(g.display_size());
                let avail = ui.available_size();
                let by_width = (avail.x - 48.0) / w.max(1.0);
                let by_height = (avail.y - 24.0) / h.max(1.0);
                let fitted = if self.shell.fit == shell::Fit::Width { by_width } else { by_width.min(by_height) };
                self.zoom = fitted.clamp(shell::MIN_ZOOM, shell::MAX_ZOOM);
            }
        }

        let mut actions: Vec<Action> = Vec::new();
        let tool = self.tool;
        let zoom = self.zoom;
        let scroll_to = self.scroll_to.take();
        let mut drag = self.drag;
        {
            let doc = self.doc.as_ref().expect("checked above");
            let (version, count) = (doc.version, doc.pdf.page_count());
            let bytes = &doc.bytes;
            let geometries = &doc.geometries;
            let pending = &self.pending_redactions;
            let textures = &mut self.textures;
            let drawable = &mut self.drawable;
            let pixels_per_point = ui.ctx().pixels_per_point();
            let max_side = max_texture_side(ui.ctx().input(|i| i.max_texture_side));
            let viewport_top = ui.clip_rect().min.y;

            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    for page in 0..count {
                        let (w, h) = layout_size(geometries.get(page).map(|g| g.display_size()).unwrap_or((612.0, 792.0)));
                        let size = egui::vec2(w * zoom, h * zoom);
                        let sense = if tool == Tool::Select { egui::Sense::hover() } else { egui::Sense::click_and_drag() };
                        let (rect, response) = ui.allocate_exact_size(size, sense);

                        if scroll_to == Some(page) {
                            ui.scroll_to_rect(rect, Some(egui::Align::TOP));
                        }
                        if rect.min.y <= viewport_top + 40.0 && rect.max.y > viewport_top + 40.0 {
                            actions.push(Action::Visible(page));
                        }

                        ui.painter().rect_filled(rect.translate(egui::vec2(3.0, 3.0)), 0.0, egui::Color32::from_black_alpha(40));
                        ui.painter().rect_filled(rect, 0.0, egui::Color32::WHITE);

                        if ui.is_rect_visible(rect) && !page_drawable(drawable, doc, page) {
                            ui.painter().text(
                                rect.center_top() + egui::vec2(0.0, 40.0),
                                egui::Align2::CENTER_CENTER,
                                "This page is too complex to show safely.",
                                egui::FontId::proportional(14.0),
                                egui::Color32::DARK_GRAY,
                            );
                        } else if ui.is_rect_visible(rect) {
                            let [tw, th] = texture_size((w, h), size.x * pixels_per_point, max_side);
                            let key = (version, page, tw);
                            if !textures.contains_key(&key) {
                                let cached: usize = textures.values().map(|t| t.size()[0] * t.size()[1]).sum();
                                if textures.len() > 24 || cached + (tw * th) as usize > TEXTURE_CACHE_PIXELS {
                                    textures.clear();
                                }
                                let texture = renderer
                                    .render_page_fitting(bytes, page, tw, th)
                                    .ok()
                                    .and_then(|image| page_texture(ui.ctx(), format!("page-{version}-{page}-{tw}"), &image, max_side));
                                if let Some(texture) = texture {
                                    textures.insert(key, texture);
                                }
                            }
                            if let Some(texture) = textures.get(&key) {
                                ui.painter().image(
                                    texture.id(),
                                    rect,
                                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                    egui::Color32::WHITE,
                                );
                            }

                            // Marked redactions are shown until applied.
                            for mark in pending.iter().filter(|r| r.page == page) {
                                let r = pdf_rect_to_screen(rect, zoom, h, mark.rect);
                                ui.painter().rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(220, 30, 30, 90));
                                ui.painter().rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::RED), egui::StrokeKind::Inside);
                            }
                        }

                        // Pointer interaction in page coordinates (points, origin bottom-left).
                        let to_page = |pos: egui::Pos2| ((pos.x - rect.min.x) / zoom, (rect.max.y - pos.y) / zoom);
                        match tool {
                            Tool::Highlight | Tool::HighlightArea | Tool::Underline | Tool::StrikeOut | Tool::Redact | Tool::Sign => {
                                if response.drag_started() {
                                    if let Some(pos) = response.interact_pointer_pos() {
                                        let p = to_page(pos);
                                        drag = Some(DragState { page, start: p, current: p });
                                    }
                                }
                                if response.dragged() {
                                    if let (Some(state), Some(pos)) = (drag.as_mut(), response.interact_pointer_pos()) {
                                        if state.page == page {
                                            state.current = to_page(pos);
                                        }
                                    }
                                }
                                if let Some(state) = drag.filter(|s| s.page == page) {
                                    let region = Rect::from_corners(state.start, state.current);
                                    let r = pdf_rect_to_screen(rect, zoom, h, region);
                                    ui.painter().rect_stroke(r, 0.0, egui::Stroke::new(1.5, theme::ACCENT), egui::StrokeKind::Outside);
                                }
                                if response.drag_stopped() {
                                    if let Some(state) = drag.take().filter(|s| s.page == page) {
                                        let region = Rect::from_corners(state.start, state.current);
                                        if region.width > 3.0 && region.height > 3.0 {
                                            match tool {
                                                Tool::Redact => actions.push(Action::MarkRedaction { page, rect: region }),
                                                Tool::Sign => actions.push(Action::SignArea { page, rect: region }),
                                                Tool::Highlight => {
                                                    actions.push(Action::Markup { page, rect: region, style: MarkupStyle::Highlight, snap: true })
                                                }
                                                Tool::HighlightArea => {
                                                    actions.push(Action::Markup { page, rect: region, style: MarkupStyle::Highlight, snap: false })
                                                }
                                                Tool::Underline => {
                                                    actions.push(Action::Markup { page, rect: region, style: MarkupStyle::Underline, snap: true })
                                                }
                                                _ => actions.push(Action::Markup { page, rect: region, style: MarkupStyle::StrikeOut, snap: true }),
                                            }
                                        }
                                    }
                                }
                            }
                            Tool::Note | Tool::TextBox => {
                                if response.clicked() {
                                    if let Some(pos) = response.interact_pointer_pos() {
                                        let (x, y) = to_page(pos);
                                        actions.push(Action::NotePrompt { page, x, y, on_page: tool == Tool::TextBox });
                                    }
                                }
                            }
                            Tool::Select => {}
                        }
                        ui.add_space(10.0);
                    }
                });
            });
        }
        self.drag = drag;

        let doc_id = self.doc.as_ref().map(|d| d.id).unwrap_or_default();
        for action in actions {
            match action {
                Action::SignArea { page, rect } => {
                    self.open_dialog(Dialog::sign(doc_id, Some((page, rect)), false));
                    self.tool = Tool::Select;
                }
                Action::Visible(page) => self.current_page = page,
                Action::MarkRedaction { page, rect } => {
                    self.pending_redactions.push(Redaction { page, rect });
                    self.status = "Redaction marked. Security > Apply removes the marked content.".into();
                }
                Action::NotePrompt { page, x, y, on_page } => {
                    self.open_dialog(Dialog::Note { doc: doc_id, page, x, y, on_page, text: String::new() });
                }
                Action::Markup { page, rect, style, snap } => {
                    let (bytes, geometry, readable) = match self.doc.as_ref() {
                        Some(d) => (d.bytes.clone(), d.geometries.get(page).copied(), page_drawable(&mut self.drawable, d, page)),
                        None => continue,
                    };
                    // Reading the text of a page that cannot be rendered safely is no safer.
                    let snap = snap && readable;
                    // Snap to the text lines inside the dragged area; fall back to the area itself.
                    let lines = match (snap, geometry) {
                        (true, Some(g)) => match renderer.text_line_rects(&bytes, page, &g, rect) {
                            Ok(lines) if !lines.is_empty() => lines,
                            _ => vec![rect],
                        },
                        _ => vec![rect],
                    };
                    self.edit("Markup", move |pdf| pdf.add_markup(page, &lines, style, None));
                }
            }
        }
    }

    // ---- OCR ---------------------------------------------------------------------------

    /// Recognizes text on a background thread; the result is applied in `poll_ocr`.
    pub fn start_ocr(&mut self) {
        let (Some(renderer), Some(doc)) = (self.renderer, self.doc.as_ref()) else { return };
        if self.ocr_job.is_some() {
            return;
        }
        let (id, version) = (doc.id, doc.version);
        let mut copy = doc.pdf.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = copy
                .make_searchable(renderer, &carbide_engine::ocr::OcrOptions::default())
                .map(|report| (copy, report))
                .map_err(|e| e.to_string());
            let _ = sender.send(result);
        });
        self.ocr_job = Some(OcrJob { doc: id, version, receiver, result: None });
        self.status = "Recognizing text...".into();
    }

    /// Applies a finished OCR job to the document it ran on. The result is a whole new copy of
    /// that document, so it is dropped when the document changed in the meantime (a redaction
    /// would come back otherwise), and it waits while another document is in front.
    fn poll_ocr(&mut self) {
        let Some(job) = self.ocr_job.as_mut() else { return };
        if job.result.is_none() {
            match job.receiver.try_recv() {
                Ok(result) => job.result = Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.ocr_job = None;
                    return self.alert("OCR stopped unexpectedly.".into());
                }
            }
        }
        let (doc, version) = (job.doc, job.version);
        let active = self.doc.as_ref().map(|d| (d.id, d.version));
        if active.map(|(id, _)| id) != Some(doc) {
            let still_open = (0..self.tabs.len()).any(|i| self.doc_at(i).is_some_and(|d| d.id == doc));
            if !still_open {
                self.ocr_job = None;
            }
            return;
        }
        let Some(OcrJob { result: Some(result), .. }) = self.ocr_job.take() else { return };
        match result {
            Ok((pdf, report)) if active == Some((doc, version)) => {
                let message = format!(
                    "OCR: {} page(s) recognized, {} already had text.",
                    report.recognized_pages.len(),
                    report.skipped_pages.len()
                );
                self.edit("Recognize text", move |target| {
                    *target = pdf;
                    Ok(())
                });
                self.status = message;
            }
            Ok(_) => {
                self.status = "The recognized text was not added: the document changed while OCR ran. Run OCR again.".into();
            }
            Err(message) => self.alert(format!("OCR failed: {message}")),
        }
    }

    // ---- signatures --------------------------------------------------------------------

    /// Signs the document file in place. The document is saved first when it has unsaved edits.
    /// Nothing is signed when the document the request was made for is no longer in front.
    pub fn sign_document(&mut self, request: dialogs::SignRequest) {
        if self.doc.as_ref().map(|d| d.id) != Some(request.doc) {
            self.status = "Nothing was signed: the document it was for is no longer in front.".into();
            return;
        }
        if self.doc.as_ref().and_then(|d| d.saved.as_ref()).is_some_and(SavedFile::is_protected) {
            return self.alert("Password-protected documents cannot be signed.".into());
        }
        let needs_save = self.doc.as_ref().is_some_and(|d| d.dirty || d.path.is_none() || d.saved.is_none());
        if needs_save {
            // Signing follows once the file is saved, which may first ask whether to rewrite it. A
            // failed or cancelled save has already said why.
            return self.save_then(AfterSave::Sign(Box::new(request)));
        }
        let Some(doc) = self.doc.as_mut() else { return };
        let (Some(path), Some(saved)) = (doc.path.clone(), doc.saved.as_ref()) else {
            return self.alert("The document must be saved before it can be signed.".into());
        };
        let base = saved.bytes().to_vec();
        let identity = match request.load_identity() {
            Ok(i) => i,
            Err(message) => return self.alert(message),
        };
        let done = match &identity {
            Some(id) => format!("Signed {} as {}", path.display(), id.common_name()),
            None => format!("Time stamped {}", path.display()),
        };
        let Some(authority) = request.timestamp_source() else {
            let signed = doc.pdf.sign(&base, &request.options(identity.as_ref(), None)).map_err(|e| e.to_string());
            return self.finish_signing(&path, done, signed);
        };
        // A time stamp authority can take a while to answer, so the window stays responsive: the
        // signature is made on a copy, and written in `poll_sign` when it is ready.
        if self.sign_job.is_some() {
            return self.alert("Another signature is still waiting for its time stamp.".into());
        }
        let (id, version, copy) = (doc.id, doc.version, doc.pdf.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let options = request.options(identity.as_ref(), Some(&authority));
            let _ = sender.send(copy.sign(&base, &options).map_err(|e| e.to_string()));
        });
        self.sign_job = Some(SignJob { doc: id, version, path, done, receiver, result: None });
        self.status = "Waiting for the time stamp authority...".into();
    }

    /// Writes signed bytes over the active document's file and reloads the document from it.
    fn finish_signing(&mut self, path: &Path, done: String, signed: Result<Vec<u8>, String>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let result = signed.and_then(|bytes| {
            carbide_engine::fs::write_replacing(path, &bytes).map_err(|e| e.to_string())?;
            doc.forget_history();
            Ok(doc.adopt_saved(bytes).map_err(|e| format!("Signed {}, but could not read it back: {e}", path.display())))
        });
        let result = match result {
            Ok(reloaded) => {
                self.after_writing(path);
                reloaded
            }
            Err(message) => Err(message),
        };
        match result {
            Ok(()) => self.status = done,
            Err(message) => self.alert(message),
        }
    }

    /// Writes a signature whose time stamp has arrived. It replaces the file only while its
    /// document is unchanged since signing began (an edit or save in the meantime would be lost
    /// otherwise), and it waits while another document is in front.
    fn poll_sign(&mut self) {
        let Some(job) = self.sign_job.as_mut() else { return };
        if job.result.is_none() {
            match job.receiver.try_recv() {
                Ok(result) => job.result = Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.sign_job = None;
                    return self.alert("Signing stopped unexpectedly.".into());
                }
            }
        }
        let (doc, version) = (job.doc, job.version);
        let active = self.doc.as_ref().map(|d| (d.id, d.version));
        if active.map(|(id, _)| id) != Some(doc) {
            let still_open = (0..self.tabs.len()).any(|i| self.doc_at(i).is_some_and(|d| d.id == doc));
            if !still_open {
                self.sign_job = None;
                self.status = "The document was closed before its time stamp arrived, so it was not signed.".into();
            }
            return;
        }
        let Some(SignJob { path, done, result: Some(result), .. }) = self.sign_job.take() else { return };
        match result {
            Ok(bytes) if active == Some((doc, version)) => self.finish_signing(&path, done, Ok(bytes)),
            Ok(_) => self.alert("The document was not signed: it changed while waiting for the time stamp. Sign it again.".into()),
            Err(message) => self.alert(message),
        }
    }

    /// Checks the signatures of the active document's file. With unsaved changes, the results say
    /// they are about that file, and that the changes are not signed.
    pub fn validate_signatures(&mut self) {
        let Some(doc) = self.doc.as_ref() else { return };
        let (id, unsaved) = (doc.id, doc.dirty);
        let (bytes, password) = signed_file(doc);
        match Pdf::verify_signatures_with_password(bytes, password, &paths::load_trust_store(), chrono::Utc::now()) {
            Ok(reports) if reports.is_empty() => self.alert("This document has no signatures.".into()),
            Ok(reports) => {
                let notice = carbide_engine::signature::notice_text(&reports).unwrap_or_default().to_string();
                self.notice = Some(notice.clone());
                let notice = about_saved_file(notice, unsaved);
                self.open_dialog(Dialog::Signatures { doc: id, reports, notice });
            }
            Err(e) => self.alert(format!("Could not read the signatures: {e}")),
        }
    }

    /// The notice bar text: the signature summary of the active document's file. While the
    /// document has unsaved changes, after an edit, undo, redo or OCR, it also says that the
    /// summary is about the document as last saved or opened, and that no signature covers those
    /// changes; saving brings back the summary alone.
    pub fn shown_notice(&self) -> Option<String> {
        let notice = self.notice.clone()?;
        Some(about_saved_file(notice, self.doc.as_ref().is_some_and(|d| d.dirty)))
    }

    // ---- file helpers ------------------------------------------------------------------

    /// Exports the active document as images or text. The exported files are not protected, so
    /// for a protected document the user is asked first.
    pub fn export(&mut self, export: Export) {
        let Some(doc) = self.doc.as_ref() else { return };
        if doc.is_protected() {
            self.open_dialog(Dialog::ConfirmExport { doc: doc.id, export });
            return;
        }
        self.export_confirmed(doc.id, export);
    }

    /// Carries on exporting the document `doc`, once exporting it unprotected was agreed to.
    pub fn export_confirmed(&mut self, doc: u64, export: Export) {
        if self.doc.as_ref().map(|d| d.id) != Some(doc) {
            self.status = "Nothing was exported: the question was about a document that is no longer in front.".into();
            return;
        }
        if self.renderer.is_none() {
            return;
        }
        match export {
            Export::Images => {
                let Some(folder) = rfd::FileDialog::new().set_title("Export Here").pick_folder() else { return };
                self.export_images_to(folder, None);
            }
            Export::Text => self.export_text(),
        }
    }

    /// Exports every page of the active document as an image into `folder`. When files of those
    /// names are already there and `existing` does not say what to do with them, the user is asked.
    pub fn export_images_to(&mut self, folder: PathBuf, existing: Option<Existing>) {
        let (Some(renderer), Some(doc)) = (self.renderer, self.doc.as_mut()) else { return };
        let base = doc.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "page".into());
        let format = carbide_engine::ImageFormat::Png;
        let existing = match existing {
            Some(existing) => existing,
            None => {
                let taken = taken(carbide_engine::image_paths(&doc.pdf, &folder, &base, format));
                if !taken.is_empty() {
                    let question = Dialog::ExistingFiles { doc: doc.id, writing: Writing::Images { folder }, taken };
                    return self.open_dialog(question);
                }
                Existing::Refuse
            }
        };
        match carbide_engine::export_images(renderer, &mut doc.pdf, &folder, &base, format, 150.0, existing) {
            Ok(paths) => self.status = format!("Exported {} image(s).", paths.len()),
            Err(e) => self.alert(format!("Export failed: {e}")),
        }
    }

    /// Writes each of `ranges` of the active document as a file of its own into `folder`, asking
    /// first when files of those names are already there and `existing` does not say what to do.
    pub fn split_into(&mut self, folder: PathBuf, ranges: &str, existing: Option<Existing>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let parsed = match Pdf::parse_ranges(ranges, doc.pdf.page_count()) {
            Ok(r) => r,
            Err(e) => return self.alert(e.to_string()),
        };
        let stem = doc.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let targets: Vec<PathBuf> =
            parsed.iter().map(|r| folder.join(format!("{stem}-pages-{}-{}.pdf", r.start() + 1, r.end() + 1))).collect();
        let mut seen = std::collections::HashSet::new();
        if let Some(repeated) = targets.iter().find(|t| !seen.insert(*t)) {
            let name = repeated.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return self.alert(format!("The ranges would write {name} twice. Give each range once."));
        }
        let existing = match existing {
            Some(existing) => existing,
            None => {
                let taken = taken(targets.clone());
                if !taken.is_empty() {
                    let writing = Writing::Split { folder, ranges: ranges.to_string() };
                    self.open_dialog(Dialog::ExistingFiles { doc: doc.id, writing, taken });
                    return;
                }
                Existing::Refuse
            }
        };
        let mut written = 0;
        for (range, target) in parsed.iter().zip(&targets) {
            let result = doc
                .pdf
                .extract_pages(&range.clone().collect::<Vec<_>>())
                .and_then(|mut part| doc.derived_bytes(&mut part))
                .and_then(|bytes| Ok(carbide_engine::fs::write_file(target, &bytes, existing)?));
            if let Err(e) = result {
                let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return self.alert(format!("Could not write {name}: {e}"));
            }
            written += 1;
        }
        self.status = format!("Wrote {written} file(s) to {}", folder.display());
    }

    /// Carries on writing files into a folder, once the user said what happens to files already
    /// there. Nothing is written when the question was about a document no longer in front.
    pub fn continue_writing(&mut self, doc: u64, writing: Writing, existing: Existing) {
        if self.doc.as_ref().map(|d| d.id) != Some(doc) {
            self.status = "Nothing was written: the question was about a document that is no longer in front.".into();
            return;
        }
        match writing {
            Writing::Images { folder } => self.export_images_to(folder, Some(existing)),
            Writing::Split { folder, ranges } => self.split_into(folder, &ranges, Some(existing)),
        }
    }

    fn export_text(&mut self) {
        let (Some(renderer), Some(doc)) = (self.renderer, self.doc.as_mut()) else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("Text", &["txt"]).set_file_name("document.txt").save_file() else { return };
        let written = carbide_engine::export_text(renderer, &mut doc.pdf)
            .and_then(|text| Ok(carbide_engine::fs::write_replacing(&path, text.as_bytes())?));
        match written {
            Ok(()) => self.status = format!("Exported text to {}", path.display()),
            Err(e) => self.alert(format!("Export failed: {e}")),
        }
    }

    fn merge_files(&mut self) {
        let Some(files) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).pick_files() else { return };
        let mut others = Vec::new();
        for file in &files {
            match Pdf::open(file) {
                Ok(p) => others.push(p),
                Err(e) => return self.alert(format!("{}: {e}", file.display())),
            }
        }
        let names = files.iter().map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
        self.merge_into_current(others, names, false);
    }

    /// Appends `others` (named `names`) to the active document, which keeps its own protection.
    /// The pages of a protected file that does not share it would be saved with this document's
    /// protection or none, so the user is asked first; `confirmed` says that was agreed to.
    pub fn merge_into_current(&mut self, mut others: Vec<Pdf>, names: Vec<String>, confirmed: bool) {
        let Some(doc) = self.doc.as_ref() else { return };
        let (protection, _) = doc.protection();
        let differs = |other: &Pdf| other.protection().is_some_and(|p| !protection.as_ref().is_some_and(|own| own.same_as(&p)));
        if !confirmed && others.iter().any(differs) {
            let protected =
                others.iter().zip(&names).filter(|(other, _)| differs(other)).map(|(_, name)| name.clone()).collect();
            self.open_dialog(Dialog::ConfirmMerge { doc: doc.id, others, names, protected, keeps_protection: protection.is_some() });
            return;
        }
        for other in others.iter_mut().filter(|other| differs(other)) {
            other.set_protection(None);
        }
        self.edit("Merge", move |pdf| {
            pdf.set_protection(protection);
            let mut all = vec![pdf.clone()];
            all.extend(others);
            let refs: Vec<&Pdf> = all.iter().collect();
            *pdf = Pdf::merge(&refs)?;
            Ok(())
        });
    }

    fn extract_current(&mut self) {
        let page = self.current_page;
        if self.doc.is_none() {
            return;
        }
        let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name(format!("Page {}.pdf", page + 1)).save_file() else { return };
        self.extract_page_to(page, &path);
    }

    /// Writes page `page` of the active document to `path` as a new file, protected like the
    /// document.
    pub fn extract_page_to(&mut self, page: usize, path: &Path) {
        let Some(doc) = self.doc.as_ref() else { return };
        let written = doc
            .pdf
            .extract_pages(&[page])
            .and_then(|mut part| doc.derived_bytes(&mut part))
            .and_then(|bytes| Ok(carbide_engine::fs::write_replacing(path, &bytes)?));
        match written {
            Ok(()) => {
                let protected = if doc.is_protected() { ", password-protected like this document" } else { "" };
                self.status = format!("Extracted page {} to {}{protected}", page + 1, path.display());
            }
            Err(e) => self.alert(format!("Could not extract: {e}")),
        }
    }

    fn apply_redactions(&mut self) {
        let Some(renderer) = self.renderer else { return };
        if self.pending_redactions.is_empty() {
            return;
        }
        // The marks stay until the redaction is made, so a failed one loses none of them.
        let redactions = self.pending_redactions.clone();
        if self.run_edit("Apply redactions", true, move |pdf| pdf.apply_redactions(renderer, &redactions, 200.0)) {
            self.pending_redactions.clear();
            // The same text may still be elsewhere in the document, so offer to look.
            if let Some(doc) = self.doc.as_ref() {
                self.open_dialog(Dialog::SuggestRemoveHidden { doc: doc.id });
            }
        }
    }

    /// Takes `items` of hidden information out of document `doc`, as an edit that removes content,
    /// and says what was found. Nothing happens when `doc` is no longer in front.
    pub fn remove_hidden_information(&mut self, doc: u64, items: Vec<carbide_engine::HiddenItem>) {
        if self.doc.as_ref().map(|d| d.id) != Some(doc) {
            self.status = "Nothing was removed: the document is no longer in front.".into();
            return;
        }
        // What only the file on disk holds is reported too; the save that has to follow leaves it out.
        let file = self.doc.as_ref().and_then(|d| d.saved.clone());
        let was_dirty = self.doc.as_ref().is_some_and(|d| d.dirty);
        let mut report = None;
        // Removing content always counts as an edit, so an empty report is undone below.
        self.edit_removing("Remove hidden information", |pdf| {
            report = Some(pdf.remove_hidden_information_from(&items, file.as_ref())?);
            Ok(())
        });
        match report {
            Some(report) if report.is_empty() => {
                // Nothing was found, so this was no edit: the document is put back exactly as it was,
                // and a save has nothing to rewrite (which could cost a signature).
                if let Some(doc) = self.doc.as_mut() {
                    if let Some(snapshot) = doc.undo.pop() {
                        let _ = doc.restore(&snapshot.bytes);
                        doc.content_removed = snapshot.content_removed;
                    }
                    doc.dirty = was_dirty;
                }
                self.status = "No hidden information was found.".into();
            }
            Some(report) => {
                let lines = report.lines().iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n");
                self.alert(format!("Removed from the document:\n{lines}\n\nSave the document to write it without them."));
            }
            None => {}
        }
    }

    fn compare_with(&mut self) {
        let (Some(renderer), Some(doc)) = (self.renderer, self.doc.as_ref()) else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_title("Choose the revised version").pick_file() else { return };
        let mut revised = match Pdf::open(&path) {
            Ok(p) => p,
            Err(e) => return self.alert(format!("{}: {e}", path.display())),
        };
        let mut original = doc.pdf.clone();
        match carbide_engine::compare_documents(renderer, &mut original, &mut revised) {
            Ok(diffs) => {
                let title = format!("{} -> {}", doc.title(), path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                self.open_dialog(Dialog::Compare { title, diffs });
            }
            Err(e) => self.alert(format!("Compare failed: {e}")),
        }
    }
}

/// Where `rect` on page `page` (display space) is after the pages changed by `change`, `before`
/// being the page geometry from before; `None` when its page was deleted.
fn follow(change: PageChange, before: &[PageGeometry], page: usize, rect: Rect) -> Option<(usize, Rect)> {
    let page = match change {
        PageChange::Inserted(at) if page >= at => page + 1,
        PageChange::Deleted(gone) if page == gone => return None,
        PageChange::Deleted(gone) if page > gone => page - 1,
        PageChange::Moved { from, to } if page == from => to,
        PageChange::Moved { from, to } if from < to && (from + 1..=to).contains(&page) => page - 1,
        PageChange::Moved { from, to } if to < from && (to..from).contains(&page) => page + 1,
        _ => page,
    };
    let rect = match change {
        PageChange::Turned { page: turned, clockwise } if turned == page => {
            let (w, h) = before.get(page)?.display_size();
            if clockwise {
                Rect::new(rect.y, w - rect.max_x(), rect.height, rect.width)
            } else {
                Rect::new(h - rect.max_y(), rect.x, rect.height, rect.width)
            }
        }
        _ => rect,
    };
    Some((page, rect))
}

/// Those of `paths` that are already taken.
fn taken(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|path| path.symlink_metadata().is_ok()).collect()
}

/// The bytes whose signatures are checked for `doc`, the file it came from when it has one, with
/// the password that file needs.
fn signed_file(doc: &OpenDoc) -> (&[u8], Option<&str>) {
    match &doc.saved {
        Some(saved) => (saved.bytes(), saved.password()),
        None => (&doc.bytes, None),
    }
}

/// Said after what is known about the signatures of a document with unsaved changes.
pub const UNSAVED_CHANGES_NOT_SIGNED: &str =
    "This is about the document as last saved or opened. Your unsaved changes are not covered by any signature.";

/// `notice`, a summary of a document's signatures, with [`UNSAVED_CHANGES_NOT_SIGNED`] after it when
/// the document has `unsaved` changes.
fn about_saved_file(notice: String, unsaved: bool) -> String {
    if unsaved {
        format!("{notice} {UNSAVED_CHANGES_NOT_SIGNED}")
    } else {
        notice
    }
}

/// Notice bar text for a document that carries signatures.
fn signature_notice(doc: &OpenDoc) -> Option<String> {
    if !doc.pdf.has_signatures() {
        return None;
    }
    let (bytes, password) = signed_file(doc);
    let trust = paths::load_trust_store();
    let reports = Pdf::verify_signatures_with_password(bytes, password, &trust, chrono::Utc::now()).ok()?;
    carbide_engine::signature::notice_text(&reports).map(str::to_string)
}

/// Whether page `page` of `doc` can be rendered safely (see [`Pdf::check_drawable`]), worked out
/// once for each version of the document.
fn page_drawable(drawable: &mut HashMap<(u64, usize), bool>, doc: &OpenDoc, page: usize) -> bool {
    *drawable.entry((doc.version, page)).or_insert_with(|| doc.pdf.check_drawable(page).is_ok())
}

/// The longest side a page is laid out with, in points: as far as page boxes reach.
const MAX_LAYOUT_SIDE: f32 = 2.0 * carbide_engine::pagetree::MAX_COORDINATE;

/// The size a page displayed `page` (width, height) points large is laid out at: each side a
/// number from 1 to [`MAX_LAYOUT_SIDE`], whatever the file says, so that layout never meets a
/// size that is not a number.
pub fn layout_size(page: (f32, f32)) -> (f32, f32) {
    let side = |v: f32| if v.is_nan() { 1.0 } else { v.clamp(1.0, MAX_LAYOUT_SIDE) };
    (side(page.0), side(page.1))
}

/// How wide the thumbnail column is, in points.
const THUMBNAIL_WIDTH: f32 = 110.0;

/// The size a thumbnail of a page laid out `page` (width, height) points large is shown at: the
/// width of the column, or less for a page more than four times as tall as it is wide, so that a
/// strip of a page does not take the whole panel.
pub fn thumbnail_size(page: (f32, f32)) -> egui::Vec2 {
    let (w, h) = layout_size(page);
    let scale = (THUMBNAIL_WIDTH / w).min(4.0 * THUMBNAIL_WIDTH / h);
    egui::vec2((w * scale).clamp(2.0, THUMBNAIL_WIDTH), (h * scale).clamp(2.0, 4.0 * THUMBNAIL_WIDTH))
}

/// The most pixels the thumbnail images kept for redrawing hold together: 64 MB.
const THUMBNAIL_CACHE_PIXELS: usize = 16 << 20;

/// True while thumbnail images of `sizes` hold no more than [`THUMBNAIL_CACHE_PIXELS`].
pub fn thumbnails_fit(sizes: impl Iterator<Item = [usize; 2]>) -> bool {
    sizes.map(|[w, h]| w * h).sum::<usize>() <= THUMBNAIL_CACHE_PIXELS
}

/// The texture side assumed when egui does not say what the GPU takes; every GPU egui runs on
/// takes at least this much.
const FALLBACK_TEXTURE_SIDE: usize = 8192;
/// The most pixels one page texture has: 16 million, 64 MB.
const MAX_TEXTURE_PIXELS: f32 = 16_777_216.0;
/// The most pixels the page textures kept for redrawing hold together: 512 MB.
const TEXTURE_CACHE_PIXELS: usize = 128 << 20;

/// The largest texture side egui says the GPU takes (`reported`), or [`FALLBACK_TEXTURE_SIDE`]
/// when it says nothing.
pub fn max_texture_side(reported: usize) -> usize {
    if reported == 0 {
        FALLBACK_TEXTURE_SIDE
    } else {
        reported
    }
}

/// The size, in pixels, of the texture a page displayed `page` (width, height) points large is
/// drawn from when it is shown `wanted_width` pixels wide. It is at most 5000 pixels wide, no side
/// is longer than `max_side`, and it holds at most [`MAX_TEXTURE_PIXELS`]: a very tall page, or a
/// large zoom, gets a texture of lower resolution, drawn stretched over the page's place. The
/// page's shape is kept, as far as whole pixels allow.
pub fn texture_size(page: (f32, f32), wanted_width: f32, max_side: usize) -> [u32; 2] {
    let positive = |v: f32| if v.is_finite() && v >= 1.0 { v } else { 1.0 };
    let (w, h) = (positive(page.0), positive(page.1));
    let side = max_side.max(1) as f32;
    let width = if wanted_width.is_finite() { wanted_width.clamp(64.0, 5000.0) } else { 64.0 };
    let width = width.min(side).min(side * w / h).min((MAX_TEXTURE_PIXELS * w / h).sqrt()).floor().max(1.0);
    let height = (width * h / w).round().clamp(1.0, side);
    [width as u32, height as u32]
}

/// A texture of `image`, unless it is larger than the GPU takes (`max_side`): uploading that would
/// end the program. Rendering keeps within it, so this is a last guard.
fn page_texture(ctx: &egui::Context, name: String, image: &image::RgbaImage, max_side: usize) -> Option<egui::TextureHandle> {
    let size = [image.width() as usize, image.height() as usize];
    if size[0] == 0 || size[1] == 0 || size[0] > max_side || size[1] > max_side {
        return None;
    }
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    Some(ctx.load_texture(name, color, egui::TextureOptions::LINEAR))
}

fn pdf_rect_to_screen(page_rect: egui::Rect, zoom: f32, page_height: f32, r: Rect) -> egui::Rect {
    let min = egui::pos2(page_rect.min.x + r.x * zoom, page_rect.min.y + (page_height - r.max_y()) * zoom);
    egui::Rect::from_min_size(min, egui::vec2(r.width * zoom, r.height * zoom))
}

impl eframe::App for CarbideApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Files dropped onto the window open like Open.
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped {
            self.open_path(&path);
        }
        self.handle_shortcuts(ui.ctx());
        self.handle_close_request(ui.ctx());
        // Quitting asked for from outside the window: the Dock, logging out, AppleScript, Quit.
        if quit::take_request() {
            self.on_system_quit(ui.ctx());
        }

        self.autosave_tick();
        self.poll_ocr();
        self.poll_sign();
        let waiting = self.ocr_job.is_some() || self.sign_job.is_some();
        ui.ctx().request_repaint_after(if waiting { Duration::from_millis(250) } else { Duration::from_secs(5) });

        self.draw_shell(ui);

        dialogs::show(self, ui.ctx());
        self.prompts(ui.ctx());
        quit::set_unsaved(self.has_unsaved_changes());
    }

    /// The app ends without its window closing first when quitting went ahead with nothing
    /// unsaved (see `quit`).
    fn on_exit(&mut self) {
        self.release_on_exit();
    }
}

/// `carbide-gui --copy FILE` opens FILE, a private temporary copy, as a new document and removes it.
const COPY_FLAG: &str = "--copy";

/// What a window opens when it starts.
enum Launch {
    /// A file on disk, which Save writes back to.
    File(PathBuf),
    /// A private temporary copy, opened as a new document (see `CarbideApp::open_copy`).
    Copy(PathBuf),
}

/// `carbide-gui --icons` shows every icon with its name, for checking the drawings.
struct IconSheet;

impl eframe::App for IconSheet {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::RIBBON)).show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            ui.horizontal_wrapped(|ui| {
                for glyph in icons::Glyph::ALL {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(104.0, 92.0), egui::Sense::hover());
                    let box_ = egui::Rect::from_center_size(rect.center_top() + egui::vec2(0.0, 34.0), egui::vec2(52.0, 52.0));
                    icons::paint(ui.painter(), box_, glyph, theme::TEXT);
                    ui.painter().text(
                        rect.center_bottom() - egui::vec2(0.0, 10.0),
                        egui::Align2::CENTER_CENTER,
                        format!("{glyph:?}"),
                        egui::FontId::proportional(11.0),
                        theme::TEXT_DIM,
                    );
                }
            });
        });
    }
}

/// Makes every way of quitting ask about unsaved documents first (see `quit`). Where that cannot
/// be set up, the application menu's Quit (and Cmd+Q) at least goes through the window's close
/// button, which asks.
fn guard_quitting(ctx: &egui::Context) {
    if !quit::install(ctx) {
        route_quit_through_window_close();
    }
}

/// Points the application menu's Quit item (and Cmd+Q) at `performClose:`, so that it closes the
/// key window, which goes through the same prompt as the window's close button. It then does
/// nothing while the only window is minimized, which is why this is only the fallback.
#[cfg(target_os = "macos")]
fn route_quit_through_window_close() {
    use objc2::{sel, MainThreadMarker};
    use objc2_app_kit::NSApplication;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let Some(main_menu) = app.mainMenu() else { return };
    for index in 0..main_menu.numberOfItems() {
        let Some(submenu) = main_menu.itemAtIndex(index).and_then(|item| item.submenu()) else { continue };
        for item in submenu.itemArray() {
            if item.action() == Some(sel!(terminate:)) {
                // SAFETY: `performClose:` is a standard NSWindow action, reached through the responder chain
                // when the target is nil.
                unsafe {
                    item.setTarget(None);
                    item.setAction(Some(sel!(performClose:)));
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn route_quit_through_window_close() {}

fn main() -> eframe::Result {
    if std::env::args_os().any(|a| a == "--icons") {
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]).with_title("Carbide PDF icons"),
            ..Default::default()
        };
        return eframe::run_native(
            "Carbide PDF icons",
            options,
            Box::new(|cc| {
                theme::apply(&cc.egui_ctx);
                Ok(Box::new(IconSheet))
            }),
        );
    }
    let mut args = std::env::args_os().skip(1);
    let launch = match args.next() {
        Some(flag) if flag == COPY_FLAG => args.next().map(|path| Launch::Copy(PathBuf::from(path))),
        Some(path) => Some(Launch::File(PathBuf::from(path))),
        None => None,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 560.0])
            .with_title("Carbide PDF"),
        ..Default::default()
    };
    eframe::run_native("Carbide PDF", options, Box::new(move |cc| Ok(Box::new(CarbideApp::new(cc, launch)))))
}
