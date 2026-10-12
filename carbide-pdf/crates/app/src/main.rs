//! Carbide PDF: desktop application (egui).

mod dialogs;
mod icons;
mod paths;
mod ribbon;
mod shell;
mod theme;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use carbide_engine::autosave::{now_unix, AutoSaveSettings, AutoSaveStore, RecordId};
use carbide_engine::{
    open_revision_bytes, pdf_date_now, revision_display_name, Error as EngineError, MarkupStyle, Pdf, PageGeometry,
    Rect, Redaction, Renderer, Revision,
};
use dialogs::Dialog;
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
    pub pdf: Pdf,
    /// Serialized current state, used for rendering and text extraction.
    pub bytes: Vec<u8>,
    pub path: Option<PathBuf>,
    /// The file as last written to disk: the base for incremental revisions.
    pub saved_bytes: Option<Vec<u8>>,
    pub version: u64,
    pub password: Option<String>,
    pub record: Option<RecordId>,
    pub dirty: bool,
    pub geometries: Vec<PageGeometry>,
    undo: Vec<Vec<u8>>,
    redo: Vec<Vec<u8>>,
    // View state kept while the document sits in a background tab.
    view_page: usize,
    view_redactions: Vec<Redaction>,
    view_notice: Option<String>,
}

impl OpenDoc {
    fn new(mut pdf: Pdf, path: Option<PathBuf>, saved: Option<Vec<u8>>, password: Option<String>) -> Result<Self, EngineError> {
        let bytes = pdf.to_bytes()?;
        let geometries = (0..pdf.page_count()).filter_map(|i| pdf.page_geometry(i).ok()).collect();
        Ok(Self {
            pdf,
            bytes,
            path,
            saved_bytes: saved,
            version: 0,
            password,
            record: None,
            dirty: false,
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
        self.geometries = (0..self.pdf.page_count()).filter_map(|i| self.pdf.page_geometry(i).ok()).collect();
        self.version += 1;
        Ok(())
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }
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
    pub dialog: Option<Dialog>,
    textures: HashMap<(u64, usize, u32), egui::TextureHandle>,
    thumbs: HashMap<(u64, usize), egui::TextureHandle>,
    drag: Option<DragState>,
    pub pending_redactions: Vec<Redaction>,
    pub current_page: usize,
    scroll_to: Option<usize>,
    pub autosave: Option<AutoSaveStore>,
    pub settings: AutoSaveSettings,
    last_autosave: Instant,
    /// Result channel of a running OCR job.
    ocr_job: Option<std::sync::mpsc::Receiver<Result<(Pdf, carbide_engine::ocr::OcrReport), String>>>,
    /// The document notice bar text (signature summary).
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
    fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);
        route_quit_through_window_close();
        let (renderer, renderer_error) = match Renderer::global() {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let autosave = AutoSaveStore::open(paths::autosave_dir()).ok();
        let mut app = Self {
            renderer,
            renderer_error,
            doc: None,
            tool: Tool::Select,
            zoom: 1.25,
            status: "Open a PDF, or drop one on the window.".into(),
            dialog: None,
            textures: HashMap::new(),
            thumbs: HashMap::new(),
            drag: None,
            pending_redactions: Vec::new(),
            current_page: 0,
            scroll_to: None,
            autosave,
            settings: paths::load_autosave_settings(),
            last_autosave: Instant::now(),
            ocr_job: None,
            notice: None,
            tabs: Vec::new(),
            active_tab: None,
            recent: paths::load_recent(),
            shell: shell::ShellState::new(),
        };
        if let Some(path) = initial {
            app.open_path(&path);
        }
        // Launch: show recovered documents waiting from an earlier session.
        if app.autosave.as_ref().is_some_and(|s| s.any_needs_save()) {
            app.dialog = Some(Dialog::AutoRecovery);
        }
        app
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
            Ok(pdf) => self.install(pdf, Some(path.to_path_buf()), Some(bytes), None),
            Err(EngineError::PasswordRequired) => {
                self.dialog = Some(Dialog::Password { path: path.to_path_buf(), bytes, input: String::new(), failed: false });
            }
            Err(e) => self.alert(format!("Could not open {}: {e}", path.display())),
        }
    }

