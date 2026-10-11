//! Original vector icons, drawn with egui's painter (no image assets).
//!
//! `paint` draws `glyph` inside `rect` (usually a 24 to 32 point square) using `color`. Icons are
//! simple line art: stroke width scales with the rect, and nothing depends on a font.

use eframe::egui::{self, Color32, Painter, Rect};
use std::f32::consts::{PI, TAU};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Glyph {
    // quick access
    Open,
    Save,
    Print,
    Undo,
    Redo,
    Hand,
    Cursor,
    Sidebar,
    // pages
    PageInsert,
    PageExtract,
    PageDelete,
    Merge,
    Split,
    RotateLeft,
    RotateRight,
    MovePageUp,
    MovePageDown,
    // document tools
    Stamp,
    Number,
    Reduce,
    Compare,
    Scan,
    ImageExport,
    TextExport,
    // comments
    Note,
    TextBox,
    Highlight,
    HighlightArea,
    Underline,
    Strikeout,
    // security
    Redact,
    ApplyRedact,
    Lock,
    Sign,
    Certify,
    TimeStamp,
    Verify,
    Trust,
    // view and navigation
    ZoomIn,
    ZoomOut,
    FitWidth,
    FitPage,
    ActualSize,
    PrevPage,
    NextPage,
    FirstPage,
    LastPage,
    // misc
    Folder,
    NewDocument,
    Search,
    Revision,
    Recovery,
    Settings,
    Close,
    Plus,
    Document,
}

impl Glyph {
    pub const ALL: [Glyph; 56] = [
        Glyph::Open, Glyph::Save, Glyph::Print, Glyph::Undo, Glyph::Redo, Glyph::Hand, Glyph::Cursor,
        Glyph::Sidebar, Glyph::PageInsert, Glyph::PageExtract, Glyph::PageDelete, Glyph::Merge, Glyph::Split,
        Glyph::RotateLeft, Glyph::RotateRight, Glyph::MovePageUp, Glyph::MovePageDown, Glyph::Stamp,
        Glyph::Number, Glyph::Reduce, Glyph::Compare, Glyph::Scan, Glyph::ImageExport, Glyph::TextExport,
        Glyph::Note, Glyph::TextBox, Glyph::Highlight, Glyph::HighlightArea, Glyph::Underline,
        Glyph::Strikeout, Glyph::Redact, Glyph::ApplyRedact, Glyph::Lock, Glyph::Sign, Glyph::Certify,
        Glyph::TimeStamp, Glyph::Verify, Glyph::Trust, Glyph::ZoomIn, Glyph::ZoomOut, Glyph::FitWidth,
        Glyph::FitPage, Glyph::ActualSize, Glyph::PrevPage, Glyph::NextPage, Glyph::FirstPage,
        Glyph::LastPage, Glyph::Folder, Glyph::NewDocument, Glyph::Search, Glyph::Revision,
        Glyph::Recovery, Glyph::Settings, Glyph::Close, Glyph::Plus, Glyph::Document,
    ];
}

/// Unit-space point: 0.0 is the left or top edge of the icon, 1.0 is the right or bottom edge.
type P = (f32, f32);

/// Empty border around the drawing, as a fraction of the rect.
const MARGIN: f32 = 0.12;

/// Length of an arrow head, in unit space.
const HEAD: f32 = 0.2;

/// Stroke and coordinate mapping for one icon.
struct Pen<'a> {
    painter: &'a Painter,
    area: Rect,
    w: f32,
    color: Color32,
    stroke: egui::Stroke,
}

