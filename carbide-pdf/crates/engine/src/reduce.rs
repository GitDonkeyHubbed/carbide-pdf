//! Reduce file size.
//!
//! Follows the original feature: one quality slider (0 to 100, default 50) that affects only
//! images; byte-identical streams are merged; images are re-encoded and down-sampled according to
//! how large they are drawn; the result is written with object streams and a cross-reference stream
//! and is rejected unless the whole file got smaller.
//!
//! Deviations (the original used JPEG 2000 and JBIG2 encoders that are not available here):
//! photographic images are re-encoded as JPEG, 1-bit and JPEG 2000 sources are left unchanged,
//! CMYK images are left unchanged, an image is only replaced if the new data is smaller, and the
//! output is not linearized.

use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use flate2::{write::ZlibEncoder, Compression};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, ExtendedColorType, ImageFormat};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::content::{tokens, Token};
use crate::error::{Error, Result};
use crate::fs::Existing;
use crate::limits::DecodeBudget;
use crate::pdf::Pdf;

pub const DEFAULT_IMAGE_FACTOR: u8 = 50;
/// Added to the file names by default, so the originals are kept.
pub const DEFAULT_SUFFIX: &str = "-reduced";

/// User-facing messages.
pub mod messages {
    pub const NOT_SMALLER: &str = "The result is not smaller than the original file.";
    pub const XFA: &str = "XFA forms cannot be reduced.";
    pub const NO_PERMISSION: &str = "This document's security settings do not allow that change.\nChange them in the document properties and try again.";
    pub const OPEN_FAILED: &str = "This document's security settings do not allow that change.";
    pub const SAVE_ERROR: &str = "The document could not be saved.";
    pub const PORTFOLIO: &str = "Skipped";
    pub const PROTECTED: &str = "Password-protected documents cannot be reduced: the result would not be encrypted.";
    pub const NEED_SAVE: &str = "Save this document before reducing it. Save now and continue?";
    pub const CHOOSE_FOLDER: &str = "Choose a folder for the output files.";
    pub const INVALID_PATH: &str = "That folder does not exist.";
    pub const FILE_EXISTS: &str = "These settings would overwrite existing files…";
    pub const REPLACES_ORIGINAL: &str =
        "These settings would replace the original file. Add text to the file names, choose another folder, or choose to replace the original files.";
    pub const OUTPUT_COLLIDES: &str = "Another file in the list would be written to the same file, or be replaced by this one's result.";
    pub const BATCH_STOPPED: &str = "Not reduced: other files in the list would be written over each other.";
    pub const SLIDER_NOTE: &str = "The slider only changes how images are compressed.";
}

/// Why a document is refused before any work is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Xfa,
    NoPermission,
    Portfolio,
    /// The reduced file is written without encryption, so it would expose the content.
    Protected,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Refusal::Xfa => messages::XFA,
            Refusal::NoPermission => messages::NO_PERMISSION,
            Refusal::Portfolio => messages::PORTFOLIO,
            Refusal::Protected => messages::PROTECTED,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReduceOptions {
    /// Slider position: 0 (small size) to 100 (high quality).
    pub image_factor: u8,
}

impl Default for ReduceOptions {
    fn default() -> Self {
        Self { image_factor: DEFAULT_IMAGE_FACTOR }
    }
}

