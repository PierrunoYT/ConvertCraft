//! Conversion backends: images (pure Rust), structured data (pure Rust),
//! audio/video (delegated to an `ffmpeg` binary on PATH).

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use image::{
    DynamicImage, ImageDecoder, ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType,
    metadata::Orientation,
};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::output::OutputPlan;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Category {
    Image,
    Data,
    Audio,
    Video,
}

impl Category {
    pub const ALL: [Category; 4] = [
        Category::Image,
        Category::Data,
        Category::Audio,
        Category::Video,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Image => "Image",
            Category::Data => "Data",
            Category::Audio => "Audio",
            Category::Video => "Video",
        }
    }

    pub fn targets(self) -> &'static [&'static str] {
        match self {
            Category::Image => &[
                "png", "jpg", "webp", "bmp", "gif", "tiff", "ico", "tga", "qoi", "pnm", "hdr",
            ],
            Category::Data => &["json", "yaml", "toml", "csv"],
            Category::Audio => &["mp3", "wav", "flac", "ogg", "m4a", "opus"],
            Category::Video => &[
                "mp4", "mkv", "webm", "avi", "mov", "gif", "mp3", "wav", "m4a",
            ],
        }
    }

    pub fn needs_ffmpeg(self) -> bool {
        matches!(self, Category::Audio | Category::Video)
    }
}

pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn detect(path: &Path) -> Option<Category> {
    let cat = match extension(path).as_str() {
        "png" | "apng" | "jpg" | "jpeg" | "jfif" | "webp" | "bmp" | "gif" | "tif" | "tiff"
        | "ico" | "tga" | "qoi" | "pnm" | "pbm" | "pgm" | "ppm" | "pam" | "hdr" => Category::Image,
        "json" | "yaml" | "yml" | "toml" | "csv" => Category::Data,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" | "aiff" | "aif" => {
            Category::Audio
        }
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" | "ts" => {
            Category::Video
        }
        _ => return None,
    };
    Some(cat)
}

/// Pick a sensible default target: the first one that differs from the input format.
pub fn default_target(path: &Path, cat: Category) -> &'static str {
    let ext = extension(path);
    let ext = normalize_ext(&ext);
    cat.targets()
        .iter()
        .copied()
        .find(|t| *t != ext)
        .unwrap_or(cat.targets()[0])
}

fn normalize_ext(ext: &str) -> &str {
    match ext {
        "jpeg" | "jfif" => "jpg",
        "apng" => "png",
        "tif" => "tiff",
        "pbm" | "pgm" | "ppm" | "pam" => "pnm",
        "yml" => "yaml",
        other => other,
    }
}

#[derive(Clone)]
pub struct Options {
    pub jpeg_quality: u8,
    pub overwrite: bool,
    /// Prefix CSV cells that spreadsheets would evaluate as formulas.
    pub csv_formula_guard: bool,
    /// Shrink images to fit within `max_width`×`max_height`, keeping the aspect ratio.
    pub resize: bool,
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            jpeg_quality: 90,
            overwrite: false,
            csv_formula_guard: true,
            resize: false,
            max_width: 1920,
            max_height: 1080,
        }
    }
}

pub fn convert(
    input: &Path,
    output: OutputPlan,
    cat: Category,
    target: &str,
    opts: &Options,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    if !cat.targets().contains(&target) {
        bail!("unsupported {} target: {target}", cat.label());
    }
    // Fails if the source was replaced after planning.
    let source = output.open_source(input)?;
    let mut staged = output.begin()?;
    match cat {
        Category::Image => convert_image(input, source, staged.file_mut(), target, opts)?,
        Category::Data => convert_data(input, source, staged.file_mut(), target, opts)?,
        Category::Audio | Category::Video => {
            // FFmpeg reopens the path itself; the check above ran just before.
            drop(source);
            convert_media(input, staged.path(), target, cancel)?;
        }
    }
    staged.commit()
}

// ---------------------------------------------------------------- images

/// Largest image converted. Normalizing to 8-bit RGB(A) allocates up to 4 bytes per
/// pixel on top of the decoded buffer, which the `image` allocation limit does not cover.
const MAX_IMAGE_PIXELS: u64 = 100_000_000;
/// Estimated memory shared by all concurrent image jobs.
const IMAGE_MEMORY_BUDGET: u64 = 1536 * 1024 * 1024;
/// Decoders accepted after content sniffing; matches the extensions in `detect`.
const INPUT_IMAGE_FORMATS: [ImageFormat; 11] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::WebP,
    ImageFormat::Bmp,
    ImageFormat::Gif,
    ImageFormat::Tiff,
    ImageFormat::Ico,
    ImageFormat::Tga,
    ImageFormat::Qoi,
    ImageFormat::Pnm,
    ImageFormat::Hdr,
];

static IMAGE_MEMORY: MemoryBudget = MemoryBudget::new(IMAGE_MEMORY_BUDGET);

/// Counting semaphore over bytes, so concurrent workers share one memory budget.
struct MemoryBudget {
    used: Mutex<u64>,
    freed: Condvar,
    limit: u64,
}

impl MemoryBudget {
    const fn new(limit: u64) -> Self {
        Self {
            used: Mutex::new(0),
            freed: Condvar::new(),
            limit,
        }
    }

    /// Blocks until `bytes` fit. A request larger than the whole budget waits until it
    /// can run alone.
    fn reserve(&self, bytes: u64) -> Reservation<'_> {
        let bytes = bytes.min(self.limit);
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        while *used + bytes > self.limit {
            used = self
                .freed
                .wait(used)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *used += bytes;
        Reservation {
            budget: self,
            bytes,
        }
    }
}

struct Reservation<'a> {
    budget: &'a MemoryBudget,
    bytes: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *used -= self.bytes;
        self.budget.freed.notify_all();
    }
}