impl Pen<'_> {
    /// Maps a unit-space point into the rect, keeping the margin.
    fn pos(&self, (x, y): P) -> egui::Pos2 {
        let span = 1.0 - 2.0 * MARGIN;
        self.area.min
            + egui::vec2(
                (MARGIN + x * span) * self.area.width(),
                (MARGIN + y * span) * self.area.height(),
            )
    }

    /// Scales a unit-space length to points.
    fn scale(&self, r: f32) -> f32 {
        r * (1.0 - 2.0 * MARGIN) * self.area.width()
    }

    /// Translucent version of the icon colour, used for fills.
    fn tint(&self) -> Color32 {
        self.color.gamma_multiply(0.25)
    }

    fn points(&self, pts: &[P]) -> Vec<egui::Pos2> {
        pts.iter().map(|&p| self.pos(p)).collect()
    }

    /// Open polyline.
    fn line(&self, pts: &[P]) {
        self.painter.add(egui::Shape::line(self.points(pts), self.stroke));
    }

    /// Single straight stroke.
    fn seg(&self, a: P, b: P) {
        self.painter.line_segment([self.pos(a), self.pos(b)], self.stroke);
    }

    /// Closed outline with no fill. Works for concave outlines.
    fn closed(&self, pts: &[P]) {
        self.painter.add(egui::Shape::closed_line(self.points(pts), self.stroke));
    }

    /// Convex region with a translucent fill, outlined when `outline` is set.
    fn shape(&self, pts: &[P], outline: bool) {
        let stroke = if outline { self.stroke } else { egui::Stroke::NONE };
        self.painter.add(egui::Shape::convex_polygon(self.points(pts), self.tint(), stroke));
    }

    /// Solid rectangle in the icon colour.
    fn fill_rect(&self, a: P, b: P) {
        let r = egui::Rect::from_min_max(self.pos(a), self.pos(b));
        self.painter.rect_filled(r, 0.0_f32, self.color);
    }

    /// Rounded rectangle outline. `radius` is measured in stroke widths.
    fn frame(&self, a: P, b: P, radius: f32) {
        let r = egui::Rect::from_min_max(self.pos(a), self.pos(b));
        self.painter.rect_stroke(r, self.w * radius, self.stroke, egui::StrokeKind::Middle);
    }

    /// Circle outline with a translucent fill.
    fn circle(&self, c: P, r: f32) {
        let (center, radius) = (self.pos(c), self.scale(r));
        self.painter.circle_filled(center, radius, self.tint());
        self.painter.circle_stroke(center, radius, self.stroke);
    }

    /// Circle outline only.
    fn ring(&self, c: P, r: f32) {
        self.painter.circle_stroke(self.pos(c), self.scale(r), self.stroke);
    }

    /// Solid dot.
    fn dot(&self, c: P, r: f32) {
        self.painter.circle_filled(self.pos(c), self.scale(r), self.color);
    }

    /// Open polyline ending in an arrow head.
    fn arrow(&self, pts: &[P]) {
        self.line(pts);
        if let [.., from, tip] = pts {
            self.head(*from, *tip);
        }
    }

    /// Arrow head at `tip`, pointing away from `from`.
    fn head(&self, from: P, tip: P) {
        let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
        let len = (dx * dx + dy * dy).sqrt().max(1e-6);
        let (ux, uy) = (dx / len, dy / len);
        let back = (tip.0 - ux * HEAD, tip.1 - uy * HEAD);
        let (nx, ny) = (-uy * HEAD * 0.5, ux * HEAD * 0.5);
        self.line(&[(back.0 + nx, back.1 + ny), tip, (back.0 - nx, back.1 - ny)]);
    }
}

/// Points on a circle around `c`, from angle `a0` to `a1` in radians. Y grows downwards, so
/// increasing angles run clockwise on screen.
fn arc(c: P, r: f32, a0: f32, a1: f32, steps: usize) -> Vec<P> {
    (0..=steps)
        .map(|i| {
            let t = a0 + (a1 - a0) * i as f32 / steps as f32;
            (c.0 + r * t.cos(), c.1 + r * t.sin())
        })
        .collect()
}

/// Mirrors points left to right when `flip` is set.
fn flip_x(pts: Vec<P>, flip: bool) -> Vec<P> {
    if flip {
        pts.into_iter().map(|(x, y)| (1.0 - x, y)).collect()
    } else {
        pts
    }
}

/// Page with a folded top-right corner, spanning the box from `(x0, y0)` to `(x1, y1)`.
fn sheet(p: &Pen, x0: f32, y0: f32, x1: f32, y1: f32, fold: f32) {
    p.shape(&[(x0, y0), (x1 - fold, y0), (x1, y0 + fold), (x1, y1), (x0, y1)], true);
    p.line(&[(x1 - fold, y0), (x1 - fold, y0 + fold), (x1, y0 + fold)]);
}

/// Standard page used by the badge icons.
fn page(p: &Pen) {
    sheet(p, 0.14, 0.04, 0.6, 0.96, 0.2);
}

/// Round badge in the lower-right corner of a page.
fn badge(p: &Pen) {
    p.circle((0.8, 0.7), 0.2);
}