impl ReduceOptions {
    /// `f = factor/100`, clamped to 0.05..=0.95.
    fn f(&self) -> f32 {
        (self.image_factor.min(100) as f32 / 100.0).clamp(0.05, 0.95)
    }
    /// Scales the down-sampling target: 1.0 at the default slider position.
    fn k2(&self) -> f32 {
        self.f() + 0.5
    }
    fn jpeg_quality(&self) -> u8 {
        (30.0 + 65.0 * self.f()).round().clamp(10.0, 95.0) as u8
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReduceReport {
    pub size_before: usize,
    pub size_after: usize,
    pub images_recompressed: usize,
    pub images_downsampled: usize,
    pub streams_merged: usize,
    pub streams_recompressed: usize,
}

/// Pixels per inch that "drawn at 100 %" corresponds to when sizing images.
const REFERENCE_PPI: f32 = 150.0;
const MIN_SIDE: u32 = 24;

// ---- eligibility ---------------------------------------------------------------------------

impl Pdf {
    pub fn reduce_refusal(&self) -> Option<Refusal> {
        if self.doc.was_encrypted() || self.doc.is_encrypted() {
            return Some(Refusal::Protected);
        }
        let catalog = self.doc.catalog().ok()?;
        if catalog.has(b"Collection") {
            return Some(Refusal::Portfolio);
        }
        if let Ok(form) = catalog.get(b"AcroForm") {
            if let Object::Dictionary(form) = self.resolve(form) {
                if form.has(b"XFA") {
                    return Some(Refusal::Xfa);
                }
                // Signatures exist and the file is append-only: a full save would invalidate them.
                if let Ok(Object::Integer(flags)) = form.get(b"SigFlags").map(|o| self.resolve(o)) {
                    if flags & 0b11 == 0b11 {
                        return Some(Refusal::NoPermission);
                    }
                }
            }
        }
        None
    }
}

// ---- reduce --------------------------------------------------------------------------------

fn flate(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(9));
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// Product of two PDF matrices: apply `a` first, then `b`.
fn multiply(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

fn number(object: &Object) -> Option<f32> {
    object.as_float().ok()
}

fn matrix_from(object: Option<&Object>) -> [f32; 6] {
    if let Some(Object::Array(values)) = object {
        let numbers: Vec<f32> = values.iter().filter_map(number).collect();
        if numbers.len() == 6 {
            return [numbers[0], numbers[1], numbers[2], numbers[3], numbers[4], numbers[5]];
        }
    }
    [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
}

/// The most forms followed while looking for the sizes images are drawn at, and the most bytes of
/// their content read, each counted every time a form is drawn. Images drawn only by forms past
/// these are recompressed but not down-sampled.
const MAX_FORMS_FOLLOWED: usize = 100_000;
const MAX_FORM_BYTES_READ: usize = crate::limits::MAX_DECODED;

/// What scanning the content of a document for drawn image sizes has decoded and followed so far.
struct Scan {
    budget: DecodeBudget,
    forms: HashMap<ObjectId, std::rc::Rc<Vec<u8>>>,
    /// The forms being drawn, outermost first. A form that draws itself is not followed again.
    path: Vec<ObjectId>,
    /// How many forms have been followed, and how many bytes of their content read.
    followed: usize,
    read: usize,
}

impl Pdf {
    /// Largest drawn size, in points, of every image XObject (page content and nested forms).
    /// Content is decoded within one budget for the whole document (see [`crate::limits`]).
    fn drawn_sizes(&self) -> Result<HashMap<ObjectId, (f32, f32)>> {
        let mut sizes: HashMap<ObjectId, (f32, f32)> = HashMap::new();
        let mut scan = Scan { budget: DecodeBudget::new(), forms: HashMap::new(), path: Vec::new(), followed: 0, read: 0 };
        for page_id in self.page_ids() {
            let resources = self
                .inherited(page_id, b"Resources")
                .and_then(|o| o.as_dict().ok().cloned())
                .unwrap_or_default();
            let content = scan.budget.page_content(&self.doc, page_id)?;
            self.scan_content(&content, &resources, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], &mut sizes, &mut scan)?;
        }
        Ok(sizes)
    }

    /// Records the size each image `content` draws is drawn at, `base` being the matrix the
    /// content is drawn with, and follows the forms it draws.
    fn scan_content(
        &self,
        content: &[u8],
        resources: &Dictionary,
        base: [f32; 6],
        sizes: &mut HashMap<ObjectId, (f32, f32)>,
        scan: &mut Scan,
    ) -> Result<()> {
        if scan.path.len() > 8 {
            return Ok(());
        }
        let xobjects = match resources.get(b"XObject").map(|o| self.resolve(o)) {
            Ok(Object::Dictionary(d)) => d,
            _ => return Ok(()),
        };
        let mut ctm = base;
        let mut stack: Vec<[f32; 6]> = Vec::new();
        // The operands since the last operator: how many, the numbers among the first six, and the
        // last name.
        let (mut operands, mut numbers, mut name) = (0usize, Vec::with_capacity(6), None);
        let mut failure = None;
        tokens(content, |token| {
            let operator = match token {
                Token::Operator(operator) => operator,
                other => {
                    operands += 1;
                    match other {
                        Token::Number(n) if numbers.len() < 6 => numbers.push(n),
                        Token::Name(n) => name = Some(n),
                        _ => {}
                    }
                    return;
                }
            };
            match operator {
                b"q" => stack.push(ctm),
                b"Q" => {
                    if let Some(saved) = stack.pop() {
                        ctm = saved;
                    }
                }
                b"cm" if operands == 6 && numbers.len() == 6 => {
                    ctm = multiply([numbers[0], numbers[1], numbers[2], numbers[3], numbers[4], numbers[5]], ctm);
                }
                b"Do" if operands == 1 && failure.is_none() => {
                    if let Some(name) = name {
                        if let Err(error) = self.draw(&xobjects, &crate::content::name(name), resources, ctm, sizes, scan) {
                            failure = Some(error);
                        }
                    }
                }
                _ => {}
            }
            (operands, name) = (0, None);
            numbers.clear();
        });
        failure.map_or(Ok(()), Err)
    }

    /// Records what drawing the XObject `name` of `xobjects` with the matrix `ctm` draws.
    fn draw(
        &self,
        xobjects: &Dictionary,
        name: &[u8],
        resources: &Dictionary,
        ctm: [f32; 6],
        sizes: &mut HashMap<ObjectId, (f32, f32)>,
        scan: &mut Scan,
    ) -> Result<()> {
        let Ok(id) = xobjects.get(name).and_then(Object::as_reference) else { return Ok(()) };
        let Ok(Object::Stream(stream)) = self.doc.get_object(id) else { return Ok(()) };
        match stream.dict.get(b"Subtype").and_then(|o| o.as_name()) {
            Ok(b"Image") => {
                let w = (ctm[0] * ctm[0] + ctm[1] * ctm[1]).sqrt();
                let h = (ctm[2] * ctm[2] + ctm[3] * ctm[3]).sqrt();
                let slot = sizes.entry(id).or_insert((0.0, 0.0));
                slot.0 = slot.0.max(w);
                slot.1 = slot.1.max(h);
            }
            Ok(b"Form") if !scan.path.contains(&id) && scan.followed < MAX_FORMS_FOLLOWED => {
                scan.followed += 1;
                let form_resources = match stream.dict.get(b"Resources").map(|o| self.resolve(o)) {
                    Ok(Object::Dictionary(d)) => d,
                    _ => resources.clone(),
                };
                let matrix = matrix_from(stream.dict.get(b"Matrix").ok());
                // A form drawn on every page is decoded once.
                let body = match scan.forms.get(&id) {
                    Some(body) => body.clone(),
                    None => {
                        let body = match scan.budget.stream(stream) {
                            Err(Error::TooLarge(message)) => return Err(Error::TooLarge(message)),
                            other => std::rc::Rc::new(other.unwrap_or_default()),
                        };
                        scan.forms.insert(id, body.clone());
                        body
                    }
                };
                if scan.read.saturating_add(body.len()) > MAX_FORM_BYTES_READ {
                    return Ok(());
                }
                scan.read += body.len();
                scan.path.push(id);
                let scanned = self.scan_content(&body, &form_resources, multiply(matrix, ctm), sizes, scan);
                scan.path.pop();
                scanned?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Objects used as soft masks or stencil masks. They are only re-Flated, never JPEG-encoded
    /// or resampled on their own, because that would alter transparency edges.
    fn mask_ids(&self) -> std::collections::HashSet<ObjectId> {
        let mut ids = std::collections::HashSet::new();
        for object in self.doc.objects.values() {
            if let Object::Stream(stream) = object {
                for key in [&b"SMask"[..], b"Mask"] {
                    if let Ok(Object::Reference(id)) = stream.dict.get(key) {
                        ids.insert(*id);
                    }
                }
            }
        }
        ids
    }

    /// Merges byte-identical streams (same dictionary and same data) into one object.
    fn merge_identical_streams(&mut self) -> usize {
        let mut first: HashMap<(Vec<u8>, Vec<u8>), ObjectId> = HashMap::new();
        let mut replace: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for (id, object) in &self.doc.objects {
            let Object::Stream(stream) = object else { continue };
            if let Ok(name) = stream.dict.get(b"Type").and_then(|o| o.as_name()) {
                if matches!(name, b"ObjStm" | b"XRef" | b"Metadata") {
                    continue;
                }
            }
            let mut dict_key = Vec::new();
            let mut entries: Vec<_> = stream.dict.iter().filter(|(k, _)| k.as_slice() != b"Length").collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in entries {
                dict_key.extend_from_slice(k);
                dict_key.extend_from_slice(format!("{v:?}").as_bytes());
            }
            match first.get(&(dict_key.clone(), stream.content.clone())) {
                Some(original) => {
                    replace.insert(*id, *original);
                }
                None => {
                    first.insert((dict_key, stream.content.clone()), *id);
                }
            }
        }
        if replace.is_empty() {
            return 0;
        }
        fn rewrite(object: &mut Object, map: &BTreeMap<ObjectId, ObjectId>) {
            match object {
                Object::Reference(id) => {
                    if let Some(target) = map.get(id) {
                        *id = *target;
                    }
                }
                Object::Array(items) => items.iter_mut().for_each(|o| rewrite(o, map)),
                Object::Dictionary(d) => d.iter_mut().for_each(|(_, o)| rewrite(o, map)),
                Object::Stream(s) => s.dict.iter_mut().for_each(|(_, o)| rewrite(o, map)),
                _ => {}
            }
        }
        for object in self.doc.objects.values_mut() {
            rewrite(object, &replace);
        }
        for (_, value) in self.doc.trailer.iter_mut() {
            rewrite(value, &replace);
        }
        let count = replace.len();
        for id in replace.keys() {
            self.doc.objects.remove(id);
        }
        count
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Raw,
    Flate,
    Jpeg,
}

struct Decoded {
    width: u32,
    height: u32,
    components: u32,
    samples: Vec<u8>,
    source: Source,
}

/// The most bytes one image's samples may take to be re-encoded: 128 MB, a page scanned in colour
/// at 600 dpi. A larger image is left as it is, so that reducing never holds more than a few
/// times this.
const MAX_IMAGE_BYTES: u64 = 128 << 20;

/// What the images of one reduce may decode to in all: 1 GB, and 32 times what they take in the
/// file, so that real images, which compress far less than that, are all decoded, while a few
/// kilobytes of data that decode to gigabytes are not. Images past it are left as they are.
fn image_budget(stored: usize) -> usize {
    crate::limits::MAX_DECODED.saturating_add(stored.saturating_mul(32))
}

/// Decodes an 8-bit gray or RGB image, or `None` when this image should be left alone. What it
/// decodes to is taken from `budget`; an image that would take more than is left is not decoded.
fn decode_image(doc: &Document, stream: &Stream, budget: &mut usize) -> Option<Decoded> {
    let dict = &stream.dict;
    if matches!(dict.get(b"ImageMask"), Ok(Object::Boolean(true))) {
        return None;
    }
    if dict.has(b"Mask") || dict.has(b"Decode") {
        return None;
    }
    let width = dict.get(b"Width").ok()?.as_i64().ok()? as u32;
    let height = dict.get(b"Height").ok()?.as_i64().ok()? as u32;
    let bpc = dict.get(b"BitsPerComponent").ok().and_then(|o| o.as_i64().ok()).unwrap_or(8);
    let components = match dict.get(b"ColorSpace").ok().map(|o| match o {
        Object::Reference(id) => doc.get_object(*id).cloned().unwrap_or(Object::Null),
        other => other.clone(),
    }) {
        Some(Object::Name(n)) => match n.as_slice() {
            b"DeviceGray" | b"CalGray" => 1,
            b"DeviceRGB" | b"CalRGB" => 3,
            _ => return None,
        },
        _ => return None, // ICCBased, Indexed, Separation, ...: leave unchanged
    };
    let expected = width as u64 * height as u64 * components as u64;
    if width == 0 || height == 0 || expected > MAX_IMAGE_BYTES {
        return None;
    }
    let expected = expected as usize;
    *budget = budget.checked_sub(expected)?;

    let filter: Vec<Vec<u8>> = match dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![n.clone()],
        Ok(Object::Array(items)) => items.iter().filter_map(|o| o.as_name().ok().map(|n| n.to_vec())).collect(),
        Ok(_) => return None,
        Err(_) => Vec::new(),
    };

    match filter.as_slice() {
        [] if bpc == 8 => {
            (stream.content.len() == expected).then(|| Decoded {
                width,
                height,
                components,
                samples: stream.content.clone(),
                source: Source::Raw,
            })
        }
        [f] if f == b"FlateDecode" && bpc == 8 && !dict.has(b"DecodeParms") => {
            // The samples of an image this size, and no more.
            let samples = stream.get_plain_content_with_limit(expected).ok()?;
            (samples.len() == expected).then_some(Decoded { width, height, components, samples, source: Source::Flate })
        }
        [f] if f == b"DCTDecode" && bpc == 8 => {
            // The JPEG data says its own size, which may not be the one in the dictionary.
            let mut reader = image::ImageReader::with_format(Cursor::new(&stream.content), ImageFormat::Jpeg);
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(width);
            limits.max_image_height = Some(height);
            limits.max_alloc = Some(MAX_IMAGE_BYTES * 2);
            reader.limits(limits);
            let image = reader.decode().ok()?;
            let (w, h) = (image.width(), image.height());
            let (samples, n) = match components {
                1 => (image.to_luma8().into_raw(), 1),
                _ => (image.to_rgb8().into_raw(), 3),
            };
            (w == width && h == height).then_some(Decoded { width: w, height: h, components: n, samples, source: Source::Jpeg })
        }
        _ => None,
    }
}

fn encode_jpeg(samples: &[u8], width: u32, height: u32, components: u32, quality: u8) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    let color = if components == 1 { ExtendedColorType::L8 } else { ExtendedColorType::Rgb8 };
    JpegEncoder::new_with_quality(&mut out, quality).encode(samples, width, height, color).ok()?;
    Some(out.into_inner())
}

impl Pdf {
    /// Re-encodes images in place. Returns (recompressed, downsampled).
    fn recompress_images(&mut self, options: &ReduceOptions) -> Result<(usize, usize)> {
        let drawn = self.drawn_sizes()?;
        let masks = self.mask_ids();
        let ids: Vec<ObjectId> = self
            .doc
            .objects
            .iter()
            .filter_map(|(id, o)| match o {
                Object::Stream(s) if matches!(s.dict.get(b"Subtype").and_then(|o| o.as_name()), Ok(b"Image")) => {
                    Some(*id)
                }
                _ => None,
            })
            .collect();

        let stored = ids.iter().filter_map(|id| self.doc.get_object(*id).and_then(Object::as_stream).ok()).map(|s| s.content.len()).sum();
        let mut budget = image_budget(stored);
        let (mut recompressed, mut downsampled) = (0, 0);
        for id in ids {
            let Ok(Object::Stream(original)) = self.doc.get_object(id).cloned() else { continue };
            let is_mask = masks.contains(&id);
            let old_len = original.content.len();
            let Some(decoded) = decode_image(&self.doc, &original, &mut budget) else { continue };
            drop(original);
            let Decoded { width, height, components, samples, source } = decoded;

            // Target size from how large the image is drawn.
            let (mut width, mut height, mut samples) = (width, height, samples);
            let mut resized = false;
            if !is_mask {
                if let Some(&(draw_w, draw_h)) = drawn.get(&id) {
                    if draw_w > 0.0 && draw_h > 0.0 {
                        let k2 = options.k2();
                        let want_w = (draw_w / 72.0 * REFERENCE_PPI * k2).ceil().max(1.0) as u32;
                        let want_h = (draw_h / 72.0 * REFERENCE_PPI * k2).ceil().max(1.0) as u32;
                        let scale = (want_w as f32 / width as f32).min(want_h as f32 / height as f32);
                        if scale < 1.0 {
                            let (nw, nh) = ((width as f32 * scale) as u32, (height as f32 * scale) as u32);
                            if nw >= MIN_SIDE && nh >= MIN_SIDE {
                                let resized_samples = if components == 1 {
                                    image::GrayImage::from_raw(width, height, std::mem::take(&mut samples))
                                        .map(|img| image::imageops::resize(&img, nw, nh, FilterType::Triangle).into_raw())
                                } else {
                                    image::RgbImage::from_raw(width, height, std::mem::take(&mut samples))
                                        .map(|img| image::imageops::resize(&img, nw, nh, FilterType::Triangle).into_raw())
                                };
                                if let Some(new_samples) = resized_samples {
                                    samples = new_samples;
                                    width = nw;
                                    height = nh;
                                    resized = true;
                                }
                            }
                        }
                    }
                }
            }

            // Photographs are JPEG-encoded; flat graphics stay lossless.
            let sample_for_estimate = &samples[..samples.len().min(1 << 20)];
            let flate_ratio = flate(sample_for_estimate).len() as f32 / sample_for_estimate.len().max(1) as f32;
            let photo_like = flate_ratio > 0.25 && width as u64 * height as u64 > 64 * 1024;
            let use_jpeg = !is_mask && photo_like && (source != Source::Jpeg || resized || options.f() < 0.5);

            let (data, filter): (Vec<u8>, &'static str) = if use_jpeg {
                match encode_jpeg(&samples, width, height, components, options.jpeg_quality()) {
                    Some(jpeg) => (jpeg, "DCTDecode"),
                    None => continue,
                }
            } else if source == Source::Jpeg && !resized {
                continue; // nothing to gain without changing the pixels
            } else {
                (flate(&samples), "FlateDecode")
            };

            // The original replaces unconditionally; here an image never gets bigger.
            if data.len() >= old_len && !resized {
                continue;
            }
            if data.len() >= old_len && resized && source == Source::Jpeg && filter == "FlateDecode" {
                continue;
            }

            if let Ok(Object::Stream(stream)) = self.doc.get_object_mut(id) {
                stream.dict.remove(b"DecodeParms");
                stream.dict.remove(b"Metadata");
                stream.dict.set("Width", width as i64);
                stream.dict.set("Height", height as i64);
                stream.dict.set("BitsPerComponent", 8);
                stream.dict.set("Filter", Object::Name(filter.as_bytes().to_vec()));
                stream.dict.set("Length", data.len() as i64);
                stream.content = data;
                stream.allows_compression = false;
                recompressed += 1;
                if resized {
                    downsampled += 1;
                }
            }
        }
        Ok((recompressed, downsampled))
    }

    /// Re-Flates other streams stored uncompressed or with an older filter.
    fn reflate_streams(&mut self) -> usize {
        let ids: Vec<ObjectId> = self.doc.objects.keys().copied().collect();
        let mut changed = 0;
        for id in ids {
            let Ok(Object::Stream(stream)) = self.doc.get_object(id) else { continue };
            if let Ok(name) = stream.dict.get(b"Type").and_then(|o| o.as_name()) {
                if matches!(name, b"ObjStm" | b"XRef" | b"Metadata") {
                    continue;
                }
            }
            if matches!(stream.dict.get(b"Subtype").and_then(|o| o.as_name()), Ok(b"Image")) {
                continue;
            }
            if stream.dict.has(b"Filter") {
                continue; // already encoded (Flate, DCT, ...): leave as is
            }
            let data = stream.content.clone();
            if data.len() < 64 {
                continue;
            }
            let compressed = flate(&data);
            if compressed.len() < data.len() {
                if let Ok(Object::Stream(stream)) = self.doc.get_object_mut(id) {
                    stream.dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                    stream.dict.set("Length", compressed.len() as i64);
                    stream.content = compressed;
                    stream.allows_compression = false;
                    changed += 1;
                }
            }
        }
        changed
    }

    /// Reduces this document and returns the new file bytes. `size_before` is the size of the file
    /// the document was loaded from (or of its current serialization when `None`).
    ///
    /// Fails with [`messages::NOT_SMALLER`] when the result is not smaller than the input.
    pub fn reduce(&mut self, options: &ReduceOptions, size_before: Option<usize>) -> Result<(Vec<u8>, ReduceReport)> {
        if let Some(refusal) = self.reduce_refusal() {
            return Err(Error::Other(refusal.message().to_string()));
        }
        let before = match size_before {
            Some(n) => n,
            None => self.clone().to_bytes()?.len(),
        };

        let mut work = self.clone();
        let mut report = ReduceReport { size_before: before, ..Default::default() };
        report.streams_merged = work.merge_identical_streams();
        let (recompressed, downsampled) = work.recompress_images(options)?;
        report.images_recompressed = recompressed;
        report.images_downsampled = downsampled;
        report.streams_recompressed = work.reflate_streams();
        crate::save::prune(&mut work.doc);
        crate::pdf::writable_numbers(&mut work.doc);

        let save = lopdf::SaveOptions::builder()
            .use_object_streams(true)
            .use_xref_streams(true)
            .compression_level(9)
            .build();
        let mut out = Vec::new();
        work.doc.save_with_options(&mut out, save)?;
        report.size_after = out.len();
        if report.size_after >= before {
            return Err(Error::Other(messages::NOT_SMALLER.to_string()));
        }
        Ok((out, report))
    }
}

// ---- batch and output options --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetFolder {
    SameAsOriginal,
    Specific(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputOptions {
    pub folder: TargetFolder,
    /// "Add to original file names": `prefix + stem + suffix + ".pdf"`. Both empty keeps the name.
    pub prefix: String,
    pub suffix: String,
    /// Replace other files already where the results go.
    pub overwrite: bool,
    /// Replace each original with its reduced copy. Results that would land on their original
    /// are refused without it, whatever `overwrite` says.
    pub replace_originals: bool,
}

impl Default for OutputOptions {
    /// Next to the originals, with [`DEFAULT_SUFFIX`] added to the names, replacing nothing.
    fn default() -> Self {
        Self {
            folder: TargetFolder::SameAsOriginal,
            prefix: String::new(),
            suffix: DEFAULT_SUFFIX.to_string(),
            overwrite: false,
            replace_originals: false,
        }
    }
}

impl OutputOptions {
    /// Replace every original with its reduced copy, as an explicit choice.
    pub fn in_place() -> Self {
        Self { suffix: String::new(), overwrite: true, replace_originals: true, ..Self::default() }
    }

    /// Output path for `input`, validated the way the Reduce dialog does.
    pub fn output_path(&self, input: &Path) -> std::result::Result<PathBuf, &'static str> {
        let directory = match &self.folder {
            TargetFolder::SameAsOriginal => input.parent().map(Path::to_path_buf).unwrap_or_default(),
            TargetFolder::Specific(path) if path.as_os_str().is_empty() => return Err(messages::CHOOSE_FOLDER),
            TargetFolder::Specific(path) => path.clone(),
        };
        let directory = if directory.as_os_str().is_empty() { PathBuf::from(".") } else { directory };
        if !directory.is_dir() {
            return Err(messages::INVALID_PATH);
        }
        let name = if self.prefix.is_empty() && self.suffix.is_empty() {
            input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            let stem = input.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{}{}{}.pdf", self.prefix, stem, self.suffix)
        };
        let path = directory.join(name);
        if file_identity(&path) == file_identity(input) {
            return if self.replace_originals { Ok(path) } else { Err(messages::REPLACES_ORIGINAL) };
        }
        // A link counts as taken even when it leads nowhere: writing would create its target.
        if !self.overwrite && path.symlink_metadata().is_ok() {
            return Err(messages::FILE_EXISTS);
        }
        Ok(path)
    }

    /// What writing a result to `target`, planned for `input`, does with a file already there:
    /// replace it only when that was chosen (`overwrite`, or replacing the original itself).
    fn existing(&self, input: &Path, target: &Path) -> Existing {
        let replaces_original = self.replace_originals && file_identity(target) == file_identity(input);
        if self.overwrite || replaces_original {
            Existing::Replace
        } else {
            Existing::Refuse
        }
    }

    /// Where each of `inputs` goes. When two of them would be written to the same file, or one's
    /// result would replace another input before that one is read, every entry involved gets
    /// [`messages::OUTPUT_COLLIDES`]: such a batch would lose a file.
    pub fn plan(&self, inputs: &[PathBuf]) -> Vec<std::result::Result<PathBuf, &'static str>> {
        let mut planned: Vec<_> = inputs.iter().map(|input| self.output_path(input)).collect();
        let sources: Vec<PathBuf> = inputs.iter().map(|input| file_identity(input)).collect();
        let targets: Vec<Option<PathBuf>> = planned.iter().map(|p| p.as_ref().ok().map(|p| file_identity(p))).collect();
        for i in 0..planned.len() {
            let Some(target) = &targets[i] else { continue };
            let collides = (0..inputs.len())
                .filter(|&j| j != i)
                .any(|j| sources[j] == *target || targets[j].as_ref() == Some(target) || sources[j] == sources[i]);
            if collides {
                planned[i] = Err(messages::OUTPUT_COLLIDES);
            }
        }
        planned
    }
}

/// What `path` names, for telling whether two paths are the same file: links followed, and folded
/// to lower case where file systems usually ignore case. Folding can only find more paths equal,
/// which refuses a batch rather than lose a file.
fn file_identity(path: &Path) -> PathBuf {
    let absolute = match (path.canonicalize(), path.parent(), path.file_name()) {
        (Ok(resolved), _, _) => resolved,
        (Err(_), Some(dir), Some(name)) => {
            let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
            dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).join(name)
        }
        _ => path.to_path_buf(),
    };
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        PathBuf::from(absolute.to_string_lossy().to_lowercase())
    } else {
        absolute
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    /// `None` on success, otherwise the message the original would show in "Conversion Warnings".
    pub warning: Option<String>,
    pub report: Option<ReduceReport>,
}

/// Reduces several files. A failed file leaves its original untouched, even when the output path
/// is the input path (in-place reduction, which needs `replace_originals`). Returns one outcome per
/// input, in order.
///
/// The output paths are planned before any file is read: when two inputs would be written to the
/// same file, or a result would replace another input, nothing is reduced at all.
pub fn reduce_files(inputs: &[PathBuf], output: &OutputOptions, options: &ReduceOptions) -> Vec<FileOutcome> {
    let planned = output.plan(inputs);
    let stopped = planned.contains(&Err(messages::OUTPUT_COLLIDES));
    inputs
        .iter()
        .zip(planned)
        .map(|(input, planned)| {
            let fail = |message: &str| FileOutcome {
                input: input.clone(),
                output: None,
                warning: Some(message.to_string()),
                report: None,
            };
            let target = match planned {
                Ok(_) if stopped => return fail(messages::BATCH_STOPPED),
                Ok(path) => path,
                Err(message) => return fail(message),
            };
            let bytes = match std::fs::read(input) {
                Ok(b) => b,
                Err(_) => return fail(messages::OPEN_FAILED),
            };
            let mut pdf = match Pdf::from_bytes(&bytes) {
                Ok(p) => p,
                Err(_) => return fail(messages::OPEN_FAILED),
            };
            match pdf.reduce(options, Some(bytes.len())) {
                Ok((reduced, report)) => {
                    // Replaced in one step, so a crash never leaves a half-written file where the
                    // original was. Without the choice to replace files, one that appeared since
                    // the batch was planned is left alone.
                    match crate::fs::write_file(&target, &reduced, output.existing(input, &target)) {
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return fail(messages::FILE_EXISTS),
                        Err(_) => return fail(messages::SAVE_ERROR),
                    }
                    FileOutcome { input: input.clone(), output: Some(target), warning: None, report: Some(report) }
                }
                Err(Error::Other(message)) => fail(&message),
                Err(_) => fail(messages::SAVE_ERROR),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn images_are_decoded_within_the_budget_of_the_operation() {
        let doc = Document::with_version("1.5");
        let dict = dictionary! {
            "Subtype" => "Image", "Width" => 100, "Height" => 100, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
            "Filter" => "FlateDecode",
        };
        let image = Stream::new(dict, flate(&[0u8; 10_000]));
        // An image larger than what is left is not decoded, and takes nothing.
        let mut budget = 9_999;
        assert!(decode_image(&doc, &image, &mut budget).is_none());
        assert_eq!(budget, 9_999);
        let mut budget = 25_000;
        assert_eq!(decode_image(&doc, &image, &mut budget).unwrap().samples.len(), 10_000);
        assert_eq!(budget, 15_000);
        // Real images, which compress a few dozen times at most, always fit.
        assert!(image_budget(100 << 20) > crate::limits::MAX_DECODED + 3000 * (1 << 20));
    }

    #[test]
    fn slider_maps_like_the_original() {
        let at = |v| ReduceOptions { image_factor: v };
        assert!((at(50).f() - 0.5).abs() < 1e-6);
        assert!((at(50).k2() - 1.0).abs() < 1e-6, "default slider leaves the drawn size unchanged");
        assert!((at(0).f() - 0.05).abs() < 1e-6, "clamped to 0.05");
        assert!((at(100).f() - 0.95).abs() < 1e-6, "clamped to 0.95");
        assert!(at(0).k2() < at(50).k2() && at(50).k2() < at(100).k2());
        assert!(at(0).jpeg_quality() < at(100).jpeg_quality());
    }

    #[test]
    fn output_paths_follow_the_dialog_rules() {
        let dir = std::env::temp_dir();
        let input = dir.join("report.pdf");
        let mut options = OutputOptions::default();
        assert_eq!(options.output_path(&input).unwrap(), dir.join("report-reduced.pdf"), "the original is kept");
        assert_eq!(OutputOptions::in_place().output_path(&input).unwrap(), input);
        options.suffix.clear();
        options.overwrite = true;
        assert_eq!(options.output_path(&input), Err(messages::REPLACES_ORIGINAL), "replacing it is a choice of its own");

        options.prefix = "small-".into();
        options.suffix = "-v2".into();
        assert_eq!(options.output_path(&input).unwrap(), dir.join("small-report-v2.pdf"));

        options.folder = TargetFolder::Specific(PathBuf::new());
        assert_eq!(options.output_path(&input), Err(messages::CHOOSE_FOLDER));
        options.folder = TargetFolder::Specific(dir.join("definitely-not-a-folder-xyz"));
        assert_eq!(options.output_path(&input), Err(messages::INVALID_PATH));
    }

    #[test]
    fn matrices_multiply_in_pdf_order() {
        // Scale by 2 then translate by (10, 0): the point (1, 1) lands on (12, 2).
        let m = multiply([2.0, 0.0, 0.0, 2.0, 0.0, 0.0], [1.0, 0.0, 0.0, 1.0, 10.0, 0.0]);
        assert_eq!((m[0] * 1.0 + m[2] * 1.0 + m[4], m[1] * 1.0 + m[3] * 1.0 + m[5]), (12.0, 2.0));
    }
}
