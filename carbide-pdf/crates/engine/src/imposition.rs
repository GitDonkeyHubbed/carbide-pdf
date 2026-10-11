//! Imposition: turns a document into printable sheets.
//!
//! Page-to-cell assignment comes from [`crate::imposition_plan`] and is covered by worked examples
//! in the tests. Cell geometry (origin, fit, creep, marks) is a documented design: pages are centred in equal cells,
//! "fit" scales uniformly to the cell, and creep moves pages toward the spine.

use lopdf::{dictionary, Dictionary, Object, ObjectId, Stream};

use crate::error::{Error, Result};
use crate::fonts::{literal_string, win_ansi, StandardFont};
use crate::geometry::fmt;
use crate::imposition_plan::{output_order, plan_plates, LayoutKind, Plate, PlanOptions, Style};
use crate::pdf::Pdf;

pub mod messages {
    pub const NO_MEDIA: &str = "Choose a media size (or Autosize) before imposing; none is selected.";
    pub const MARGINS: &str = "The margins leave no room on this page size.";
    pub const INVALID_SETTINGS: &str = "The settings file is not valid and could not be loaded.";
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    Point,
    Millimeter,
    Centimeter,
    Inch,
}

impl Unit {
    pub fn to_points(self, value: f32) -> f32 {
        match self {
            Unit::Point => value,
            Unit::Millimeter => value * 72.0 / 25.4,
            Unit::Centimeter => value * 72.0 / 2.54,
            Unit::Inch => value * 72.0,
        }
    }
}

/// Built-in media (portrait points unless the size is naturally landscape).
pub const PAPER_SIZES: &[(&str, f32, f32)] = &[
    ("A0", 2384.0, 3370.0),
    ("A1", 1684.0, 2384.0),
    ("A2", 1191.0, 1684.0),
    ("A3", 842.0, 1191.0),
    ("A4", 595.0, 842.0),
    ("B4", 709.0, 1001.0),
    ("B5", 499.0, 709.0),
    ("Executive", 522.0, 756.0),
    ("Ledger", 1224.0, 792.0),
    ("Legal", 612.0, 1008.0),
    ("Letter", 612.0, 792.0),
    ("Tabloid", 792.0, 1224.0),
];

#[derive(Debug, Clone, PartialEq)]
pub enum Media {
    /// Nothing chosen: refused with [`messages::NO_MEDIA`].
    None,
    Named(String),
    Custom { width: f32, height: f32 },
    /// Sheet size follows the page size, scale, margins and gutters.
    Autosize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStyle {
    Cross,
    Circle,
}

#[derive(Debug, Clone)]
pub struct ImpositionOptions {
    pub plan: PlanOptions,
    pub media: Media,
    pub auto_rotate: bool,
    pub fit_to_media: bool,
    pub scale_x_percent: f32,
    pub scale_y_percent: f32,
    pub margins: Margins,
    /// Gap between the two pages of a spread.
    pub gutter_horizontal: f32,
    pub gutter_vertical: f32,
    /// Gap between spreads in 4- and 8-column grids.
    pub spread: f32,
    pub creep: f32,
    pub crop_marks: bool,
    pub inner_marks: bool,
    pub registration_marks: bool,
    pub registration_style: RegistrationStyle,
    pub page_info: bool,
    /// Text for the "Description" line; defaults to sheet number and side.
    pub description: Option<String>,
    pub scale_large_pages: bool,
    pub flip_backs: bool,
}

impl Default for ImpositionOptions {
    fn default() -> Self {
        Self {
            plan: PlanOptions::default(),
            media: Media::Named("Letter".into()),
            auto_rotate: true,
            fit_to_media: true,
            scale_x_percent: 100.0,
            scale_y_percent: 100.0,
            margins: Margins { left: 0.0, right: 0.0, top: 0.0, bottom: 0.0 },
            gutter_horizontal: 0.0,
            gutter_vertical: 0.0,
            spread: 0.0,
            creep: 0.0,
            crop_marks: false,
            inner_marks: false,
            registration_marks: false,
            registration_style: RegistrationStyle::Cross,
            page_info: false,
            description: None,
            scale_large_pages: false,
            flip_backs: false,
        }
    }
}

/// Horizontal gaps between the `cols` columns of a plate.
fn column_gaps(options: &ImpositionOptions, cols: usize) -> Vec<f32> {
    let matrix = matches!(
        options.plan.layout,
        LayoutKind::Booklet2Up | LayoutKind::Booklet4Up | LayoutKind::Booklet8Up
    );
    (1..cols)
        .map(|boundary| {
            if matrix && boundary % 2 == 0 {
                options.spread
            } else {
                options.gutter_horizontal
            }
        })
        .collect()
}

struct Grid {
    width: f32,
    height: f32,
    cell_w: f32,
    cell_h: f32,
    gaps: Vec<f32>,
}

impl ImpositionOptions {
    fn grid_size(&self, cols: usize, rows: usize) -> (usize, usize) {
        (cols.max(1), rows.max(1))
    }