fn page_insert(p: &Pen) {
    page(p);
    badge(p);
    p.seg((0.8, 0.6), (0.8, 0.8));
    p.seg((0.7, 0.7), (0.9, 0.7));
}

fn page_extract(p: &Pen) {
    page(p);
    badge(p);
    p.arrow(&[(0.72, 0.78), (0.88, 0.62)]);
}

fn page_delete(p: &Pen) {
    page(p);
    badge(p);
    p.seg((0.72, 0.62), (0.88, 0.78));
    p.seg((0.88, 0.62), (0.72, 0.78));
}

fn merge(p: &Pen) {
    p.seg((0.06, 0.2), (0.36, 0.5));
    p.seg((0.06, 0.8), (0.36, 0.5));
    p.arrow(&[(0.36, 0.5), (0.6, 0.5)]);
    sheet(p, 0.62, 0.1, 0.94, 0.9, 0.12);
}

fn split(p: &Pen) {
    sheet(p, 0.06, 0.1, 0.4, 0.9, 0.12);
    p.line(&[(0.4, 0.45), (0.52, 0.25), (0.62, 0.25)]);
    p.line(&[(0.4, 0.55), (0.52, 0.75), (0.62, 0.75)]);
    sheet(p, 0.62, 0.06, 0.94, 0.42, 0.1);
    sheet(p, 0.62, 0.58, 0.94, 0.94, 0.1);
}

/// Page with a curved arrow over it. `left` gives the counter-clockwise version.
fn rotate(p: &Pen, left: bool) {
    sheet(p, 0.3, 0.46, 0.7, 0.96, 0.12);
    p.arrow(&flip_x(arc((0.5, 0.52), 0.4, -2.75, -0.4, 22), left));
}

fn move_page(p: &Pen, up: bool) {
    sheet(p, 0.14, 0.12, 0.6, 0.92, 0.16);
    if up {
        p.arrow(&[(0.8, 0.86), (0.8, 0.14)]);
    } else {
        p.arrow(&[(0.8, 0.14), (0.8, 0.86)]);
    }
}

/// Rubber stamp: knob, neck, body and an inked base.
fn stamp(p: &Pen) {
    p.frame((0.38, 0.1), (0.62, 0.34), 1.0);
    p.seg((0.5, 0.34), (0.5, 0.46));
    p.frame((0.2, 0.46), (0.8, 0.66), 1.5);
    p.fill_rect((0.12, 0.72), (0.88, 0.86));
}

/// Page with a hash sign.
fn number(p: &Pen) {
    sheet(p, 0.14, 0.04, 0.86, 0.96, 0.22);
    p.seg((0.4, 0.34), (0.35, 0.72));
    p.seg((0.62, 0.34), (0.57, 0.72));
    p.seg((0.28, 0.46), (0.72, 0.46));
    p.seg((0.25, 0.6), (0.69, 0.6));
}

/// Page in the centre with four arrows pointing in at its corners.
fn reduce(p: &Pen) {
    sheet(p, 0.3, 0.28, 0.7, 0.72, 0.12);
    p.arrow(&[(0.04, 0.04), (0.26, 0.26)]);
    p.arrow(&[(0.96, 0.04), (0.74, 0.26)]);
    p.arrow(&[(0.04, 0.96), (0.26, 0.74)]);
    p.arrow(&[(0.96, 0.96), (0.74, 0.74)]);
}

/// Two pages side by side, with one highlighted difference on the right.
fn compare(p: &Pen) {
    sheet(p, 0.04, 0.12, 0.44, 0.92, 0.1);
    sheet(p, 0.56, 0.12, 0.96, 0.92, 0.1);
    for y in [0.32, 0.5, 0.68] {
        p.seg((0.12, y), (0.36, y));
    }
    p.seg((0.6, 0.32), (0.9, 0.32));
    p.fill_rect((0.6, 0.44), (0.9, 0.56));
    p.seg((0.6, 0.68), (0.9, 0.68));
}

/// Page with corner brackets and a horizontal scan line.
fn scan(p: &Pen) {
    p.line(&[(0.06, 0.24), (0.06, 0.06), (0.24, 0.06)]);
    p.line(&[(0.76, 0.06), (0.94, 0.06), (0.94, 0.24)]);
    p.line(&[(0.94, 0.76), (0.94, 0.94), (0.76, 0.94)]);
    p.line(&[(0.24, 0.94), (0.06, 0.94), (0.06, 0.76)]);
    sheet(p, 0.26, 0.18, 0.74, 0.82, 0.12);
    p.seg((0.12, 0.5), (0.88, 0.5));
}