fn convert_image(
    input: &Path,
    source: File,
    output: &mut File,
    target: &str,
    opts: &Options,
) -> Result<()> {
    // Content wins over the extension (a PNG named .jpg still converts), but only
    // advertised formats are decoded.
    let mut reader = image::ImageReader::new(BufReader::new(source));
    // The extension is the fallback for formats without a signature, such as TGA.
    if let Ok(format) = ImageFormat::from_path(input) {
        reader.set_format(format);
    }
    let reader = reader.with_guessed_format()?;
    let format = reader.format().context("unrecognized image format")?;
    ensure!(
        INPUT_IMAGE_FORMATS.contains(&format),
        "unsupported image format: {format:?}"
    );
    let mut decoder = reader.into_decoder().context("cannot decode image")?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    ensure!(
        pixels <= MAX_IMAGE_PIXELS,
        "image is too large to convert ({width}×{height}; the limit is {} megapixels)",
        MAX_IMAGE_PIXELS / 1_000_000
    );
    // Cameras store rotation as EXIF metadata; bake it into the pixels, since outputs
    // carry no metadata. Unreadable EXIF is ignored rather than failing the conversion.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (shown_width, shown_height) = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (height, width),
        _ => (width, height),
    };
    let mut max = opts.resize.then_some((opts.max_width, opts.max_height));
    if target == "ico" {
        // ICO entries are limited to 256x256.
        let (w, h) = max.unwrap_or((256, 256));
        max = Some((w.min(256), h.min(256)));
    }
    let fitted = max.and_then(|(w, h)| fit_within(shown_width, shown_height, w, h));

    // Decoded buffer, the copy in the encoder's layout (up to 32-bit float RGBA for
    // HDR), a rotated copy, and the resize's f32 RGBA intermediate (source
    // width × new height) plus its result.
    let copy_bytes = match target {
        "hdr" => 16,
        _ => 4,
    };
    let mut memory = decoder.total_bytes().saturating_add(pixels * copy_bytes);
    if orientation != Orientation::NoTransforms {
        memory = memory.saturating_add(decoder.total_bytes());
    }
    if let Some((w, h)) = fitted {
        let (w, h) = (u64::from(w), u64::from(h));
        memory = memory.saturating_add(u64::from(shown_width) * h * 16 + w * h * 4);
    }
    let _memory = IMAGE_MEMORY.reserve(memory);
    let mut img = DynamicImage::from_decoder(decoder).context("cannot decode image")?;
    img.apply_orientation(orientation);
    if let Some((w, h)) = fitted {
        img = img.resize_exact(w, h, FilterType::Lanczos3);
    }

    let mut writer = BufWriter::new(output);
    match target {
        "jpg" => {
            encode_jpeg(&img, &mut writer, opts.jpeg_quality)?;
        }
        "png" => encoder_layout(img, target).write_to(&mut writer, ImageFormat::Png)?,
        "ico" => encode_ico(img, &mut writer)?,
        _ => {
            let format = match target {
                "webp" => ImageFormat::WebP,
                "bmp" => ImageFormat::Bmp,
                "gif" => ImageFormat::Gif,
                "tiff" => ImageFormat::Tiff,
                "tga" => ImageFormat::Tga,
                "qoi" => ImageFormat::Qoi,
                "pnm" => ImageFormat::Pnm,
                "hdr" => ImageFormat::Hdr,
                other => bail!("unsupported image target: {other}"),
            };
            encoder_layout(img, target).write_to(&mut writer, format)?;
        }
    }
    writer.flush().context("cannot flush image output")?;
    Ok(())
}

/// Standard icon sizes Windows picks from for lists, title bars, and the taskbar.
const ICO_SMALL_SIZES: [u32; 3] = [48, 32, 16];

/// Writes the image (already fitted within 256×256) plus every smaller standard size,
/// so Windows shows a downscaled icon instead of shrinking the large one itself.
fn encode_ico(img: DynamicImage, writer: &mut impl Write) -> Result<()> {
    use image::codecs::ico::{IcoEncoder, IcoFrame};
    let img = img.into_rgba8();
    let (width, height) = img.dimensions();
    let frame = |pixels: &image::RgbaImage| {
        let (w, h) = pixels.dimensions();
        IcoFrame::as_png(pixels.as_raw(), w, h, image::ExtendedColorType::Rgba8)
    };
    let mut frames = vec![frame(&img)?];
    for size in ICO_SMALL_SIZES {
        if let Some((w, h)) = fit_within(width, height, size, size) {
            frames.push(frame(&image::imageops::resize(
                &img,
                w,
                h,
                FilterType::Lanczos3,
            ))?);
        }
    }
    IcoEncoder::new(writer).encode_images(&frames)?;
    Ok(())
}

/// The size that fits `width`×`height` inside `max_width`×`max_height` keeping the
/// aspect ratio, or `None` when it already fits. Images are never enlarged.
fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    if width <= max_width && height <= max_height {
        return None;
    }
    let scale =
        (f64::from(max_width) / f64::from(width)).min(f64::from(max_height) / f64::from(height));
    let side = |n: u32| ((f64::from(n) * scale).round() as u32).max(1);
    Some((side(width), side(height)))
}

/// Converts to the closest pixel layout the target encoder accepts, keeping grayscale
/// and opaque images without an added alpha channel and 16-bit depth where supported.
/// `into_*` reuses the buffer when the image already has that layout.
fn encoder_layout(img: DynamicImage, target: &str) -> DynamicImage {
    let color = img.color();
    let (gray, alpha) = (!color.has_color(), color.has_alpha());
    let bytes_per_sample = color.bytes_per_pixel() / color.channel_count();
    let deep = bytes_per_sample > 1;
    match target {
        // HDR stores float RGB samples without alpha.
        "hdr" => DynamicImage::ImageRgb32F(img.into_rgb32f()),
        // PNG keeps 8- and 16-bit layouts as-is; float (HDR) becomes 16-bit.
        "png" if bytes_per_sample == 4 && alpha => DynamicImage::ImageRgba16(img.into_rgba16()),
        "png" if bytes_per_sample == 4 => DynamicImage::ImageRgb16(img.into_rgb16()),
        "png" => img,
        // TIFF and PNM (written as PAM) store 16-bit samples but have no 16-bit
        // gray+alpha layout in `image`.
        "tiff" | "pnm" if deep => match (gray, alpha) {
            (true, false) => DynamicImage::ImageLuma16(img.into_luma16()),
            (false, false) => DynamicImage::ImageRgb16(img.into_rgb16()),
            (_, true) => DynamicImage::ImageRgba16(img.into_rgba16()),
        },
        "tiff" if gray && alpha => DynamicImage::ImageRgba8(img.into_rgba8()),
        // GIF quantizes from RGBA.
        "gif" => DynamicImage::ImageRgba8(img.into_rgba8()),
        // QOI has only RGB and RGBA.
        "qoi" if alpha => DynamicImage::ImageRgba8(img.into_rgba8()),
        "qoi" => DynamicImage::ImageRgb8(img.into_rgb8()),
        // BMP, TGA, WebP, PNM and 8-bit TIFF accept every 8-bit layout.
        _ => match (gray, alpha) {
            (true, false) => DynamicImage::ImageLuma8(img.into_luma8()),
            (true, true) => DynamicImage::ImageLumaA8(img.into_luma_alpha8()),
            (false, false) => DynamicImage::ImageRgb8(img.into_rgb8()),
            (false, true) => DynamicImage::ImageRgba8(img.into_rgba8()),
        },
    }
}

fn encode_jpeg(image: &DynamicImage, writer: &mut impl Write, quality: u8) -> Result<()> {
    {
        let mut encoder = JpegEncoder::new_with_quality(&mut *writer, quality);
        encoder.encode_image(&DynamicImage::ImageRgb8(flatten_on_white(image)))?;
    }
    writer.flush().context("cannot flush JPEG output")?;
    Ok(())
}