    /// Sheet size and cell size for a source page of `page` points.
    fn layout_grid(&self, cols: usize, rows: usize, page: (f32, f32)) -> Result<Grid> {
        let (cols, rows) = self.grid_size(cols, rows);
        let gaps = column_gaps(self, cols);
        let gap_total_h: f32 = gaps.iter().sum();
        let gap_total_v = self.gutter_vertical * (rows as f32 - 1.0);
        let margin_h = self.margins.left + self.margins.right;
        let margin_v = self.margins.top + self.margins.bottom;

        let (width, height) = match &self.media {
            Media::None => return Err(Error::Other(messages::NO_MEDIA.into())),
            Media::Custom { width, height } => (*width, *height),
            Media::Named(name) => {
                let (_, w, h) = PAPER_SIZES
                    .iter()
                    .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
                    .ok_or_else(|| Error::Other(format!("unknown media size {name:?}")))?;
                (*w, *h)
            }
            Media::Autosize => {
                let cell_w = page.0 * self.scale_x_percent / 100.0;
                let cell_h = page.1 * self.scale_y_percent / 100.0;
                (
                    cell_w * cols as f32 + gap_total_h + margin_h,
                    cell_h * rows as f32 + gap_total_v + margin_v,
                )
            }
        };

        let build = |width: f32, height: f32| -> Result<Grid> {
            let usable_w = width - margin_h - gap_total_h;
            let usable_h = height - margin_v - gap_total_v;
            if usable_w <= 0.0 || usable_h <= 0.0 {
                return Err(Error::Other(messages::MARGINS.into()));
            }
            Ok(Grid {
                width,
                height,
                cell_w: usable_w / cols as f32,
                cell_h: usable_h / rows as f32,
                gaps: gaps.clone(),
            })
        };

        let upright = build(width, height)?;
        if self.auto_rotate && !matches!(self.media, Media::Autosize) {
            // Pick the orientation in which the page is drawn larger.
            if let Ok(turned) = build(height, width) {
                let scale = |g: &Grid| (g.cell_w / page.0).min(g.cell_h / page.1);
                if scale(&turned) > scale(&upright) {
                    return Ok(turned);
                }
            }
        }
        Ok(upright)
    }