/// Page with a small landscape picture (mountain and sun).
fn image_export(p: &Pen) {
    sheet(p, 0.14, 0.04, 0.86, 0.96, 0.22);
    p.frame((0.26, 0.36), (0.74, 0.76), 0.5);
    p.line(&[(0.26, 0.7), (0.42, 0.52), (0.52, 0.62), (0.6, 0.54), (0.74, 0.7)]);
    p.dot((0.64, 0.46), 0.05);
}

/// Page with text lines and an arrow leaving it.
fn text_export(p: &Pen) {
    sheet(p, 0.14, 0.04, 0.86, 0.96, 0.22);
    p.seg((0.26, 0.36), (0.74, 0.36));
    p.seg((0.26, 0.5), (0.74, 0.5));
    p.seg((0.26, 0.64), (0.56, 0.64));
    p.arrow(&[(0.26, 0.82), (0.74, 0.82)]);
}

/// Speech bubble with two text lines.
fn note(p: &Pen) {
    p.shape(&[(0.1, 0.12), (0.9, 0.12), (0.9, 0.7), (0.1, 0.7)], false);
    p.closed(&[
        (0.1, 0.12),
        (0.9, 0.12),
        (0.9, 0.7),
        (0.46, 0.7),
        (0.26, 0.9),
        (0.3, 0.7),
        (0.1, 0.7),
    ]);
    p.seg((0.26, 0.32), (0.74, 0.32));
    p.seg((0.26, 0.5), (0.6, 0.5));
}

/// Rectangle with a capital T inside.
fn text_box(p: &Pen) {
    p.frame((0.08, 0.16), (0.92, 0.84), 1.5);
    p.seg((0.28, 0.32), (0.72, 0.32));
    p.seg((0.5, 0.32), (0.5, 0.68));
}

/// Marker pen over a line of text.
fn highlight(p: &Pen) {
    p.shape(&[(0.08, 0.66), (0.92, 0.66), (0.92, 0.84), (0.08, 0.84)], false);
    p.seg((0.14, 0.75), (0.86, 0.75));
    p.shape(&[(0.3, 0.66), (0.484, 0.618), (0.89, 0.21), (0.75, 0.07), (0.342, 0.476)], true);
}

/// Translucent rectangle with corner marks.
fn highlight_area(p: &Pen) {
    p.shape(&[(0.22, 0.22), (0.78, 0.22), (0.78, 0.78), (0.22, 0.78)], true);
    p.line(&[(0.08, 0.3), (0.08, 0.08), (0.3, 0.08)]);
    p.line(&[(0.7, 0.08), (0.92, 0.08), (0.92, 0.3)]);
    p.line(&[(0.92, 0.7), (0.92, 0.92), (0.7, 0.92)]);
    p.line(&[(0.3, 0.92), (0.08, 0.92), (0.08, 0.7)]);
}

/// Letter A, used by the text markup icons.
fn letter_a(p: &Pen) {
    p.line(&[(0.18, 0.74), (0.5, 0.12), (0.82, 0.74)]);
    p.seg((0.32, 0.54), (0.68, 0.54));
}

fn underline(p: &Pen) {
    letter_a(p);
    p.seg((0.12, 0.92), (0.88, 0.92));
}

fn strikeout(p: &Pen) {
    letter_a(p);
    p.seg((0.06, 0.4), (0.94, 0.4));
}

/// Text lines with a solid redaction bar across the middle.
fn redact(p: &Pen) {
    p.seg((0.12, 0.16), (0.88, 0.16));
    p.fill_rect((0.12, 0.34), (0.88, 0.66));
    p.seg((0.12, 0.84), (0.6, 0.84));
}

/// Redaction bar with a check mark below it.
fn apply_redact(p: &Pen) {
    p.seg((0.12, 0.12), (0.88, 0.12));
    p.fill_rect((0.12, 0.3), (0.88, 0.56));
    p.line(&[(0.2, 0.74), (0.42, 0.9), (0.8, 0.6)]);
}