/// JPEG has no alpha. Dropping it would expose whatever color sits under transparent
/// pixels (often black), so composite over white like image viewers and browsers do.
fn flatten_on_white(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    image::RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let [r, g, b, a] = image::GenericImageView::get_pixel(image, x, y).0;
        let a = u16::from(a);
        let over_white = |c: u8| ((u16::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        image::Rgb([over_white(r), over_white(g), over_white(b)])
    })
}

// ---------------------------------------------------------------- data

/// Largest data file read.
const MAX_DATA_INPUT_BYTES: u64 = 64 * 1024 * 1024;
/// Estimated in-memory size of the parsed document. YAML aliases and repeated CSV
/// headers can expand a small file far beyond its own size.
const MAX_DATA_VALUE_BYTES: u64 = 768 * 1024 * 1024;
/// Largest converted data file written.
const MAX_DATA_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
/// Excel's column limit; also bounds the rows × headers cell expansion of sparse rows.
const MAX_CSV_COLUMNS: usize = 16_384;
/// Rough per-node cost of a `serde_json::Value`, charged on top of string bytes.
const VALUE_NODE_BYTES: u64 = 64;

fn convert_data(
    input: &Path,
    source: File,
    output: &mut File,
    target: &str,
    opts: &Options,
) -> Result<()> {
    let mut text = String::new();
    source
        .take(MAX_DATA_INPUT_BYTES + 1)
        .read_to_string(&mut text)
        .context("cannot read file")?;
    ensure!(
        text.len() as u64 <= MAX_DATA_INPUT_BYTES,
        "data file is too large (the limit is {} MiB)",
        MAX_DATA_INPUT_BYTES >> 20
    );
    let budget = ValueBudget::new(MAX_DATA_VALUE_BYTES);
    let value: Value = match normalize_ext(&extension(input)) {
        "json" => {
            let mut de = serde_json::Deserializer::from_str(&text);
            let value = budget.seed().deserialize(&mut de).context("invalid JSON")?;
            de.end().context("invalid JSON")?;
            value
        }
        "yaml" => {
            serde_saphyr::with_deserializer_from_str_with_options(&text, yaml_options(), |de| {
                budget.seed().deserialize(de)
            })
            .context("invalid YAML")?
        }
        "toml" => budget
            .seed()
            .deserialize(toml::Deserializer::new(&text))
            .context("invalid TOML")?,
        "csv" => read_csv(&text, &budget)?,
        other => bail!("unsupported data source: {other}"),
    };

    // Stream into the staged file, refusing to write past the output limit.
    let mut writer = LimitedWriter {
        inner: BufWriter::new(output),
        remaining: MAX_DATA_OUTPUT_BYTES,
    };
    match target {
        "json" => {
            serde_json::to_writer_pretty(&mut writer, &value)?;
            writer.write_all(b"\n")?;
        }
        "yaml" => serde_saphyr::to_io_writer(&mut writer, &value)?,
        "toml" => {
            // TOML documents must be a table at the top level.
            let value = match value {
                Value::Object(_) => value,
                other => Value::Object(Map::from_iter([("items".to_string(), other)])),
            };
            let text = toml::to_string_pretty(&value)
                .context("data cannot be represented as TOML (nulls are not allowed)")?;
            writer.write_all(text.as_bytes())?;
        }
        "csv" => write_csv(&value, &mut writer, opts.csv_formula_guard)?,
        other => bail!("unsupported data target: {other}"),
    }
    writer.flush().context("cannot flush data output")?;
    Ok(())
}

/// The parser's own defaults are sized for configuration files. Data files up to
/// `MAX_DATA_INPUT_BYTES` may hold millions of nodes, and alias expansion is already
/// charged to the `ValueBudget`, so only the structural limits are kept.
fn yaml_options() -> serde_saphyr::Options {
    let nodes = (MAX_DATA_VALUE_BYTES / VALUE_NODE_BYTES) as usize;
    let mut options = serde_saphyr::Options::default();
    let mut budget = serde_saphyr::Budget::default();
    budget.max_events = 2 * nodes;
    budget.max_nodes = nodes;
    budget.max_aliases = nodes;
    budget.max_anchors = nodes;
    // Same nesting limit as serde_json.
    budget.max_depth = 128;
    // Many aliases of one anchor (`<<: *defaults`) are ordinary YAML.
    budget.enforce_alias_anchor_ratio = false;
    options.budget = Some(budget);
    options.alias_limits.max_total_replayed_events = 2 * nodes;
    options.alias_limits.max_replay_stack_depth = 128;
    // Row errors are tooltips; keep them to the message and location.
    options.with_snippet = false;
    options
}

/// Estimated memory charged while a document is built.
struct ValueBudget {
    remaining: Cell<u64>,
}

impl ValueBudget {
    fn new(limit: u64) -> Self {
        Self {
            remaining: Cell::new(limit),
        }
    }

    fn charge(&self, bytes: usize) -> std::result::Result<(), String> {
        let cost = VALUE_NODE_BYTES.saturating_add(bytes as u64);
        match self.remaining.get().checked_sub(cost) {
            Some(rest) => {
                self.remaining.set(rest);
                Ok(())
            }
            None => Err(format!(
                "document expands beyond the {} MiB memory limit",
                MAX_DATA_VALUE_BYTES >> 20
            )),
        }
    }

    fn seed(&self) -> ValueSeed<'_> {
        ValueSeed { budget: self }
    }
}

/// Builds a `serde_json::Value` like its own `Deserialize` impl, but charges every
/// node against a budget (so YAML aliases cannot expand without bound) and rejects
/// duplicate keys instead of keeping the last one.
#[derive(Clone, Copy)]
struct ValueSeed<'a> {
    budget: &'a ValueBudget,
}