    pub fn open_with_password(&mut self, path: PathBuf, bytes: Vec<u8>, password: String) -> bool {
        match Pdf::from_bytes_with_password(&bytes, &password) {
            Ok(pdf) => {
                self.install(pdf, Some(path), None, Some(password));
                true
            }
            Err(_) => false,
        }
    }

    /// Opens `pdf` in a new tab and makes it the active document.
    fn install(&mut self, pdf: Pdf, path: Option<PathBuf>, saved: Option<Vec<u8>>, password: Option<String>) {
        match OpenDoc::new(pdf, path.clone(), saved, password) {
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
                self.last_autosave = Instant::now();
                if let Some(path) = path {
                    self.push_recent(&path);
                }
            }
            Err(e) => self.alert(format!("Could not read the document: {e}")),
        }
    }

    fn reset_view_caches(&mut self) {
        self.textures.clear();
        self.thumbs.clear();
        self.drag = None;
        self.shell.forget_caches();
    }

    /// The document in tab `index`, whether it is the active one or parked.
    fn doc_at(&self, index: usize) -> Option<&OpenDoc> {
        if self.active_tab == Some(index) {
            self.doc.as_ref()
        } else {
            self.tabs.get(index).and_then(|slot| slot.as_ref())
        }
    }

    /// Moves the active document back into its tab slot, together with its view state.
    fn park_active(&mut self) {
        let Some(index) = self.active_tab else { return };
        let Some(mut doc) = self.doc.take() else { return };
        doc.view_page = self.current_page;
        doc.view_redactions = std::mem::take(&mut self.pending_redactions);
        doc.view_notice = self.notice.take();
        self.tabs[index] = Some(doc);
        self.active_tab = None;
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
                self.install(pdf, None, None, None);
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
        if documents.is_empty() {
            return;
        }
        let refs: Vec<&Pdf> = documents.iter().collect();
        match Pdf::merge(&refs) {
            Ok(pdf) => {
                self.install(pdf, None, None, None);
                if let Some(doc) = self.doc.as_mut() {
                    doc.dirty = true;
                }
            }
            Err(e) => self.alert(format!("Could not combine the files: {e}")),
        }
    }

    pub fn alert(&mut self, message: String) {
        self.status = message.lines().next().unwrap_or_default().to_string();
        self.dialog = Some(Dialog::Message(message));
    }

    /// Runs an edit on the open document with undo support.
    pub fn edit(&mut self, label: &str, f: impl FnOnce(&mut Pdf) -> Result<(), EngineError>) {
        let Some(doc) = self.doc.as_mut() else { return };
        let snapshot = doc.bytes.clone();
        match f(&mut doc.pdf).and_then(|_| doc.refresh()) {
            Ok(()) => {
                doc.undo.push(snapshot);
                if doc.undo.len() > MAX_UNDO {
                    doc.undo.remove(0);
                }
                doc.redo.clear();
                doc.dirty = true;
                self.status = format!("{label} done.");
                self.textures.clear();
                self.thumbs.clear();
            }
            Err(e) => {
                // Put the document back exactly as it was.
                if let Ok(pdf) = Pdf::from_bytes(&snapshot) {
                    doc.pdf = pdf;
                    let _ = doc.refresh();
                }
                let message = format!("{label} failed: {e}");
                self.alert(message);
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
        let current = doc.bytes.clone();
        match Pdf::from_bytes(&snapshot) {
            Ok(pdf) => {
                doc.pdf = pdf;
                let _ = doc.refresh();
                if undo {
                    doc.redo.push(current);
                } else {
                    doc.undo.push(current);
                }
                doc.dirty = true;
                self.status = if undo { "Undid last change." } else { "Redid last change." }.into();
                self.textures.clear();
                self.thumbs.clear();
                self.pending_redactions.clear();
            }
            Err(e) => self.alert(format!("Could not {}: {e}", if undo { "undo" } else { "redo" })),
        }
    }

    /// Writes the document to `path` and updates the revision base.
    ///
    /// An existing file is updated in place by appending, so signatures and saved revisions keep
    /// their byte offsets. A password-protected file is encrypted again with the password that
    /// opened it; the decrypted in-memory copy is never written over the original.
    pub fn save_to(&mut self, path: &Path) {
        let Some(doc) = self.doc.as_mut() else { return };
        let password = doc.password.clone();
        let base = password.is_none().then(|| doc.saved_bytes.clone()).flatten();
        let result = doc.pdf.bytes_for_save(base.as_deref(), password.as_deref()).and_then(|bytes| {
            let unchanged = doc.path.as_deref() == Some(path) && doc.saved_bytes.as_deref() == Some(bytes.as_slice());
            if !unchanged {
                write_replacing(path, &bytes)?;
            }
            Ok(bytes)
        });
        let mut reload_error = None;
        match result {
            Ok(bytes) => {
                let mut rerender = false;
                if password.is_none() && doc.saved_bytes.as_ref() != Some(&bytes) {
                    match Pdf::from_bytes(&bytes) {
                        Ok(reloaded) => {
                            doc.geometries = (0..reloaded.page_count()).filter_map(|i| reloaded.page_geometry(i).ok()).collect();
                            doc.pdf = reloaded;
                            doc.bytes = bytes.clone();
                            doc.version = doc.version.wrapping_add(1);
                            rerender = true;
                        }
                        Err(e) => reload_error = Some(e.to_string()),
                    }
                }
                doc.path = Some(path.to_path_buf());
                doc.saved_bytes = Some(bytes);
                doc.dirty = false;
                if let (Some(store), Some(id)) = (self.autosave.as_mut(), doc.record) {
                    let _ = store.document_saved(id);
                    doc.record = Some(store.begin_document(Some(path), now_unix()));
                }
                if rerender {
                    self.textures.clear();
                    self.thumbs.clear();
                }
                self.status = format!("Saved {}", path.display());
            }
            Err(e) => self.alert(format!("Could not save {}: {e}", path.display())),
        }
        if let Some(error) = reload_error {
            self.alert(format!("Saved {}, but could not reload it: {error}", path.display()));
        }
    }

    fn save(&mut self) {
        match self.doc.as_ref().and_then(|d| d.path.clone()) {
            Some(path) => self.save_to(&path),
            None => self.save_as(),
        }
    }

    fn save_as(&mut self) {
        let name = self.doc.as_ref().map(|d| d.title()).unwrap_or_else(|| "Untitled.pdf".into());
        if let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name(name).save_file() {
            self.save_to(&path);
        }
    }

    // ---- revisions and recovery --------------------------------------------------------

    pub fn save_revision(&mut self, label: &str) -> Result<(), String> {
        let doc = self.doc.as_mut().ok_or("No document is open.")?;
        let path = doc.path.clone().ok_or("Save the document first, then save a revision.")?;
        // The base is the file as last written; pending edits are folded into the update.
        let base = match &doc.saved_bytes {
            Some(b) => b.clone(),
            None => std::fs::read(&path).map_err(|e| e.to_string())?,
        };
        let updated = doc.pdf.save_revision(&base, label, &pdf_date_now()).map_err(|e| e.to_string())?;
        write_replacing(&path, &updated).map_err(|e| e.to_string())?;
        let pdf = Pdf::from_bytes(&updated).map_err(|e| e.to_string())?;
        doc.pdf = pdf;
        doc.saved_bytes = Some(updated);
        doc.dirty = false;
        doc.refresh().map_err(|e| e.to_string())?;
        self.textures.clear();
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
                let name = revision_display_name(&doc.title(), revision).replace(['/', ':'], "-");
                let target = std::env::temp_dir().join(format!("{name}.pdf"));
                if std::fs::write(&target, prefix).is_ok() {
                    self.spawn_window(&target);
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

    pub fn renderer(&self) -> Option<&'static Renderer> {
        self.renderer
    }

    // ---- auto save ---------------------------------------------------------------------

    fn autosave_tick(&mut self) {
        if !self.settings.enabled || self.last_autosave.elapsed() < self.settings.interval() {
            return;
        }
        self.last_autosave = Instant::now();
        let Some(store) = self.autosave.as_mut() else { return };
        let mut saved = 0;
        let mut failure = None;
        for doc in self.doc.iter_mut().chain(self.tabs.iter_mut().flatten()) {
            let Some(id) = doc.record else { continue };
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
        let mut goto: Option<usize> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for page in 0..count {
                let (w, h) = geometries.get(page).map(|g| g.display_size()).unwrap_or((612.0, 792.0));
                let width = 110.0f32;
                let size = egui::vec2(width, width * h / w.max(1.0));
                let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
                if ui.is_rect_visible(rect) {
                    let key = (version, page);
                    if !self.thumbs.contains_key(&key) {
                        let dpi = 72.0 * width / w.max(1.0) * ui.ctx().pixels_per_point();
                        if let Ok(image) = renderer.render_page(bytes, page, dpi) {
                            let color = egui::ColorImage::from_rgba_unmultiplied(
                                [image.width() as usize, image.height() as usize],
                                image.as_raw(),
                            );
                            let texture = ui.ctx().load_texture(format!("thumb-{version}-{page}"), color, egui::TextureOptions::LINEAR);
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
            self.zoom = (self.zoom * pinch).clamp(0.25, 6.0);
            self.shell.fit = shell::Fit::Manual;
        }
        if self.shell.fit != shell::Fit::Manual {
            if let Some(g) = self.doc.as_ref().and_then(|d| d.geometries.get(self.current_page)) {
                let (w, h) = g.display_size();
                let avail = ui.available_size();
                let by_width = (avail.x - 48.0) / w.max(1.0);
                let by_height = (avail.y - 24.0) / h.max(1.0);
                let fitted = if self.shell.fit == shell::Fit::Width { by_width } else { by_width.min(by_height) };
                self.zoom = fitted.clamp(0.25, 6.0);
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
            let pixels_per_point = ui.ctx().pixels_per_point();
            let viewport_top = ui.clip_rect().min.y;

            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    for page in 0..count {
                        let (w, h) = geometries.get(page).map(|g| g.display_size()).unwrap_or((612.0, 792.0));
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

                        if ui.is_rect_visible(rect) {
                            let px_width = (size.x * pixels_per_point).round().clamp(64.0, 5000.0) as u32;
                            let key = (version, page, px_width);
                            if !textures.contains_key(&key) {
                                if textures.len() > 24 {
                                    textures.clear();
                                }
                                let dpi = 72.0 * px_width as f32 / w.max(1.0);
                                if let Ok(image) = renderer.render_page(bytes, page, dpi) {
                                    let color = egui::ColorImage::from_rgba_unmultiplied(
                                        [image.width() as usize, image.height() as usize],
                                        image.as_raw(),
                                    );
                                    let texture = ui.ctx().load_texture(
                                        format!("page-{version}-{page}-{px_width}"),
                                        color,
                                        egui::TextureOptions::LINEAR,
                                    );
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

        for action in actions {
            match action {
                Action::SignArea { page, rect } => {
                    self.dialog = Some(Dialog::sign(Some((page, rect)), false));
                    self.tool = Tool::Select;
                }
                Action::Visible(page) => self.current_page = page,
                Action::MarkRedaction { page, rect } => {
                    self.pending_redactions.push(Redaction { page, rect });
                    self.status = "Redaction marked. Security > Apply removes the marked content.".into();
                }
                Action::NotePrompt { page, x, y, on_page } => {
                    self.dialog = Some(Dialog::Note { page, x, y, on_page, text: String::new() });
                }
                Action::Markup { page, rect, style, snap } => {
                    let (bytes, geometry) = match self.doc.as_ref() {
                        Some(d) => (d.bytes.clone(), d.geometries.get(page).copied()),
                        None => continue,
                    };
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
        let mut copy = doc.pdf.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = copy
                .make_searchable(renderer, &carbide_engine::ocr::OcrOptions::default())
                .map(|report| (copy, report))
                .map_err(|e| e.to_string());
            let _ = sender.send(result);
        });
        self.ocr_job = Some(receiver);
        self.status = "Recognizing text...".into();
    }

    fn poll_ocr(&mut self) {
        let Some(receiver) = &self.ocr_job else { return };
        match receiver.try_recv() {
            Ok(Ok((pdf, report))) => {
                self.ocr_job = None;
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
            Ok(Err(message)) => {
                self.ocr_job = None;
                self.alert(format!("OCR failed: {message}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.ocr_job = None;
                self.alert("OCR stopped unexpectedly.".into());
            }
        }
    }

    // ---- signatures --------------------------------------------------------------------

    /// Signs the document file in place. The document is saved first when it has unsaved edits.
    pub fn sign_document(&mut self, request: dialogs::SignRequest) {
        let needs_save = self.doc.as_ref().is_some_and(|d| d.dirty || d.path.is_none() || d.saved_bytes.is_none());
        if needs_save {
            self.save();
        }
        let Some(doc) = self.doc.as_mut() else { return };
        let (Some(path), Some(base)) = (doc.path.clone(), doc.saved_bytes.clone()) else {
            return self.alert("The document must be saved before it can be signed.".into());
        };
        let identity = match request.load_identity() {
            Ok(i) => i,
            Err(message) => return self.alert(message),
        };
        let authority = request.timestamp_source();
        let options = request.options(identity.as_ref(), authority.as_ref());
        let result = doc.pdf.sign(&base, &options).map_err(|e| e.to_string()).and_then(|bytes| {
            write_replacing(&path, &bytes).map_err(|e| e.to_string())?;
            let pdf = Pdf::from_bytes(&bytes).map_err(|e| e.to_string())?;
            Ok((pdf, bytes))
        });
        match result {
            Ok((pdf, bytes)) => {
                doc.pdf = pdf;
                doc.saved_bytes = Some(bytes);
                doc.dirty = false;
                let _ = doc.refresh();
                self.textures.clear();
                self.thumbs.clear();
                self.notice = signature_notice(self.doc.as_ref().expect("open"));
                self.status = match &identity {
                    Some(id) => format!("Signed {} as {}", path.display(), id.common_name()),
                    None => format!("Time stamped {}", path.display()),
                };
            }
            Err(message) => self.alert(message),
        }
    }

    pub fn validate_signatures(&mut self) {
        let Some(doc) = self.doc.as_ref() else { return };
        let bytes = doc.saved_bytes.clone().unwrap_or_else(|| doc.bytes.clone());
        match Pdf::verify_signatures(&bytes, &paths::load_trust_store(), chrono::Utc::now()) {
            Ok(reports) if reports.is_empty() => self.alert("This document has no signatures.".into()),
            Ok(reports) => {
                let notice = carbide_engine::signature::notice_text(&reports).unwrap_or_default().to_string();
                self.notice = Some(notice.clone());
                self.dialog = Some(Dialog::Signatures { reports, notice });
            }
            Err(e) => self.alert(format!("Could not read the signatures: {e}")),
        }
    }

    // ---- file helpers ------------------------------------------------------------------

    fn export_images(&mut self) {
        let (Some(renderer), Some(doc)) = (self.renderer, self.doc.as_mut()) else { return };
        let Some(folder) = rfd::FileDialog::new().set_title("Export Here").pick_folder() else { return };
        let base = doc.path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "page".into());
        match carbide_engine::export_images(renderer, &mut doc.pdf, &folder, &base, carbide_engine::ImageFormat::Png, 150.0) {
            Ok(paths) => self.status = format!("Exported {} image(s).", paths.len()),
            Err(e) => self.alert(format!("Export failed: {e}")),
        }
    }

    fn export_text(&mut self) {
        let (Some(renderer), Some(doc)) = (self.renderer, self.doc.as_mut()) else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("Text", &["txt"]).set_file_name("document.txt").save_file() else { return };
        match carbide_engine::export_text(renderer, &mut doc.pdf).and_then(|t| std::fs::write(&path, t).map_err(Into::into)) {
            Ok(()) => self.status = format!("Exported text to {}", path.display()),
            Err(e) => self.alert(format!("Export failed: {e}")),
        }
    }

    fn merge_files(&mut self) {
        let Some(files) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).pick_files() else { return };
        let mut others = Vec::new();
        for file in files {
            match Pdf::open(&file) {
                Ok(p) => others.push(p),
                Err(e) => return self.alert(format!("{}: {e}", file.display())),
            }
        }
        self.edit("Merge", move |pdf| {
            let mut all = vec![pdf.clone()];
            all.extend(others);
            let refs: Vec<&Pdf> = all.iter().collect();
            *pdf = Pdf::merge(&refs)?;
            Ok(())
        });
    }

    fn extract_current(&mut self) {
        let page = self.current_page;
        let Some(doc) = self.doc.as_ref() else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("PDF Files", &["pdf"]).set_file_name(format!("Page {}.pdf", page + 1)).save_file() else { return };
        match doc.pdf.extract_pages(&[page]).and_then(|mut p| p.save(&path)) {
            Ok(()) => self.status = format!("Extracted page {} to {}", page + 1, path.display()),
            Err(e) => self.alert(format!("Could not extract: {e}")),
        }
    }

    fn apply_redactions(&mut self) {
        let (Some(renderer), redactions) = (self.renderer, std::mem::take(&mut self.pending_redactions)) else { return };
        if redactions.is_empty() {
            return;
        }
        self.edit("Apply redactions", move |pdf| pdf.apply_redactions(renderer, &redactions, 200.0));
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
                self.dialog = Some(Dialog::Compare { title, diffs });
            }
            Err(e) => self.alert(format!("Compare failed: {e}")),
        }
    }
}

/// Replaces `path` only after the new bytes are fully written, so a failed save leaves the
/// previous file in place.
fn write_replacing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("pdf.carbide-tmp");
    std::fs::write(&tmp, bytes)?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Notice bar text for a document that carries signatures.
fn signature_notice(doc: &OpenDoc) -> Option<String> {
    if !doc.pdf.has_signatures() {
        return None;
    }
    let bytes = doc.saved_bytes.as_ref().unwrap_or(&doc.bytes);
    let trust = paths::load_trust_store();
    let reports = Pdf::verify_signatures(bytes, &trust, chrono::Utc::now()).ok()?;
    carbide_engine::signature::notice_text(&reports).map(str::to_string)
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

        self.autosave_tick();
        self.poll_ocr();
        ui.ctx().request_repaint_after(if self.ocr_job.is_some() { Duration::from_millis(250) } else { Duration::from_secs(5) });

        self.draw_shell(ui);

        dialogs::show(self, ui.ctx());
        self.prompts(ui.ctx());
    }
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

/// winit's application menu ends the process from "Quit" (and Cmd+Q) without any close request, so
/// unsaved documents would be lost silently. Pointing the item at `performClose:` makes it close the
/// key window instead, which goes through the same prompt as the window's close button.
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
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 560.0])
            .with_title("Carbide PDF"),
        ..Default::default()
    };
    eframe::run_native("Carbide PDF", options, Box::new(move |cc| Ok(Box::new(CarbideApp::new(cc, initial)))))
}