fn lock(p: &Pen) {
    let mut shackle: Vec<P> = vec![(0.3, 0.46), (0.3, 0.34)];
    shackle.extend(arc((0.5, 0.34), 0.2, PI, TAU, 16));
    shackle.push((0.7, 0.46));
    p.line(&shackle);
    p.shape(&[(0.2, 0.46), (0.8, 0.46), (0.8, 0.9), (0.2, 0.9)], true);
    p.dot((0.5, 0.6), 0.05);
    p.seg((0.5, 0.64), (0.5, 0.76));
}

/// Pen nib writing a squiggle.
fn sign(p: &Pen) {
    p.line(&[(0.08, 0.8), (0.2, 0.68), (0.3, 0.86), (0.42, 0.72), (0.54, 0.88), (0.7, 0.76), (0.9, 0.82)]);
    p.shape(&[(0.36, 0.62), (0.557, 0.563), (0.91, 0.21), (0.77, 0.07), (0.417, 0.423)], true);
}

/// Ribbon seal with a check mark.
fn certify(p: &Pen) {
    p.circle((0.5, 0.4), 0.28);
    p.line(&[(0.4, 0.64), (0.32, 0.92), (0.42, 0.84), (0.48, 0.92)]);
    p.line(&[(0.6, 0.64), (0.68, 0.92), (0.58, 0.84), (0.52, 0.92)]);
    p.line(&[(0.38, 0.4), (0.48, 0.5), (0.64, 0.32)]);
}

/// Clock with a small page beside it.
fn time_stamp(p: &Pen) {
    p.circle((0.38, 0.38), 0.28);
    p.line(&[(0.38, 0.2), (0.38, 0.38), (0.5, 0.38)]);
    sheet(p, 0.6, 0.6, 0.94, 0.94, 0.12);
}

/// Shield with a check mark.
fn verify(p: &Pen) {
    p.shape(
        &[(0.5, 0.08), (0.84, 0.2), (0.84, 0.48), (0.74, 0.66), (0.5, 0.92), (0.26, 0.66), (0.16, 0.48), (0.16, 0.2)],
        true,
    );
    p.line(&[(0.34, 0.5), (0.46, 0.64), (0.68, 0.38)]);
}

/// Key.
fn key(p: &Pen) {
    p.circle((0.28, 0.5), 0.2);
    p.dot((0.28, 0.5), 0.05);
    p.seg((0.48, 0.5), (0.92, 0.5));
    p.seg((0.72, 0.5), (0.72, 0.66));
    p.seg((0.86, 0.5), (0.86, 0.64));
}

/// Magnifier lens and handle.
fn magnifier(p: &Pen) {
    p.circle((0.42, 0.42), 0.3);
    p.seg((0.64, 0.64), (0.9, 0.9));
}

fn zoom_in(p: &Pen) {
    magnifier(p);
    p.seg((0.42, 0.3), (0.42, 0.54));
    p.seg((0.3, 0.42), (0.54, 0.42));
}

fn zoom_out(p: &Pen) {
    magnifier(p);
    p.seg((0.3, 0.42), (0.54, 0.42));
}

/// Two arrows pointing out at vertical bars on the left and right.
fn fit_width(p: &Pen) {
    p.seg((0.1, 0.2), (0.1, 0.8));
    p.seg((0.9, 0.2), (0.9, 0.8));
    p.arrow(&[(0.4, 0.5), (0.16, 0.5)]);
    p.arrow(&[(0.6, 0.5), (0.84, 0.5)]);
}

fn fit_page(p: &Pen) {
    p.line(&[(0.1, 0.34), (0.1, 0.1), (0.34, 0.1)]);
    p.line(&[(0.66, 0.1), (0.9, 0.1), (0.9, 0.34)]);
    p.line(&[(0.9, 0.66), (0.9, 0.9), (0.66, 0.9)]);
    p.line(&[(0.34, 0.9), (0.1, 0.9), (0.1, 0.66)]);
}

/// Square with a tick mark inside.
fn actual_size(p: &Pen) {
    p.frame((0.1, 0.1), (0.9, 0.9), 0.8);
    p.line(&[(0.28, 0.52), (0.44, 0.68), (0.74, 0.34)]);
}

fn prev_page(p: &Pen) {
    p.line(&[(0.66, 0.18), (0.3, 0.5), (0.66, 0.82)]);
}

fn next_page(p: &Pen) {
    p.line(&[(0.34, 0.18), (0.7, 0.5), (0.34, 0.82)]);
}

fn first_page(p: &Pen) {
    p.seg((0.2, 0.2), (0.2, 0.8));
    p.line(&[(0.74, 0.18), (0.38, 0.5), (0.74, 0.82)]);
}