impl ValueSeed<'_> {
    fn charge<E: de::Error>(self, bytes: usize) -> std::result::Result<(), E> {
        self.budget.charge(bytes).map_err(E::custom)
    }
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ValueSeed<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON-compatible value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        self.charge(0)?;
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        self.charge(0)?;
        Ok(Value::from(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        self.charge(0)?;
        Ok(Value::from(v))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        self.charge(0)?;
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        self.charge(v.len())?;
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.charge(v.len())?;
        Ok(Value::String(v))
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        self.charge(0)?;
        Ok(Value::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        self.charge(0)?;
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        self.deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        self.charge(0)?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        self.charge(0)?;
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            self.charge(key.len())?;
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let value = map.next_value_seed(self)?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

fn read_csv(text: &str, budget: &ValueBudget) -> Result<Value> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers()?.clone();
    ensure!(
        headers.len() <= MAX_CSV_COLUMNS,
        "CSV has more than {MAX_CSV_COLUMNS} columns"
    );
    let mut seen = HashSet::new();
    for header in &headers {
        ensure!(seen.insert(header), "duplicate CSV header {header:?}");
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record?;
        let mut row = Map::new();
        for (header, field) in headers.iter().zip(record.iter()) {
            // Every row stores its own copy of each header name.
            budget
                .charge(header.len() + field.len())
                .map_err(|message| anyhow!(message))?;
            row.insert(header.to_string(), infer_scalar(field));
        }
        rows.push(Value::Object(row));
    }
    Ok(Value::Array(rows))
}

fn infer_scalar(s: &str) -> Value {
    if s.is_empty() {
        return Value::Null;
    }
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    let integer_like = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    // Identifiers such as "00042" keep their leading zeros.
    let leading_zero =
        digits.len() > 1 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit();
    if !leading_zero {
        if let Ok(i) = s.parse::<i64>() {
            return Value::from(i);
        }
        if let Ok(u) = s.parse::<u64>() {
            return Value::from(u);
        }
        // Integers beyond 64 bits stay exact as text rather than rounding through f64.
        if !integer_like
            && let Ok(f) = s.parse::<f64>()
            && f.is_finite()
        {
            return Value::from(f);
        }
    }
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(s.to_string()),
    }
}

/// Prefix text that a spreadsheet would evaluate as a formula (OWASP CSV injection list).
fn csv_text(s: &str, formula_guard: bool) -> Cow<'_, str> {
    if formula_guard && s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        Cow::Owned(format!("'{s}"))
    } else {
        Cow::Borrowed(s)
    }
}

fn write_csv(value: &Value, output: impl Write, formula_guard: bool) -> Result<()> {
    let rows: Vec<&Map<String, Value>> = match value {
        Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_object()
                    .ok_or_else(|| anyhow!("CSV needs an array of objects"))
            })
            .collect::<Result<_>>()?,
        Value::Object(map) => vec![map],
        _ => bail!("CSV needs an array of objects"),
    };

    // Union of all keys, in first-seen order.
    let mut headers: Vec<&str> = Vec::new();
    let mut seen = HashSet::new();
    for row in &rows {
        for key in row.keys() {
            if seen.insert(key.as_str()) {
                ensure!(
                    headers.len() < MAX_CSV_COLUMNS,
                    "CSV would have more than {MAX_CSV_COLUMNS} columns"
                );
                headers.push(key);
            }
        }
    }

    let mut writer = csv::Writer::from_writer(output);
    writer.write_record(
        headers
            .iter()
            .map(|h| csv_text(h, formula_guard).into_owned()),
    )?;
    for row in rows {
        let record = headers.iter().map(|h| match row.get(*h) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => csv_text(s, formula_guard).into_owned(),
            Some(other) => other.to_string(),
        });
        writer.write_record(record)?;
    }
    writer.flush()?;
    Ok(())
}

/// Fails once more than `remaining` bytes would be written.
struct LimitedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(std::io::Error::other(format!(
                "converted output would exceed the {} MiB limit",
                MAX_DATA_OUTPUT_BYTES >> 20
            )));
        }
        let written = self.inner.write(buf)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------- audio / video

/// Demuxers for the advertised audio/video inputs. Playlist and concat demuxers (HLS,
/// ffconcat) that could pull other local files into the output are not on the list.
const MEDIA_DEMUXERS: &str = "aac,aiff,asf,avi,flac,flv,matroska,mov,mp3,mpeg,mpegts,ogg,wav";
/// FFmpeg reports progress about twice a second; a job silent this long is stuck.
const FFMPEG_STALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Only the end of FFmpeg's error output is shown, so only the end is kept.
const MAX_FFMPEG_STDERR: usize = 64 * 1024;

static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Locate and check FFmpeg once per process (blocking on first call).
pub fn ffmpeg_path() -> Option<&'static Path> {
    FFMPEG.get_or_init(locate_ffmpeg).as_deref()
}

/// `None` while the startup check is still running.
pub fn ffmpeg_status() -> Option<Option<&'static Path>> {
    FFMPEG.get().map(Option::as_deref)
}

/// Homebrew's bin directories (Apple Silicon, then Intel), searched after PATH. An app
/// opened from Finder or the Dock gets launchd's minimal PATH, which has neither.
#[cfg(target_os = "macos")]
const FALLBACK_FFMPEG_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];
#[cfg(not(target_os = "macos"))]
const FALLBACK_FFMPEG_DIRS: &[&str] = &[];

/// Candidate executables from absolute PATH entries, then the fallback directories.
/// Unlike `Command::new`, this never picks up an `ffmpeg` that sits next to the
/// application or in the working directory.
fn ffmpeg_candidates(path_var: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .chain(FALLBACK_FFMPEG_DIRS.iter().map(PathBuf::from))
        .map(move |dir| dir.join(name))
}

fn locate_ffmpeg() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let ffmpeg = ffmpeg_candidates(&path_var).find(|candidate| candidate.is_file())?;
    let works = ffmpeg_command(&ffmpeg)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    works.then_some(ffmpeg)
}