    /// Checks settings that do not depend on the document. Returns the user-facing message on failure.
    pub fn validate(&self) -> Result<()> {
        if matches!(self.media, Media::None) {
            return Err(Error::Other(messages::NO_MEDIA.into()));
        }
        let (cols, rows) = match self.plan.layout {
            LayoutKind::Booklet2Up => (2, 1),
            LayoutKind::Booklet4Up => (2, 2),
            LayoutKind::Booklet8Up => (4, 2),
            LayoutKind::Sequential => (1, 1),
            _ => (self.plan.columns, self.plan.rows),
        };
        if matches!(self.plan.layout, LayoutKind::Booklet2Up | LayoutKind::Booklet4Up | LayoutKind::Booklet8Up)
            && cols % 2 != 0
        {
            return Err(Error::Other("booklet layouts need an even number of columns".into()));
        }
        self.layout_grid(cols, rows, (1.0, 1.0)).map(|_| ())
    }
}

// ---- rendering -----------------------------------------------------------------------------

fn invert_matrix(m: [f32; 6]) -> [f32; 6] {
    let det = m[0] * m[3] - m[1] * m[2];
    [
        m[3] / det,
        -m[1] / det,
        -m[2] / det,
        m[0] / det,
        (m[2] * m[5] - m[3] * m[4]) / det,
        (m[1] * m[4] - m[0] * m[5]) / det,
    ]
}

fn real_array(values: &[f32]) -> Object {
    Object::Array(values.iter().map(|v| Object::Real(*v)).collect())
}

impl Pdf {
    /// Wraps source page `index` in a form XObject whose coordinates are the displayed page
    /// (origin bottom-left, rotation and crop box applied).
    fn page_as_form(&mut self, index: usize) -> Result<ObjectId> {
        let page_id = self.page_id(index)?;
        let geometry = self.page_geometry(index)?;
        let content = self.doc.get_page_content(page_id);
        let resources = self.inherited(page_id, b"Resources").unwrap_or(Object::Dictionary(Dictionary::new()));
        let (w, h) = (geometry.box_width, geometry.box_height);
        let mut body = b"q\n".to_vec();
        body.extend_from_slice(&content);
        body.extend_from_slice(b"\nQ\n");
        let form = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => real_array(&[geometry.x0, geometry.y0, geometry.x0 + w, geometry.y0 + h]),
                "Matrix" => real_array(&invert_matrix(geometry.display_to_user())),
                "Resources" => resources,
            },
            body,
        );
        Ok(self.doc.add_object(form))
    }

    /// Builds a new document of imposed sheets. `progress` receives 0..=100 and returns `true` to
    /// stop early (which yields an error).
    pub fn impose(
        &self,
        options: &ImpositionOptions,
        mut progress: Option<&mut dyn FnMut(u32) -> bool>,
    ) -> Result<Pdf> {
        options.validate()?;
        let total = self.page_count();
        let plates = plan_plates(&options.plan, total)?;
        let plan = &options.plan;
        let fold_active = plan.binding == crate::imposition_plan::Binding::Perfect;
        let order = output_order(plates.len(), plan.printing_order, plan.double_sided, plan.style, fold_active);

        let first = self.page_geometry(0)?;
        let first_size = first.display_size();

        let mut out = self.clone();
        out.flatten_page_tree()?;
        let root_id = out.pages_root()?;
        let mut forms: Vec<Option<ObjectId>> = vec![None; total];
        let mut new_pages: Vec<Object> = Vec::with_capacity(order.len());
        let cells_total: usize = order.iter().map(|&p| plates[p].cells.len()).sum();
        let mut cells_done = 0usize;

        for (output_index, &plate_index) in order.iter().enumerate() {
            let plate = &plates[plate_index];
            let grid = options.layout_grid(plate.cols, plate.rows, first_size)?;
            let mut xobjects = Dictionary::new();
            let mut content = String::new();
            let mut fonts = Dictionary::new();

            let column_x = column_origins(options, plate, &grid);
            for row in 0..plate.rows {
                let cell_y = options.margins.bottom
                    + (plate.rows - 1 - row) as f32 * (grid.cell_h + options.gutter_vertical);
                for col in 0..plate.cols {
                    let cell = plate.cell(row, col);
                    cells_done += 1;
                    if let Some(callback) = progress.as_deref_mut() {
                        if callback((cells_done * 100 / cells_total.max(1)) as u32) {
                            return Err(Error::Other("imposition was cancelled".into()));
                        }
                    }
                    let Some(page) = cell.page else { continue };
                    let form_id = match forms[page] {
                        Some(id) => id,
                        None => {
                            let id = out.page_as_form(page)?;
                            forms[page] = Some(id);
                            id
                        }
                    };
                    let (pw, ph) = out.page_geometry(page)?.display_size();
                    let (mut sx, mut sy) = if options.fit_to_media {
                        let s = (grid.cell_w / pw).min(grid.cell_h / ph);
                        (s, s)
                    } else {
                        (options.scale_x_percent / 100.0, options.scale_y_percent / 100.0)
                    };
                    if options.scale_large_pages && (pw * sx > grid.cell_w || ph * sy > grid.cell_h) {
                        let s = (grid.cell_w / pw).min(grid.cell_h / ph);
                        sx = sx.min(s);
                        sy = sy.min(s);
                    }
                    let (draw_w, draw_h) = (pw * sx, ph * sy);
                    let mut x = column_x[col] + (grid.cell_w - draw_w) / 2.0;
                    let y = cell_y + (grid.cell_h - draw_h) / 2.0;
                    if options.creep != 0.0 && cell.creep_steps > 0 {
                        // Move toward the spine of this spread: even columns shift right, odd left.
                        let shift = options.creep * cell.creep_steps as f32;
                        x += if col % 2 == 0 { shift } else { -shift };
                    }
                    let name = format!("Pg{page}");
                    xobjects.set(name.as_bytes().to_vec(), Object::Reference(form_id));
                    if cell.inverted {
                        content.push_str(&format!(
                            "q {} 0 0 {} {} {} cm /{name} Do Q\n",
                            fmt(-sx),
                            fmt(-sy),
                            fmt(x + draw_w),
                            fmt(y + draw_h)
                        ));
                    } else {
                        content.push_str(&format!(
                            "q {} 0 0 {} {} {} cm /{name} Do Q\n",
                            fmt(sx),
                            fmt(sy),
                            fmt(x),
                            fmt(y)
                        ));
                    }
                }
            }

            draw_marks(options, plate, &grid, &column_x, &mut content);
            if options.page_info {
                let text = options.description.clone().unwrap_or_else(|| {
                    format!(
                        "Sheet {} - {}",
                        plate.sheet + 1,
                        if plate.front { "Front" } else { "Back" }
                    )
                });
                let font_id = out.doc.add_object(dictionary! {
                    "Type" => "Font", "Subtype" => "Type1", "BaseFont" => StandardFont::Helvetica.base_font(),
                    "Encoding" => "WinAnsiEncoding",
                });
                fonts.set("CarbideInfo", Object::Reference(font_id));
                let mut line = format!("BT /CarbideInfo 7 Tf 0 g {} {} Td ", fmt(options.margins.left.max(6.0)), fmt(grid.height - 10.0))
                    .into_bytes();
                line.extend_from_slice(&literal_string(&win_ansi(&text)));
                line.extend_from_slice(b" Tj ET\n");
                content.push_str(&String::from_utf8_lossy(&line));
            }

            let content_id = out.doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
            let mut resources = Dictionary::new();
            resources.set("XObject", Object::Dictionary(xobjects));
            if !fonts.is_empty() {
                resources.set("Font", Object::Dictionary(fonts));
            }
            let mut page = dictionary! {
                "Type" => "Page",
                "Parent" => root_id,
                "MediaBox" => real_array(&[0.0, 0.0, grid.width, grid.height]),
                "Resources" => resources,
                "Contents" => content_id,
            };
            if options.flip_backs && plan.double_sided && !plate.front && output_index % 2 == 1 {
                page.set("Rotate", 180);
            }
            new_pages.push(Object::Reference(out.doc.add_object(page)));
        }

        let count = new_pages.len() as i64;
        let root = out.doc.get_dictionary_mut(root_id)?;
        root.set("Kids", new_pages);
        root.set("Count", count);
        // Outlines and named destinations pointed at pages that no longer exist.
        if let Ok(catalog_id) = out.doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
            if let Ok(catalog) = out.doc.get_dictionary_mut(catalog_id) {
                for key in [&b"Outlines"[..], b"Names", b"Dests", b"OpenAction", b"PageLabels", b"StructTreeRoot"] {
                    catalog.remove(key);
                }
            }
        }
        out.doc.prune_objects();
        // Form XObjects hold decoded page content; store it compressed again.
        out.doc.compress();
        Ok(out)
    }
}