fn last_page(p: &Pen) {
    p.seg((0.8, 0.2), (0.8, 0.8));
    p.line(&[(0.26, 0.18), (0.62, 0.5), (0.26, 0.82)]);
}

/// Open palm with four fingers and a thumb.
fn hand(p: &Pen) {
    p.shape(&[(0.3, 0.5), (0.74, 0.5), (0.8, 0.6), (0.74, 0.9), (0.36, 0.9), (0.22, 0.66)], true);
    for (x, top) in [(0.36, 0.22), (0.48, 0.1), (0.6, 0.16), (0.72, 0.3)] {
        p.seg((x, 0.5), (x, top));
    }
    p.seg((0.24, 0.62), (0.12, 0.44));
}

fn cursor(p: &Pen) {
    p.closed(&[(0.26, 0.1), (0.26, 0.8), (0.42, 0.64), (0.54, 0.9), (0.66, 0.84), (0.54, 0.58), (0.76, 0.58)]);
}

/// Window with a narrow left column.
fn sidebar(p: &Pen) {
    p.shape(&[(0.1, 0.16), (0.36, 0.16), (0.36, 0.84), (0.1, 0.84)], false);
    p.frame((0.1, 0.16), (0.9, 0.84), 1.2);
    p.seg((0.36, 0.16), (0.36, 0.84));
}

fn folder(p: &Pen) {
    p.shape(&[(0.08, 0.32), (0.92, 0.32), (0.92, 0.84), (0.08, 0.84)], false);
    p.closed(&[(0.08, 0.22), (0.38, 0.22), (0.46, 0.32), (0.92, 0.32), (0.92, 0.84), (0.08, 0.84)]);
}

/// Folder with its front flap opened.
fn open_folder(p: &Pen) {
    p.line(&[(0.08, 0.84), (0.08, 0.22), (0.38, 0.22), (0.46, 0.32), (0.7, 0.32)]);
    p.seg((0.08, 0.84), (0.18, 0.84));
    p.shape(&[(0.18, 0.84), (0.3, 0.5), (0.96, 0.5), (0.84, 0.84)], true);
}

fn floppy(p: &Pen) {
    p.shape(&[(0.12, 0.12), (0.72, 0.12), (0.88, 0.28), (0.88, 0.88), (0.12, 0.88)], true);
    p.frame((0.3, 0.12), (0.68, 0.38), 0.5);
    p.frame((0.26, 0.58), (0.74, 0.82), 0.5);
}

fn printer(p: &Pen) {
    p.line(&[(0.28, 0.38), (0.28, 0.14), (0.72, 0.14), (0.72, 0.38)]);
    p.shape(&[(0.12, 0.38), (0.88, 0.38), (0.88, 0.74), (0.12, 0.74)], true);
    p.shape(&[(0.3, 0.6), (0.7, 0.6), (0.7, 0.9), (0.3, 0.9)], true);
    p.dot((0.76, 0.5), 0.04);
}

/// Curved arrow turning over the top. `redo` mirrors it.
fn undo_redo(p: &Pen, redo: bool) {
    p.arrow(&flip_x(arc((0.5, 0.56), 0.3, 0.0, -PI * 1.1, 24), redo));
}

/// History clock: a counter-clockwise arrow around clock hands.
fn revision(p: &Pen) {
    p.arrow(&arc((0.5, 0.5), 0.42, 2.3, -2.85, 28));
    p.seg((0.5, 0.5), (0.5, 0.26));
    p.seg((0.5, 0.5), (0.66, 0.6));
}

/// Lifebuoy: a ring with four spokes.
fn recovery(p: &Pen) {
    p.ring((0.5, 0.5), 0.36);
    p.ring((0.5, 0.5), 0.14);
    for k in 0..4 {
        let a = PI / 4.0 + k as f32 * PI / 2.0;
        let (c, s) = (a.cos(), a.sin());
        p.seg((0.5 + 0.14 * c, 0.5 + 0.14 * s), (0.5 + 0.36 * c, 0.5 + 0.36 * s));
    }
}