/// Stop FFmpeg and anything it started. Package managers such as Chocolatey put a
/// shim `ffmpeg.exe` on PATH that runs the real one as a child; killing only the shim
/// would leave the real encoder running and holding the staged output open.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let windows = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        // Must run while the parent is alive: /T finds children through it.
        let _ = Command::new(windows.join("System32").join("taskkill.exe"))
            .args(["/T", "/F", "/PID"])
            .arg(child.id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn ffmpeg_command(ffmpeg: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(ffmpeg);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
fn ffmpeg_command(ffmpeg: &Path) -> Command {
    Command::new(ffmpeg)
}

fn ffmpeg_args(target: &str) -> &'static [&'static str] {
    match target {
        "mp3" => &["-vn", "-c:a", "libmp3lame", "-q:a", "2"],
        "wav" => &["-vn"],
        "flac" => &["-vn", "-c:a", "flac"],
        "ogg" => &["-vn", "-c:a", "libvorbis", "-q:a", "5"],
        "m4a" => &["-vn", "-c:a", "aac", "-b:a", "192k"],
        "opus" => &["-vn", "-c:a", "libopus", "-b:a", "128k"],
        "mp4" | "mov" => &[
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "160k",
            "-movflags",
            "+faststart",
        ],
        "mkv" => &[
            "-c:v", "libx264", "-crf", "23", "-c:a", "aac", "-b:a", "160k",
        ],
        "webm" => &[
            "-c:v",
            "libvpx-vp9",
            "-crf",
            "32",
            "-b:v",
            "0",
            "-c:a",
            "libopus",
        ],
        "avi" => &[
            "-c:v",
            "mpeg4",
            "-q:v",
            "5",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "4",
        ],
        "gif" => &[
            "-an",
            "-vf",
            "fps=12,scale='min(480,iw)':-2:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse",
            "-loop",
            "0",
        ],
        _ => &[],
    }
}

fn convert_media(input: &Path, output: &Path, target: &str, cancel: &AtomicBool) -> Result<()> {
    let ffmpeg = ffmpeg_path()
        .context("ffmpeg was not found on PATH — install it to convert audio/video")?;
    let mut child = ffmpeg_command(ffmpeg)
        .args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
        .args(["-progress", "pipe:1"])
        .args([
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            MEDIA_DEMUXERS,
        ])
        .arg("-i")
        .arg(input)
        .args(ffmpeg_args(target))
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run ffmpeg")?;

    // Progress lines on stdout show the job is alive; stderr keeps only its tail.
    let started = Instant::now();
    let last_progress = Arc::new(AtomicU64::new(0));
    let stdout = child.stdout.take().context("ffmpeg stdout")?;
    let progress = {
        let last_progress = Arc::clone(&last_progress);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.is_err() {
                    break;
                }
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                last_progress.store(elapsed, Ordering::Relaxed);
            }
        })
    };
    let mut stderr = child.stderr.take().context("ffmpeg stderr")?;
    let errors = thread::spawn(move || {
        let mut tail = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(read) = stderr.read(&mut chunk) {
            if read == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..read]);
            if tail.len() > MAX_FFMPEG_STDERR {
                tail.drain(..tail.len() - MAX_FFMPEG_STDERR);
            }
        }
        tail
    });

    let stopped = loop {
        if let Some(status) = child.try_wait().context("failed to wait for ffmpeg")? {
            break Ok(status);
        }
        if cancel.load(Ordering::Relaxed) {
            break Err(anyhow!("cancelled"));
        }
        let since_progress = started
            .elapsed()
            .saturating_sub(Duration::from_millis(last_progress.load(Ordering::Relaxed)));
        if since_progress > FFMPEG_STALL_TIMEOUT {
            break Err(anyhow!(
                "ffmpeg made no progress for {} minutes",
                FFMPEG_STALL_TIMEOUT.as_secs() / 60
            ));
        }
        thread::sleep(Duration::from_millis(100));
    };
    let status = match stopped {
        Ok(status) => status,
        Err(error) => {
            kill_tree(&mut child);
            return Err(error);
        }
    };
    let _ = progress.join();
    let tail = errors.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&tail);
        let msg = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("unknown error");
        bail!("ffmpeg: {msg}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputPlanner;

    /// Skipping is fine locally; CI sets `var` so a missing dependency fails the test.
    fn require_unset(var: &str, reason: &str) {
        assert!(std::env::var_os(var).is_none(), "{var} is set but {reason}");
    }

    fn convert_to(
        input: &Path,
        directory: &Path,
        cat: Category,
        target: &str,
        opts: &Options,
    ) -> Result<PathBuf> {
        let (mut planner, errors) = OutputPlanner::new([input]);
        if let Some((_, error)) = errors.into_iter().next() {
            return Err(error);
        }
        let plan = planner.plan(input, Some(directory), target, opts.overwrite)?;
        convert(input, plan, cat, target, opts, &AtomicBool::new(false))
    }

    #[test]
    fn image_to_every_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.png");
        image::RgbaImage::from_fn(300, 200, |x, y| image::Rgba([x as u8, y as u8, 128, 200]))
            .save(&src)
            .unwrap();
        for &t in Category::Image.targets() {
            let out = convert_to(&src, dir.path(), Category::Image, t, &Options::default())
                .unwrap_or_else(|e| panic!("{t}: {e:#}"));
            image::open(&out).unwrap_or_else(|e| panic!("reopen {t}: {e}"));
        }
    }

    #[test]
    fn outputs_keep_channels_and_tiff_keeps_16_bit() {
        use image::ColorType;
        let dir = tempfile::tempdir().unwrap();
        let gray16 = image::ImageBuffer::<image::Luma<u16>, _>::from_fn(8, 8, |x, y| {
            image::Luma([(x * 8000 + y) as u16])
        });
        let sources = [
            ("gray16.png", DynamicImage::ImageLuma16(gray16)),
            (
                "rgb.png",
                DynamicImage::ImageRgb8(image::RgbImage::new(8, 8)),
            ),
            (
                "gray_alpha.png",
                DynamicImage::ImageLumaA8(image::GrayAlphaImage::new(8, 8)),
            ),
        ];
        for (name, img) in sources {
            let src = dir.path().join(name);
            img.save(&src).unwrap();
            for &t in Category::Image.targets() {
                let out = convert_to(&src, dir.path(), Category::Image, t, &Options::default())
                    .unwrap_or_else(|e| panic!("{name} -> {t}: {e:#}"));
                let color = image::open(&out)
                    .unwrap_or_else(|e| panic!("reopen {name} -> {t}: {e}"))
                    .color();
                let expected = match (name, t) {
                    ("gray16.png", "tiff" | "pnm") => Some(ColorType::L16),
                    ("rgb.png", "bmp" | "tiff" | "qoi" | "tga" | "webp" | "pnm") => {
                        Some(ColorType::Rgb8)
                    }
                    ("gray_alpha.png", "tga" | "pnm") => Some(ColorType::La8),
                    _ => None,
                };
                if let Some(expected) = expected {
                    assert_eq!(color, expected, "{name} -> {t}");
                }
            }
        }
    }

    #[test]
    fn jpeg_puts_transparency_on_white() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("logo.png");
        // Left half fully transparent black, right half opaque red.
        image::RgbaImage::from_fn(16, 16, |x, _| {
            if x < 8 {
                image::Rgba([0, 0, 0, 0])
            } else {
                image::Rgba([255, 0, 0, 255])
            }
        })
        .save(&src)
        .unwrap();
        let out = convert_to(
            &src,
            dir.path(),
            Category::Image,
            "jpg",
            &Options::default(),
        )
        .unwrap();
        let out = image::open(out).unwrap().into_rgb8();
        let near = |p: image::Rgb<u8>, want: [u8; 3]| {
            p.0.iter().zip(want).all(|(&a, b)| a.abs_diff(b) < 16)
        };
        assert!(
            near(*out.get_pixel(2, 8), [255, 255, 255]),
            "{:?}",
            out.get_pixel(2, 8)
        );
        assert!(
            near(*out.get_pixel(13, 8), [255, 0, 0]),
            "{:?}",
            out.get_pixel(13, 8)
        );
    }

    /// Writes a `width`×`height` PNG whose EXIF orientation says "rotate 90° clockwise".
    fn rotated_png(path: &Path, width: u32, height: u32) {
        use image::ImageEncoder;
        // Big-endian TIFF header + one IFD entry: Orientation (0x0112) = 6, rotate 90° CW.
        let exif = [
            b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0,
            0, 0, 0,
        ];
        let pixels = image::RgbImage::from_fn(width, height, |x, _| image::Rgb([x as u8, 0, 0]));
        let mut encoder = image::codecs::png::PngEncoder::new(File::create(path).unwrap());
        encoder.set_exif_metadata(exif.to_vec()).unwrap();
        encoder
            .write_image(
                pixels.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
    }

    #[test]
    fn exif_orientation_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("rotated.png");
        rotated_png(&src, 4, 2);
        let out = convert_to(
            &src,
            dir.path(),
            Category::Image,
            "bmp",
            &Options::default(),
        )
        .unwrap();
        let out = image::open(out).unwrap();
        assert_eq!((out.width(), out.height()), (2, 4));
    }

    #[test]
    fn ico_contains_standard_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let sizes = |name: &str, width: u32, height: u32| {
            let src = dir.path().join(name);
            image::RgbaImage::new(width, height).save(&src).unwrap();
            let out = convert_to(
                &src,
                dir.path(),
                Category::Image,
                "ico",
                &Options::default(),
            )
            .unwrap();
            let ico = std::fs::read(out).unwrap();
            let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
            // ICONDIRENTRY width/height bytes; 0 means 256.
            let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
            (0..count)
                .map(|i| (side(ico[6 + i * 16]), side(ico[7 + i * 16])))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sizes("big.png", 512, 512),
            [(256, 256), (48, 48), (32, 32), (16, 16)]
        );
        assert_eq!(
            sizes("wide.png", 600, 300),
            [(256, 128), (48, 24), (32, 16), (16, 8)]
        );
        assert_eq!(sizes("small.png", 40, 40), [(40, 40), (32, 32), (16, 16)]);
    }

    #[test]
    fn resize_fits_within_bounds_and_never_enlarges() {
        let dir = tempfile::tempdir().unwrap();
        let opts = Options {
            resize: true,
            max_width: 100,
            max_height: 100,
            ..Options::default()
        };
        let wide = dir.path().join("wide.png");
        image::RgbImage::new(400, 200).save(&wide).unwrap();
        let small = dir.path().join("small.png");
        image::RgbImage::new(50, 30).save(&small).unwrap();
        // Stored 400×200, displayed 200×400: the bounds apply to the upright image.
        let rotated = dir.path().join("rotated.png");
        rotated_png(&rotated, 400, 200);
        for (src, target, expected) in [
            (&wide, "jpg", (100, 50)),
            (&wide, "png", (100, 50)),
            (&small, "bmp", (50, 30)),
            (&rotated, "png", (50, 100)),
        ] {
            let out = convert_to(src, dir.path(), Category::Image, target, &opts).unwrap();
            let out = image::open(out).unwrap();
            assert_eq!((out.width(), out.height()), expected, "{src:?} -> {target}");
        }
    }

    /// A BMP header declaring `width`x`height` pixels, with no pixel data.
    fn bmp_header(width: i32, height: i32) -> Vec<u8> {
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&54u32.to_le_bytes()); // file size (header only)
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        bmp.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        bmp.extend_from_slice(&width.to_le_bytes());
        bmp.extend_from_slice(&height.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        bmp.extend_from_slice(&[0; 24]); // no compression, sizes, resolution, palette
        bmp
    }

    #[test]
    fn oversized_images_are_rejected_before_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("huge.bmp");
        std::fs::write(&src, bmp_header(20_000, 20_000)).unwrap();
        let error = convert_to(
            &src,
            dir.path(),
            Category::Image,
            "png",
            &Options::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("too large"), "{error:#}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn undeclared_image_formats_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("photo.png");
        // OpenEXR magic number followed by junk.
        std::fs::write(&src, [0x76, 0x2f, 0x31, 0x01, 2, 0, 0, 0, 0, 0]).unwrap();
        let error = convert_to(
            &src,
            dir.path(),
            Category::Image,
            "bmp",
            &Options::default(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("unsupported image format: OpenExr"),
            "{error:#}"
        );
    }

    #[test]
    fn sniffed_supported_format_still_converts() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("actually-png.jpg");
        image::RgbImage::new(4, 4)
            .save_with_format(&src, ImageFormat::Png)
            .unwrap();
        convert_to(
            &src,
            dir.path(),
            Category::Image,
            "bmp",
            &Options::default(),
        )
        .unwrap();
    }

    #[test]
    fn memory_budget_blocks_until_released() {
        use std::sync::mpsc;
        use std::time::Duration;
        static BUDGET: MemoryBudget = MemoryBudget::new(100);
        let first = BUDGET.reserve(60);
        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _second = BUDGET.reserve(60);
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(first);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
        // Oversized requests are clamped and run alone instead of deadlocking.
        drop(BUDGET.reserve(1_000));
    }

    fn convert_text(name: &str, contents: &str, target: &str, opts: &Options) -> Result<String> {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join(name);
        std::fs::write(&src, contents).unwrap();
        let out = convert_to(&src, dir.path(), Category::Data, target, opts)?;
        Ok(std::fs::read_to_string(out).unwrap())
    }

    #[test]
    fn yaml_round_trip_keeps_ambiguous_scalars() {
        let original = serde_json::json!({
            "strings": ["yes", "no", "null", "~", "true", "0x1F", "010", "1e3", ".inf",
                "", " padded ", "a: b", "- item", "#hash", "multi\nline", "*alias", "&anchor"],
            "numbers": [0, -7, 18446744073709551615u64, 0.5, 1e300],
            "nested": {"empty_list": [], "empty_map": {}, "null": null, "bool": false},
        });
        let opts = Options::default();
        let yaml = convert_text("in.json", &original.to_string(), "yaml", &opts).unwrap();
        let json = convert_text("in.yaml", &yaml, "json", &opts).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&json).unwrap(),
            original,
            "{yaml}"
        );
    }

    #[test]
    fn yaml_alias_expansion_is_budgeted() {
        let mut yaml = format!("anchor: &s {}\nrefs: [", "x".repeat(4096));
        yaml.push_str(&vec!["*s"; 1000].join(","));
        yaml.push_str("]\n");
        let parse = |budget: ValueBudget| {
            serde_saphyr::with_deserializer_from_str_with_options(&yaml, yaml_options(), |de| {
                budget.seed().deserialize(de)
            })
        };
        let error = parse(ValueBudget::new(1024 * 1024)).unwrap_err();
        assert!(error.to_string().contains("memory limit"), "{error}");
        // The same document fits a budget large enough for its ~4 MB expansion.
        let value = parse(ValueBudget::new(8 * 1024 * 1024)).unwrap();
        assert_eq!(value["refs"].as_array().unwrap().len(), 1000);
    }

    #[test]
    fn duplicate_keys_and_headers_are_rejected() {
        let opts = Options::default();
        for (name, contents) in [
            ("dup.json", r#"{"a": 1, "a": 2}"#),
            ("dup.yaml", "a: 1\na: 2\n"),
            ("dup.csv", "x,x\nfirst,second\n"),
        ] {
            let error = convert_text(name, contents, "json", &opts).unwrap_err();
            assert!(
                format!("{error:#}").contains("duplicate"),
                "{name}: {error:#}"
            );
        }
    }

    #[test]
    fn csv_scalars_keep_exact_integers_and_identifiers() {
        let csv = "v\n00042\n-007\n0\n-7\n+5\n18446744073709551615\n18446744073709551616\n0.5\n1e3\n1e400\ntrue\n";
        let json = convert_text("in.csv", csv, "json", &Options::default()).unwrap();
        let rows: Value = serde_json::from_str(&json).unwrap();
        let values: Vec<&Value> = rows.as_array().unwrap().iter().map(|r| &r["v"]).collect();
        assert_eq!(
            values,
            [
                &Value::from("00042"),
                &Value::from("-007"),
                &Value::from(0),
                &Value::from(-7),
                &Value::from(5),
                &Value::from(u64::MAX),
                &Value::from("18446744073709551616"),
                &Value::from(0.5),
                &Value::from(1000.0),
                &Value::from("1e400"),
                &Value::from(true),
            ]
        );
    }

    #[test]
    fn csv_formulas_are_neutralized_unless_disabled() {
        let json = r#"[{"=1+1": "=HYPERLINK(\"http://example.invalid\")", "n": "@SUM(1,2)", "ok": "plain", "num": -5}]"#;
        let safe = convert_text("in.json", json, "csv", &Options::default()).unwrap();
        assert_eq!(
            safe,
            "'=1+1,n,num,ok\n\"'=HYPERLINK(\"\"http://example.invalid\"\")\",\"'@SUM(1,2)\",-5,plain\n"
        );
        let raw_opts = Options {
            csv_formula_guard: false,
            ..Options::default()
        };
        let raw = convert_text("in.json", json, "csv", &raw_opts).unwrap();
        assert!(raw.starts_with("=1+1,"), "{raw}");
    }

    #[test]
    fn sparse_csv_columns_are_capped() {
        let rows: Vec<String> = (0..=MAX_CSV_COLUMNS)
            .map(|i| format!(r#"{{"k{i}":0}}"#))
            .collect();
        let json = format!("[{}]", rows.join(","));
        let error = convert_text("sparse.json", &json, "csv", &Options::default()).unwrap_err();
        assert!(format!("{error:#}").contains("columns"), "{error:#}");
    }

    #[test]
    fn data_input_and_output_sizes_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.json");
        let file = File::create(&src).unwrap();
        file.set_len(MAX_DATA_INPUT_BYTES + 1).unwrap();
        drop(file);
        let error = convert_to(
            &src,
            dir.path(),
            Category::Data,
            "yaml",
            &Options::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("too large"), "{error:#}");

        let mut writer = LimitedWriter {
            inner: Vec::new(),
            remaining: 4,
        };
        writer.write_all(b"1234").unwrap();
        let error = writer.write_all(b"5").unwrap_err();
        assert!(error.to_string().contains("limit"), "{error}");
        assert_eq!(writer.inner, b"1234");
    }

    #[test]
    fn data_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.json");
        std::fs::write(
            &src,
            r#"[{"name":"a","n":1,"ok":true},{"name":"b","n":2.5,"ok":false}]"#,
        )
        .unwrap();
        for &t in Category::Data.targets() {
            let out = convert_to(
                &src,
                &dir.path().join(t),
                Category::Data,
                t,
                &Options::default(),
            )
            .unwrap_or_else(|e| panic!("{t}: {e:#}"));
            // Convert back to JSON and compare.
            let back = convert_to(
                &out,
                &dir.path().join(format!("back-{t}")),
                Category::Data,
                "json",
                &Options::default(),
            )
            .unwrap();
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&back).unwrap()).unwrap();
            let rows = if t == "toml" { v["items"].clone() } else { v };
            assert_eq!(rows[1]["n"], 2.5, "{t}");
            assert_eq!(rows[0]["ok"], true, "{t}");
        }
    }

    #[test]
    fn failed_jpeg_conversion_preserves_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("wide.png");
        let output = dir.path().join("wide.jpg");
        image::RgbImage::new(65_536, 1).save(&input).unwrap();
        std::fs::write(&output, b"existing image").unwrap();
        let opts = Options {
            overwrite: true,
            ..Options::default()
        };
        assert!(convert_to(&input, dir.path(), Category::Image, "jpg", &opts).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"existing image");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn jpeg_flush_error_is_returned() {
        struct FailedDisk;
        impl Write for FailedDisk {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("simulated disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = BufWriter::new(FailedDisk);
        let error = encode_jpeg(&DynamicImage::new_rgb8(1, 1), &mut writer, 90).unwrap_err();
        assert!(format!("{error:#}").contains("simulated disk full"));
    }

    #[test]
    fn failed_data_conversion_preserves_existing_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("invalid.json");
        let output = dir.path().join("invalid.yaml");
        std::fs::write(&input, b"{").unwrap();
        std::fs::write(&output, b"previous data").unwrap();
        let opts = Options {
            overwrite: true,
            ..Options::default()
        };
        assert!(convert_to(&input, dir.path(), Category::Data, "yaml", &opts).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"previous data");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn successful_conversion_replaces_only_the_planned_destination() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("data.json");
        let output = dir.path().join("data.yaml");
        std::fs::write(&input, r#"{"updated":true}"#).unwrap();
        std::fs::write(&output, b"previous data").unwrap();
        let opts = Options {
            overwrite: true,
            ..Options::default()
        };
        convert_to(&input, dir.path(), Category::Data, "yaml", &opts).unwrap();
        assert_eq!(std::fs::read_to_string(output).unwrap(), "updated: true\n");
    }

    #[test]
    fn cross_format_batch_preserves_both_sources() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("batch.json");
        let yaml = dir.path().join("batch.yaml");
        std::fs::write(&json, r#"{"origin":"A"}"#).unwrap();
        std::fs::write(&yaml, "origin: B\n").unwrap();
        let (mut planner, errors) = OutputPlanner::new([json.as_path(), yaml.as_path()]);
        assert!(errors.is_empty());
        let json_plan = planner.plan(&json, None, "yaml", true).unwrap();
        let yaml_plan = planner.plan(&yaml, None, "json", true).unwrap();
        let opts = Options {
            overwrite: true,
            ..Options::default()
        };
        let no_cancel = AtomicBool::new(false);
        let a = convert(&json, json_plan, Category::Data, "yaml", &opts, &no_cancel).unwrap();
        let b = convert(&yaml, yaml_plan, Category::Data, "json", &opts, &no_cancel).unwrap();
        assert_eq!(std::fs::read_to_string(json).unwrap(), r#"{"origin":"A"}"#);
        assert_eq!(std::fs::read_to_string(yaml).unwrap(), "origin: B\n");
        assert_eq!(std::fs::read_to_string(a).unwrap(), "origin: A\n");
        let converted: Value = serde_json::from_str(&std::fs::read_to_string(b).unwrap()).unwrap();
        assert_eq!(converted, serde_json::json!({"origin": "B"}));
    }

    #[test]
    fn failed_ffmpeg_conversion_preserves_existing_output() {
        if ffmpeg_path().is_none() {
            require_unset("CONVERTCRAFT_REQUIRE_FFMPEG", "ffmpeg is unavailable");
            eprintln!("skipping FFmpeg failure test: ffmpeg is unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("broken.mp4");
        let output = dir.path().join("broken.wav");
        std::fs::write(&input, b"not a media file").unwrap();
        std::fs::write(&output, b"previous audio").unwrap();
        let opts = Options {
            overwrite: true,
            ..Options::default()
        };
        assert!(convert_to(&input, dir.path(), Category::Video, "wav", &opts).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"previous audio");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn ffmpeg_lookup_ignores_relative_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path_var =
            std::env::join_paths([Path::new("relative"), Path::new("."), dir.path()]).unwrap();
        let candidates: Vec<PathBuf> = ffmpeg_candidates(&path_var).collect();
        assert_eq!(candidates.len(), 1 + FALLBACK_FFMPEG_DIRS.len());
        assert!(candidates[0].starts_with(dir.path()));
        assert!(candidates.iter().all(|candidate| candidate.is_absolute()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ffmpeg_lookup_falls_back_to_homebrew_after_path() {
        // The PATH a Finder-launched app gets.
        let candidates: Vec<PathBuf> = ffmpeg_candidates(OsStr::new("/usr/bin:/bin")).collect();
        assert_eq!(
            candidates,
            [
                "/usr/bin/ffmpeg",
                "/bin/ffmpeg",
                "/opt/homebrew/bin/ffmpeg",
                "/usr/local/bin/ffmpeg"
            ]
            .map(PathBuf::from)
        );
    }

    /// Generate a short sample in `dir` for one media input extension.
    fn media_fixture(dir: &Path, ext: &str) -> PathBuf {
        let path = dir.join(format!("sample.{ext}"));
        let video = !matches!(
            ext,
            "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" | "aiff" | "aif"
        );
        let mut cmd = ffmpeg_command(ffmpeg_path().unwrap());
        cmd.args(["-y", "-loglevel", "error"]);
        if video {
            cmd.args(["-f", "lavfi", "-i", "testsrc=duration=1:size=64x48:rate=10"]);
        }
        cmd.args(["-f", "lavfi", "-i", "sine=duration=1"]);
        if matches!(ext, "mpg" | "mpeg") {
            cmd.args(["-c:v", "mpeg2video", "-c:a", "mp2"]);
        }
        let ok = cmd.arg("-shortest").arg(&path).status().unwrap().success();
        assert!(ok, "cannot generate {ext} fixture");
        path
    }

    #[test]
    fn every_media_input_extension_passes_the_demuxer_whitelist() {
        if ffmpeg_path().is_none() {
            require_unset("CONVERTCRAFT_REQUIRE_FFMPEG", "ffmpeg is unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for ext in [
            "mp3", "wav", "flac", "ogg", "m4a", "aac", "opus", "wma", "aiff", "aif", "mp4", "mkv",
            "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg", "ts",
        ] {
            let src = media_fixture(dir.path(), ext);
            let cat = detect(&src).unwrap();
            let out = dir.path().join(format!("out-{ext}"));
            convert_to(&src, &out, cat, "wav", &Options::default())
                .unwrap_or_else(|e| panic!("{ext}: {e:#}"));
        }
    }

    #[test]
    fn playlists_disguised_as_media_are_rejected() {
        if ffmpeg_path().is_none() {
            require_unset("CONVERTCRAFT_REQUIRE_FFMPEG", "ffmpeg is unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let secret = media_fixture(dir.path(), "ts");
        let playlists = [
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{}\n#EXT-X-ENDLIST\n",
                secret.display()
            ),
            format!("ffconcat version 1.0\nfile '{}'\n", secret.display()),
        ];
        for (i, playlist) in playlists.iter().enumerate() {
            let src = dir.path().join(format!("playlist{i}.mp4"));
            std::fs::write(&src, playlist).unwrap();
            let out = dir.path().join(format!("out{i}"));
            assert!(
                convert_to(&src, &out, Category::Video, "wav", &Options::default()).is_err(),
                "playlist {i} was converted"
            );
        }
    }

    #[test]
    fn cancelling_stops_ffmpeg_and_publishes_nothing() {
        if ffmpeg_path().is_none() {
            require_unset("CONVERTCRAFT_REQUIRE_FFMPEG", "ffmpeg is unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("long.mp4");
        let ok = ffmpeg_command(ffmpeg_path().unwrap())
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=duration=30:size=640x480:rate=30")
            .args(["-c:v", "libx264", "-preset", "ultrafast"])
            .arg(&src)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let out = dir.path().join("out");
        let (mut planner, errors) = OutputPlanner::new([src.as_path()]);
        assert!(errors.is_empty());
        let plan = planner.plan(&src, Some(&out), "webm", false).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let started = Instant::now();
        let worker = thread::spawn(move || {
            convert(
                &src,
                plan,
                Category::Video,
                "webm",
                &Options::default(),
                &flag,
            )
        });
        thread::sleep(Duration::from_millis(500));
        cancel.store(true, Ordering::Relaxed);
        let error = worker.join().unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn media_via_ffmpeg() {
        if ffmpeg_path().is_none() {
            require_unset("CONVERTCRAFT_REQUIRE_FFMPEG", "ffmpeg is unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mp4");
        let ok = ffmpeg_command(ffmpeg_path().unwrap())
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=15",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-shortest",
            ])
            .arg(&src)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        for &t in Category::Video.targets() {
            let out = convert_to(
                &src,
                &dir.path().join("video"),
                Category::Video,
                t,
                &Options::default(),
            )
            .unwrap_or_else(|e| panic!("{t}: {e:#}"));
            assert!(std::fs::metadata(&out).unwrap().len() > 0, "{t}");
        }
        let wav = dir.path().join("video/src.wav");
        for &t in Category::Audio.targets() {
            convert_to(
                &wav,
                &dir.path().join("audio"),
                Category::Audio,
                t,
                &Options::default(),
            )
            .unwrap_or_else(|e| panic!("{t}: {e:#}"));
        }
    }
}