/// Left edge of each column's cell.
fn column_origins(options: &ImpositionOptions, plate: &Plate, grid: &Grid) -> Vec<f32> {
    let mut x = options.margins.left;
    let mut origins = Vec::with_capacity(plate.cols);
    for col in 0..plate.cols {
        origins.push(x);
        x += grid.cell_w + grid.gaps.get(col).copied().unwrap_or(0.0);
    }
    origins
}

/// Crop and registration marks, drawn in the margins as thin black lines.
fn draw_marks(options: &ImpositionOptions, plate: &Plate, grid: &Grid, column_x: &[f32], out: &mut String) {
    if !options.crop_marks && !options.registration_marks {
        return;
    }
    out.push_str("q 0 G 0.5 w\n");
    if options.crop_marks {
        const LENGTH: f32 = 12.0;
        const OFFSET: f32 = 3.0;
        for row in 0..plate.rows {
            let y0 = options.margins.bottom + (plate.rows - 1 - row) as f32 * (grid.cell_h + options.gutter_vertical);
            let y1 = y0 + grid.cell_h;
            for col in 0..plate.cols {
                let outer_left = col == 0;
                let outer_right = col == plate.cols - 1;
                let outer_top = row == 0;
                let outer_bottom = row == plate.rows - 1;
                let (x0, x1) = (column_x[col], column_x[col] + grid.cell_w);
                for (x, vertical_outer_x) in [(x0, outer_left), (x1, outer_right)] {
                    for (y, horizontal_outer_y) in [(y0, outer_bottom), (y1, outer_top)] {
                        if !options.inner_marks && !(vertical_outer_x && horizontal_outer_y) {
                            continue;
                        }
                        let dx = if x == x0 { -1.0 } else { 1.0 };
                        let dy = if y == y0 { -1.0 } else { 1.0 };
                        out.push_str(&format!(
                            "{} {} m {} {} l S {} {} m {} {} l S\n",
                            fmt(x + dx * OFFSET),
                            fmt(y),
                            fmt(x + dx * (OFFSET + LENGTH)),
                            fmt(y),
                            fmt(x),
                            fmt(y + dy * OFFSET),
                            fmt(x),
                            fmt(y + dy * (OFFSET + LENGTH)),
                        ));
                    }
                }
            }
        }
    }
    if options.registration_marks {
        let (w, h) = (grid.width, grid.height);
        for (cx, cy) in [(w / 2.0, 8.0), (w / 2.0, h - 8.0), (8.0, h / 2.0), (w - 8.0, h / 2.0)] {
            out.push_str(&format!(
                "{} {} m {} {} l S {} {} m {} {} l S\n",
                fmt(cx - 5.0),
                fmt(cy),
                fmt(cx + 5.0),
                fmt(cy),
                fmt(cx),
                fmt(cy - 5.0),
                fmt(cx),
                fmt(cy + 5.0)
            ));
            if options.registration_style == RegistrationStyle::Circle {
                // Four Bezier arcs approximating a circle of radius 3.5.
                let (r, k) = (3.5f32, 3.5f32 * 0.5523);
                out.push_str(&format!(
                    "{} {} m {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c S\n",
                    fmt(cx + r), fmt(cy),
                    fmt(cx + r), fmt(cy + k), fmt(cx + k), fmt(cy + r), fmt(cx), fmt(cy + r),
                    fmt(cx - k), fmt(cy + r), fmt(cx - r), fmt(cy + k), fmt(cx - r), fmt(cy),
                    fmt(cx - r), fmt(cy - k), fmt(cx - k), fmt(cy - r), fmt(cx), fmt(cy - r),
                    fmt(cx + k), fmt(cy - r), fmt(cx + r), fmt(cy - k), fmt(cx + r), fmt(cy),
                ));
            }
        }
    }
    out.push_str("Q\n");
}