/// Gear with eight teeth and a centre hole.
fn gear(p: &Pen) {
    let teeth: [(f32, f32); 4] = [(-0.34, 0.31), (-0.2, 0.4), (0.2, 0.4), (0.34, 0.31)];
    let mut pts: Vec<P> = Vec::new();
    for k in 0..8 {
        let base = k as f32 * TAU / 8.0;
        for (d, r) in teeth {
            pts.push((0.5 + r * (base + d).cos(), 0.5 + r * (base + d).sin()));
        }
    }
    p.closed(&pts);
    p.ring((0.5, 0.5), 0.13);
}

fn close_x(p: &Pen) {
    p.seg((0.2, 0.2), (0.8, 0.8));
    p.seg((0.8, 0.2), (0.2, 0.8));
}

fn plus_sign(p: &Pen) {
    p.seg((0.5, 0.12), (0.5, 0.88));
    p.seg((0.12, 0.5), (0.88, 0.5));
}

fn document(p: &Pen) {
    sheet(p, 0.16, 0.04, 0.84, 0.96, 0.22);
    p.seg((0.3, 0.42), (0.7, 0.42));
    p.seg((0.3, 0.58), (0.7, 0.58));
    p.seg((0.3, 0.74), (0.56, 0.74));
}

/// Blank page with a plus sign.
fn new_document(p: &Pen) {
    sheet(p, 0.16, 0.04, 0.84, 0.96, 0.22);
    p.seg((0.5, 0.4), (0.5, 0.72));
    p.seg((0.34, 0.56), (0.66, 0.56));
}

/// Draws `glyph` inside `rect`.
pub fn paint(painter: &Painter, rect: Rect, glyph: Glyph, color: Color32) {
    let w = (rect.width() / 16.0).max(1.0);
    let p = Pen {
        painter,
        area: rect,
        w,
        color,
        stroke: egui::Stroke::new(w, color),
    };
    match glyph {
        Glyph::Open => open_folder(&p),
        Glyph::Save => floppy(&p),
        Glyph::Print => printer(&p),
        Glyph::Undo => undo_redo(&p, false),
        Glyph::Redo => undo_redo(&p, true),
        Glyph::Hand => hand(&p),
        Glyph::Cursor => cursor(&p),
        Glyph::Sidebar => sidebar(&p),
        Glyph::PageInsert => page_insert(&p),
        Glyph::PageExtract => page_extract(&p),
        Glyph::PageDelete => page_delete(&p),
        Glyph::Merge => merge(&p),
        Glyph::Split => split(&p),
        Glyph::RotateLeft => rotate(&p, true),
        Glyph::RotateRight => rotate(&p, false),
        Glyph::MovePageUp => move_page(&p, true),
        Glyph::MovePageDown => move_page(&p, false),
        Glyph::Stamp => stamp(&p),
        Glyph::Number => number(&p),
        Glyph::Reduce => reduce(&p),
        Glyph::Compare => compare(&p),
        Glyph::Scan => scan(&p),
        Glyph::ImageExport => image_export(&p),
        Glyph::TextExport => text_export(&p),
        Glyph::Note => note(&p),
        Glyph::TextBox => text_box(&p),
        Glyph::Highlight => highlight(&p),
        Glyph::HighlightArea => highlight_area(&p),
        Glyph::Underline => underline(&p),
        Glyph::Strikeout => strikeout(&p),
        Glyph::Redact => redact(&p),
        Glyph::ApplyRedact => apply_redact(&p),
        Glyph::Lock => lock(&p),
        Glyph::Sign => sign(&p),
        Glyph::Certify => certify(&p),
        Glyph::TimeStamp => time_stamp(&p),
        Glyph::Verify => verify(&p),
        Glyph::Trust => key(&p),
        Glyph::ZoomIn => zoom_in(&p),
        Glyph::ZoomOut => zoom_out(&p),
        Glyph::FitWidth => fit_width(&p),
        Glyph::FitPage => fit_page(&p),
        Glyph::ActualSize => actual_size(&p),
        Glyph::PrevPage => prev_page(&p),
        Glyph::NextPage => next_page(&p),
        Glyph::FirstPage => first_page(&p),
        Glyph::LastPage => last_page(&p),
        Glyph::Folder => folder(&p),
        Glyph::NewDocument => new_document(&p),
        Glyph::Search => magnifier(&p),
        Glyph::Revision => revision(&p),
        Glyph::Recovery => recovery(&p),
        Glyph::Settings => gear(&p),
        Glyph::Close => close_x(&p),
        Glyph::Plus => plus_sign(&p),
        Glyph::Document => document(&p),
    }
}