// ---- settings file -------------------------------------------------------------------------

impl ImpositionOptions {
    /// Serializes to the INI shape of the original (`[Media]` and `[Options]` sections, original
    /// key names) with the layout keys added in a `[Layout]` section.
    pub fn to_ini(&self) -> String {
        let mut s = String::new();
        let b = |v: bool| if v { "1" } else { "0" };
        s.push_str("[Layout]\n");
        s.push_str(&format!("Layout={}\n", layout_code(self.plan.layout)));
        s.push_str(&format!("Columns={}\nRows={}\n", self.plan.columns, self.plan.rows));
        s.push_str(&format!("Binding={}\n", if self.plan.binding == crate::imposition_plan::Binding::Perfect { 1 } else { 0 }));
        s.push_str(&format!("SheetsPerFold={}\n", self.plan.sheets_per_fold));
        s.push_str(&format!(
            "Style={}\n",
            match self.plan.style {
                Style::Sheetwise => 0,
                Style::WorkAndTumble => 1,
                Style::WorkAndTurn => 2,
            }
        ));
        s.push_str(&format!("DoubleSided={}\n", b(self.plan.double_sided)));
        s.push_str(&format!(
            "Printing={}\n",
            match self.plan.printing_order {
                crate::imposition_plan::PrintingOrder::Interleaved => 0,
                crate::imposition_plan::PrintingOrder::FrontsThenBacks => 1,
                crate::imposition_plan::PrintingOrder::FrontsThenBacksReversed => 2,
            }
        ));
        s.push_str(&format!(
            "GutterHorizontal={}\nGutterVertical={}\nSpread={}\nCreep={}\n",
            self.gutter_horizontal, self.gutter_vertical, self.spread, self.creep
        ));
        s.push_str("\n[Media]\n");
        match &self.media {
            Media::None => s.push_str("PaperSelected=\n"),
            Media::Named(n) => s.push_str(&format!("PaperSelected={n}\n")),
            Media::Custom { width, height } => s.push_str(&format!("PaperSelected=Custom\nCustomWidth={width}\nCustomHeight={height}\n")),
            Media::Autosize => s.push_str("PaperSelected=\n"),
        }
        s.push_str(&format!("Autosize={}\nAutoRotate={}\nFit={}\n", b(matches!(self.media, Media::Autosize)), b(self.auto_rotate), b(self.fit_to_media)));
        s.push_str(&format!("HorzScale={}\nVertScale={}\n", self.scale_x_percent, self.scale_y_percent));
        s.push_str(&format!(
            "MarginLeft={}\nMarginRight={}\nMarginTop={}\nMarginBottom={}\nUnit=pt\n",
            self.margins.left, self.margins.right, self.margins.top, self.margins.bottom
        ));
        s.push_str("\n[Options]\n");
        s.push_str(&format!("AddCropMark={}\nInnerMark={}\nAddRegMark={}\n", b(self.crop_marks), b(self.inner_marks), b(self.registration_marks)));
        s.push_str(&format!(
            "RegStyle={}\nAddPageInfo={}\nScalePage={}\nFlipBack={}\n",
            if self.registration_style == RegistrationStyle::Circle { 1 } else { 0 },
            b(self.page_info),
            b(self.scale_large_pages),
            b(self.flip_backs)
        ));
        s
    }

    /// Parses [`Self::to_ini`] output. Any malformed value gives [`messages::INVALID_SETTINGS`].
    pub fn from_ini(text: &str) -> Result<ImpositionOptions> {
        let invalid = || Error::Other(messages::INVALID_SETTINGS.into());
        let mut values = std::collections::HashMap::new();
        let mut saw_section = false;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if line.starts_with('[') && line.ends_with(']') {
                saw_section = true;
            } else if let Some((key, value)) = line.split_once('=') {
                values.insert(key.trim().to_string(), value.trim().to_string());
            } else {
                return Err(invalid());
            }
        }
        if !saw_section {
            return Err(invalid());
        }
        let get = |k: &str| values.get(k).map(String::as_str);
        let float = |k: &str, default: f32| -> Result<f32> {
            get(k).map(|v| v.parse::<f32>().map_err(|_| invalid())).unwrap_or(Ok(default))
        };
        let int = |k: &str, default: i64| -> Result<i64> {
            get(k).map(|v| v.parse::<i64>().map_err(|_| invalid())).unwrap_or(Ok(default))
        };
        let flag = |k: &str, default: bool| -> Result<bool> { Ok(int(k, default as i64)? != 0) };

        let mut o = ImpositionOptions::default();
        o.plan.layout = layout_from_code(int("Layout", 0)?).ok_or_else(invalid)?;
        o.plan.columns = int("Columns", 2)?.max(1) as usize;
        o.plan.rows = int("Rows", 1)?.max(1) as usize;
        o.plan.binding = if int("Binding", 0)? == 1 {
            crate::imposition_plan::Binding::Perfect
        } else {
            crate::imposition_plan::Binding::Saddle
        };
        o.plan.sheets_per_fold = int("SheetsPerFold", 1)?.max(1) as usize;
        o.plan.style = match int("Style", 0)? {
            0 => Style::Sheetwise,
            1 => Style::WorkAndTumble,
            2 => Style::WorkAndTurn,
            _ => return Err(invalid()),
        };
        o.plan.double_sided = flag("DoubleSided", true)?;
        o.plan.printing_order = match int("Printing", 0)? {
            0 => crate::imposition_plan::PrintingOrder::Interleaved,
            1 => crate::imposition_plan::PrintingOrder::FrontsThenBacks,
            2 => crate::imposition_plan::PrintingOrder::FrontsThenBacksReversed,
            _ => return Err(invalid()),
        };
        o.gutter_horizontal = float("GutterHorizontal", 0.0)?;
        o.gutter_vertical = float("GutterVertical", 0.0)?;
        o.spread = float("Spread", 0.0)?;
        o.creep = float("Creep", 0.0)?;
        o.media = if flag("Autosize", false)? {
            Media::Autosize
        } else {
            match get("PaperSelected") {
                None | Some("") => Media::None,
                Some("Custom") => Media::Custom { width: float("CustomWidth", 0.0)?, height: float("CustomHeight", 0.0)? },
                Some(name) => {
                    if !PAPER_SIZES.iter().any(|(n, _, _)| n.eq_ignore_ascii_case(name)) {
                        return Err(Error::Other(
                            "The custom media size in this settings file no longer exists. Recreate it or choose another size.".into(),
                        ));
                    }
                    Media::Named(name.to_string())
                }
            }
        };
        o.auto_rotate = flag("AutoRotate", true)?;
        o.fit_to_media = flag("Fit", true)?;
        o.scale_x_percent = float("HorzScale", 100.0)?;
        o.scale_y_percent = float("VertScale", 100.0)?;
        o.margins = Margins {
            left: float("MarginLeft", 0.0)?,
            right: float("MarginRight", 0.0)?,
            top: float("MarginTop", 0.0)?,
            bottom: float("MarginBottom", 0.0)?,
        };
        o.crop_marks = flag("AddCropMark", false)?;
        o.inner_marks = flag("InnerMark", false)?;
        o.registration_marks = flag("AddRegMark", false)?;
        o.registration_style = if int("RegStyle", 0)? == 1 { RegistrationStyle::Circle } else { RegistrationStyle::Cross };
        o.page_info = flag("AddPageInfo", false)?;
        o.scale_large_pages = flag("ScalePage", false)?;
        o.flip_backs = flag("FlipBack", false)?;
        Ok(o)
    }
}

fn layout_code(layout: LayoutKind) -> i64 {
    match layout {
        LayoutKind::Booklet2Up => 0,
        LayoutKind::Booklet4Up => 4,
        LayoutKind::Booklet8Up => 5,
        LayoutKind::CutStacks => 1,
        LayoutKind::StepAndRepeat => 2,
        LayoutKind::Sequential => 3,
    }
}

fn layout_from_code(code: i64) -> Option<LayoutKind> {
    Some(match code {
        0 => LayoutKind::Booklet2Up,
        4 => LayoutKind::Booklet4Up,
        5 => LayoutKind::Booklet8Up,
        1 => LayoutKind::CutStacks,
        2 => LayoutKind::StepAndRepeat,
        3 => LayoutKind::Sequential,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_convert_to_points() {
        assert!((Unit::Inch.to_points(1.0) - 72.0).abs() < 1e-4);
        assert!((Unit::Millimeter.to_points(25.4) - 72.0).abs() < 1e-3);
        assert!((Unit::Centimeter.to_points(2.54) - 72.0).abs() < 1e-3);
    }

    #[test]
    fn validation_uses_the_original_messages() {
        let mut o = ImpositionOptions::default();
        o.media = Media::None;
        assert_eq!(o.validate().unwrap_err().to_string(), messages::NO_MEDIA);
        o.media = Media::Named("Letter".into());
        o.margins = Margins { left: 400.0, right: 400.0, top: 0.0, bottom: 0.0 };
        assert_eq!(o.validate().unwrap_err().to_string(), messages::MARGINS);
    }

    #[test]
    fn settings_round_trip_and_reject_garbage() {
        let mut o = ImpositionOptions::default();
        o.plan.layout = LayoutKind::StepAndRepeat;
        o.plan.columns = 3;
        o.plan.rows = 2;
        o.creep = 1.5;
        o.crop_marks = true;
        o.media = Media::Custom { width: 300.0, height: 400.0 };
        let again = ImpositionOptions::from_ini(&o.to_ini()).unwrap();
        assert_eq!(again.plan, o.plan);
        assert_eq!(again.media, o.media);
        assert!(again.crop_marks && (again.creep - 1.5).abs() < 1e-6);
        assert_eq!(
            ImpositionOptions::from_ini("nonsense").unwrap_err().to_string(),
            messages::INVALID_SETTINGS
        );
        assert!(ImpositionOptions::from_ini("[Layout]\nColumns=abc\n").is_err());
    }
}
