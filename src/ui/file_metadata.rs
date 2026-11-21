//! File details for the Files sidebar: binary sizes, durations and
//! header-only format probes (images, PDF, ISO BMFF video/audio, WAV, FLAC,
//! MP3, text). Everything here runs in the blocking metadata task, reads only
//! bounded prefixes or container headers, never decodes media, never spawns
//! external commands, and reports unknown values as `-` instead of guessing.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// What a probe could learn. `kind` fills the first detail line; `duration`
/// joins the size on the second line for timed media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaSummary {
    pub kind: String,
    pub duration: Option<String>,
}

const UNKNOWN: &str = "-";

/// Largest PDF scanned for page geometry; bigger files show `-`.
const PDF_SCAN_LIMIT: u64 = 128 * 1024 * 1024;
/// Largest decompressed PDF object stream accepted.
const PDF_OBJSTM_LIMIT: u64 = 16 * 1024 * 1024;
/// Total decompressed bytes across all object streams of one PDF, so many
/// small compressed streams cannot add up to an unbounded allocation.
const PDF_OBJSTM_TOTAL_LIMIT: u64 = 64 * 1024 * 1024;
/// Largest `moov` box read into memory.
const MP4_MOOV_LIMIT: u64 = 64 * 1024 * 1024;
/// Largest text file whose lines are counted.
const TEXT_SCAN_LIMIT: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Pure formatting
// ---------------------------------------------------------------------------

/// Binary (1024-based) size: `B` as an integer, then `KiB`, `MiB`, `GiB` and
/// `TiB` with one decimal. A value that would round up to `1024.0` of a unit
/// is shown as `1.0` of the next unit instead.
pub(crate) fn format_binary_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && (value * 10.0).round() / 10.0 >= 1024.0 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `MM:SS` below an hour, `HH:MM:SS` from an hour; `-` for unusable input.
pub(crate) fn format_duration(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds.filter(|value| value.is_finite() && *value >= 0.0) else {
        return UNKNOWN.to_string();
    };
    // Anything past ~11.5 days of media is a corrupt header, not a duration.
    if seconds > 1_000_000.0 {
        return UNKNOWN.to_string();
    }
    let total = seconds.floor() as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// `seconds = units / timescale`, rejecting a zero timescale.
fn ratio_seconds(units: u64, timescale: u64) -> Option<f64> {
    (timescale > 0).then(|| units as f64 / timescale as f64)
}

fn format_sample_rate(hz: u32) -> String {
    if hz == 0 {
        return UNKNOWN.to_string();
    }
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", f64::from(hz) / 1000.0)
    }
}

fn format_channels(channels: u16) -> String {
    match channels {
        0 => UNKNOWN.to_string(),
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        other => format!("{other}ch"),
    }
}

fn dims(width: u64, height: u64) -> String {
    if width == 0 || height == 0 {
        UNKNOWN.to_string()
    } else {
        format!("{width}×{height}")
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Probes `path` by extension. `None` means "no special handling" and the
/// caller shows the common details.
pub(crate) fn probe(path: &Path, extension: &str, size: u64) -> Option<MediaSummary> {
    let mut file = File::open(path).ok()?;
    match extension {
        "png" => probe_png(file),
        "jpg" | "jpeg" => Some(image_summary("JPEG", probe_jpeg(&mut file))),
        "gif" => Some(image_summary("GIF", probe_gif(&read_prefix(&mut file, 16)))),
        "bmp" => Some(image_summary("BMP", probe_bmp(&read_prefix(&mut file, 64)))),
        "webp" => Some(image_summary(
            "WEBP",
            probe_webp(&read_prefix(&mut file, 64)),
        )),
        "pdf" => Some(pdf_summary(probe_pdf(&mut file, size))),
        "mp4" | "m4v" | "mov" | "m4a" | "m4b" | "3gp" => Some(bmff_summary(
            &extension.to_ascii_uppercase(),
            probe_bmff(&mut file, size),
        )),
        "wav" => Some(probe_wav(&mut file)),
        "flac" => Some(probe_flac(&read_prefix(&mut file, 64))),
        "mp3" => Some(probe_mp3(&mut file)),
        _ if is_text_extension(extension) => Some(probe_text(file, size)),
        _ => None,
    }
}

pub(crate) fn is_text_extension(extension: &str) -> bool {
    matches!(
        extension,
        "c" | "cc"
            | "cpp"
            | "css"
            | "csv"
            | "h"
            | "hpp"
            | "html"
            | "js"
            | "json"
            | "log"
            | "md"
            | "py"
            | "rs"
            | "sh"
            | "swift"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "xml"
            | "yaml"
            | "yml"
    )
}

fn read_prefix(file: &mut File, limit: u64) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = file.by_ref().take(limit).read_to_end(&mut buffer);
    buffer
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}
fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}
fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ImageInfo {
    pub width: u64,
    pub height: u64,
    /// Color model and depth, e.g. `RGBA 8-bit`.
    pub color: Option<String>,
}

fn image_summary(format: &str, info: Option<ImageInfo>) -> MediaSummary {
    let kind = match info {
        Some(info) => {
            let mut kind = format!("{format} · {}", dims(info.width, info.height));
            if let Some(color) = info.color {
                kind.push_str(" · ");
                kind.push_str(&color);
            }
            kind
        }
        None => format!("{format} · {UNKNOWN}"),
    };
    MediaSummary {
        kind,
        duration: None,
    }
}

fn probe_png(file: File) -> Option<MediaSummary> {
    let info = png::Decoder::new(file).read_info().ok().map(|reader| {
        let info = reader.info();
        let (mode, alpha) = match info.color_type {
            png::ColorType::Grayscale => ("Gray", info.trns.is_some()),
            png::ColorType::GrayscaleAlpha => ("GrayA", true),
            png::ColorType::Rgb => ("RGB", info.trns.is_some()),
            png::ColorType::Rgba => ("RGBA", true),
            png::ColorType::Indexed => ("Indexed", info.trns.is_some()),
        };
        let bits = info.bit_depth as u8;
        ImageInfo {
            width: u64::from(info.width),
            height: u64::from(info.height),
            color: Some(format!(
                "{mode} {bits}-bit{}",
                if alpha && !mode.ends_with('A') {
                    " · alpha"
                } else {
                    ""
                }
            )),
        }
    });
    Some(image_summary("PNG", info))
}

/// Walks JPEG markers (seeking over segment bodies) to the first SOF.
fn probe_jpeg(file: &mut File) -> Option<ImageInfo> {
    let mut header = [0u8; 2];
    file.read_exact(&mut header).ok()?;
    if header != [0xFF, 0xD8] {
        return None;
    }
    for _ in 0..512 {
        let mut marker = [0u8; 2];
        file.read_exact(&mut marker).ok()?;
        if marker[0] != 0xFF {
            return None;
        }
        let kind = marker[1];
        if kind == 0xFF {
            // Fill byte: re-read from the second 0xFF.
            file.seek(SeekFrom::Current(-1)).ok()?;
            continue;
        }
        if matches!(kind, 0xD0..=0xD9 | 0x01) {
            continue;
        }
        let mut length = [0u8; 2];
        file.read_exact(&mut length).ok()?;
        let length = u16::from_be_bytes(length);
        if length < 2 {
            return None;
        }
        let is_sof = matches!(kind, 0xC0..=0xCF) && !matches!(kind, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let mut body = [0u8; 6];
            file.read_exact(&mut body).ok()?;
            let precision = body[0];
            let height = u64::from(u16::from_be_bytes([body[1], body[2]]));
            let width = u64::from(u16::from_be_bytes([body[3], body[4]]));
            let model = match body[5] {
                1 => "Gray",
                3 => "YCbCr",
                4 => "CMYK",
                _ => UNKNOWN,
            };
            return Some(ImageInfo {
                width,
                height,
                color: Some(format!("{model} {precision}-bit")),
            });
        }
        file.seek(SeekFrom::Current(i64::from(length) - 2)).ok()?;
    }
    None
}

fn probe_gif(bytes: &[u8]) -> Option<ImageInfo> {
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    Some(ImageInfo {
        width: u64::from(le_u16(bytes, 6)?),
        height: u64::from(le_u16(bytes, 8)?),
        color: Some("Indexed".to_string()),
    })
}

fn probe_bmp(bytes: &[u8]) -> Option<ImageInfo> {
    if !bytes.starts_with(b"BM") {
        return None;
    }
    let header_size = le_u32(bytes, 14)?;
    let (width, height, bits) = if header_size == 12 {
        (
            u64::from(le_u16(bytes, 18)?),
            u64::from(le_u16(bytes, 20)?),
            le_u16(bytes, 24)?,
        )
    } else {
        let width = le_u32(bytes, 18)? as i32;
        let height = le_u32(bytes, 22)? as i32;
        (
            u64::from(width.unsigned_abs()),
            u64::from(height.unsigned_abs()),
            le_u16(bytes, 28)?,
        )
    };
    Some(ImageInfo {
        width,
        height,
        color: Some(format!("{bits}-bit")),
    })
}

fn probe_webp(bytes: &[u8]) -> Option<ImageInfo> {
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    match bytes.get(12..16)? {
        b"VP8 " => {
            if bytes.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some(ImageInfo {
                width: u64::from(le_u16(bytes, 26)? & 0x3FFF),
                height: u64::from(le_u16(bytes, 28)? & 0x3FFF),
                color: Some("lossy".to_string()),
            })
        }
        b"VP8L" => {
            if *bytes.get(20)? != 0x2F {
                return None;
            }
            let bits = le_u32(bytes, 21)?;
            Some(ImageInfo {
                width: u64::from(bits & 0x3FFF) + 1,
                height: u64::from((bits >> 14) & 0x3FFF) + 1,
                color: Some(
                    if (bits >> 28) & 1 == 1 {
                        "lossless · alpha"
                    } else {
                        "lossless"
                    }
                    .to_string(),
                ),
            })
        }
        b"VP8X" => {
            let flags = *bytes.get(20)?;
            let read24 = |at: usize| -> Option<u64> {
                let b = bytes.get(at..at + 3)?;
                Some(u64::from(b[0]) | (u64::from(b[1]) << 8) | (u64::from(b[2]) << 16))
            };
            Some(ImageInfo {
                width: read24(24)? + 1,
                height: read24(27)? + 1,
                color: (flags & 0x10 != 0).then(|| "alpha".to_string()),
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PDF
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PdfInfo {
    /// First page size in CSS pixels (96 DPI), rotation applied.
    pub width_px: Option<u64>,
    pub height_px: Option<u64>,
    pub pages: Option<u64>,
}

fn pdf_summary(info: PdfInfo) -> MediaSummary {
    let size = match (info.width_px, info.height_px) {
        (Some(width), Some(height)) => format!("{width}×{height} px"),
        _ => UNKNOWN.to_string(),
    };
    let pages = info
        .pages
        .map_or_else(|| UNKNOWN.to_string(), |pages| pages.to_string());
    MediaSummary {
        kind: format!("PDF · {size} · {pages}p"),
        duration: None,
    }
}

/// PDF points to 96-DPI pixels: `round(points / 72 × 96)`.
pub(crate) fn pdf_points_to_px(points: f64) -> Option<u64> {
    let px = (points / 72.0 * 96.0).round();
    (px.is_finite() && (1.0..=10_000_000.0).contains(&px)).then_some(px as u64)
}

fn probe_pdf(file: &mut File, size: u64) -> PdfInfo {
    if size > PDF_SCAN_LIMIT {
        return PdfInfo::default();
    }
    let bytes = read_prefix(file, PDF_SCAN_LIMIT);
    parse_pdf(&bytes)
}

type PdfObjects = std::collections::HashMap<u32, Vec<u8>>;

/// Minimal, tolerant PDF reader for page 1 geometry and the page count. It
/// indexes `N G obj … endobj` bodies directly (no xref trust), expands
/// Flate-compressed object streams, then walks the page tree from the
/// catalog. Encrypted or damaged files simply yield fewer facts.
pub(crate) fn parse_pdf(bytes: &[u8]) -> PdfInfo {
    if !bytes.starts_with(b"%PDF") {
        return PdfInfo::default();
    }
    let objects = pdf_objects(bytes);
    let Some(catalog) = objects
        .values()
        .find(|body| pdf_dict_has_type(pdf_dict_part(body), b"Catalog"))
    else {
        return PdfInfo::default();
    };
    let Some(root_ref) = pdf_ref_value(pdf_dict_part(catalog), b"Pages") else {
        return PdfInfo::default();
    };
    let pages = objects
        .get(&root_ref)
        .and_then(|root| pdf_int_value(pdf_dict_part(root), b"Count"))
        .and_then(|count| u64::try_from(count).ok())
        .filter(|count| *count <= 10_000_000);

    // Walk to the first leaf page, collecting inherited box/rotate values.
    let mut media = None;
    let mut crop = None;
    let mut rotate = 0i64;
    let mut node_ref = root_ref;
    let mut found_page = false;
    for _ in 0..64 {
        let Some(node) = objects.get(&node_ref) else {
            break;
        };
        let dict = pdf_dict_part(node);
        if let Some(value) = pdf_box_value(dict, b"MediaBox", &objects) {
            media = Some(value);
        }
        if let Some(value) = pdf_box_value(dict, b"CropBox", &objects) {
            crop = Some(value);
        }
        if let Some(value) = pdf_int_value(dict, b"Rotate") {
            rotate = value;
        }
        if pdf_dict_has_type(dict, b"Page") {
            found_page = true;
            break;
        }
        match pdf_first_kid(dict) {
            Some(kid) => node_ref = kid,
            None => break,
        }
    }
    let size = found_page
        .then(|| page_size_points(media, crop, rotate))
        .flatten();
    PdfInfo {
        width_px: size.and_then(|(width, _)| pdf_points_to_px(width)),
        height_px: size.and_then(|(_, height)| pdf_points_to_px(height)),
        pages,
    }
}

/// Visible page size: CropBox clipped to MediaBox when both exist, else
/// whichever exists; 90°/270° rotation swaps width and height.
pub(crate) fn page_size_points(
    media: Option<[f64; 4]>,
    crop: Option<[f64; 4]>,
    rotate: i64,
) -> Option<(f64, f64)> {
    let normalize = |b: [f64; 4]| {
        [
            b[0].min(b[2]),
            b[1].min(b[3]),
            b[0].max(b[2]),
            b[1].max(b[3]),
        ]
    };
    let area = match (media.map(normalize), crop.map(normalize)) {
        (Some(m), Some(c)) => {
            let clipped = [
                m[0].max(c[0]),
                m[1].max(c[1]),
                m[2].min(c[2]),
                m[3].min(c[3]),
            ];
            if clipped[2] > clipped[0] && clipped[3] > clipped[1] {
                clipped
            } else {
                m
            }
        }
        (Some(m), None) => m,
        (None, Some(c)) => c,
        (None, None) => return None,
    };
    let (width, height) = (area[2] - area[0], area[3] - area[1]);
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return None;
    }
    if rotate.rem_euclid(180) == 90 {
        Some((height, width))
    } else {
        Some((width, height))
    }
}

fn pdf_objects(bytes: &[u8]) -> PdfObjects {
    let mut objects = PdfObjects::new();
    let Ok(header) = regex::bytes::Regex::new(r"(?-u)(\d{1,10})\s+\d{1,5}\s+obj\b") else {
        return objects;
    };
    let mut streams = Vec::new();
    for capture in header.captures_iter(bytes) {
        let (Some(whole), Some(number)) = (capture.get(0), capture.get(1)) else {
            continue;
        };
        let Some(number) = std::str::from_utf8(number.as_bytes())
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        let start = whole.end();
        let end = find(bytes, b"endobj", start).unwrap_or(bytes.len());
        let body = bytes[start..end].to_vec();
        if pdf_dict_has_type(pdf_dict_part(&body), b"ObjStm") {
            streams.push(body.clone());
        }
        // Later definitions (incremental updates) replace earlier ones.
        objects.insert(number, body);
    }
    let mut budget = PDF_OBJSTM_TOTAL_LIMIT;
    for stream in streams {
        if budget == 0 {
            break;
        }
        let mut used = 0;
        let expanded = pdf_object_stream(&stream, budget.min(PDF_OBJSTM_LIMIT), &mut used);
        budget = budget.saturating_sub(used);
        for (number, body) in expanded {
            objects.entry(number).or_insert(body);
        }
    }
    objects
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// The dictionary text of an object body (everything before `stream`).
fn pdf_dict_part(body: &[u8]) -> &[u8] {
    match find(body, b"stream", 0) {
        Some(at) => &body[..at],
        None => body,
    }
}

/// Expands one Flate object stream, decompressing at most `limit` bytes.
/// `used` receives how many decompressed bytes were produced.
fn pdf_object_stream(body: &[u8], limit: u64, used: &mut u64) -> Vec<(u32, Vec<u8>)> {
    let dict = pdf_dict_part(body);
    if !contains(dict, b"/FlateDecode") || contains(dict, b"/DecodeParms") {
        return Vec::new();
    }
    let (Some(count), Some(first)) = (pdf_int_value(dict, b"N"), pdf_int_value(dict, b"First"))
    else {
        return Vec::new();
    };
    let Some(stream_at) = find(body, b"stream", 0) else {
        return Vec::new();
    };
    let mut data_at = stream_at + b"stream".len();
    if body.get(data_at) == Some(&b'\r') {
        data_at += 1;
    }
    if body.get(data_at) == Some(&b'\n') {
        data_at += 1;
    }
    let data_end = find(body, b"endstream", data_at).unwrap_or(body.len());
    let Some(data) = body.get(data_at..data_end) else {
        return Vec::new();
    };
    let mut decoded = Vec::new();
    let decoder = flate2::read::ZlibDecoder::new(data);
    let result = decoder.take(limit).read_to_end(&mut decoded);
    *used = decoded.len() as u64;
    if result.is_err() && decoded.is_empty() {
        return Vec::new();
    }
    let (Ok(count), Ok(first)) = (usize::try_from(count), usize::try_from(first)) else {
        return Vec::new();
    };
    let Some(index) = decoded.get(..first.min(decoded.len())) else {
        return Vec::new();
    };
    let numbers = std::str::from_utf8(index)
        .unwrap_or_default()
        .split_ascii_whitespace()
        .filter_map(|token| token.parse::<usize>().ok())
        .collect::<Vec<_>>();
    let pairs = numbers
        .chunks_exact(2)
        .take(count.min(1_000_000))
        .map(|pair| (pair[0], pair[1]))
        .collect::<Vec<_>>();
    let mut objects = Vec::with_capacity(pairs.len());
    for (index, (number, offset)) in pairs.iter().enumerate() {
        let start = first.saturating_add(*offset);
        let end = pairs
            .get(index + 1)
            .map_or(decoded.len(), |(_, next)| first.saturating_add(*next));
        let (Some(body), Ok(number)) = (decoded.get(start..end.max(start)), u32::try_from(*number))
        else {
            continue;
        };
        objects.push((number, body.to_vec()));
    }
    objects
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle, 0).is_some()
}

/// Position just after `/Key` when it is a whole name (not `/KeyMore`).
fn pdf_key(dict: &[u8], key: &[u8]) -> Option<usize> {
    let mut from = 0;
    loop {
        let at = find(dict, b"/", from)?;
        let name_end = at + 1 + key.len();
        if dict.get(at + 1..name_end) == Some(key)
            && dict
                .get(name_end)
                .is_none_or(|next| !next.is_ascii_alphanumeric())
        {
            return Some(name_end);
        }
        from = at + 1;
    }
}

fn skip_ws(dict: &[u8], mut at: usize) -> usize {
    while dict.get(at).is_some_and(|byte| byte.is_ascii_whitespace()) {
        at += 1;
    }
    at
}

fn pdf_dict_has_type(dict: &[u8], name: &[u8]) -> bool {
    let mut from = 0;
    while let Some(after) = pdf_key(&dict[from..], b"Type").map(|offset| from + offset) {
        let at = skip_ws(dict, after);
        if dict.get(at) == Some(&b'/') {
            let end = at + 1 + name.len();
            if dict.get(at + 1..end) == Some(name)
                && dict
                    .get(end)
                    .is_none_or(|next| !next.is_ascii_alphanumeric())
            {
                return true;
            }
        }
        from = after;
    }
    false
}

fn pdf_number_at(dict: &[u8], at: usize) -> Option<(f64, usize)> {
    let start = skip_ws(dict, at);
    let mut end = start;
    while dict
        .get(end)
        .is_some_and(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.'))
    {
        end += 1;
    }
    let value = std::str::from_utf8(dict.get(start..end)?)
        .ok()?
        .parse::<f64>()
        .ok()?;
    value.is_finite().then_some((value, end))
}

fn pdf_int_value(dict: &[u8], key: &[u8]) -> Option<i64> {
    let (value, end) = pdf_number_at(dict, pdf_key(dict, key)?)?;
    // An indirect reference ("5 0 R") is not an inline integer.
    let after = skip_ws(dict, end);
    if dict.get(after).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    (value.fract() == 0.0 && value.abs() < 1e15).then_some(value as i64)
}

/// `N G R` immediately after `at`.
fn pdf_ref_at(dict: &[u8], at: usize) -> Option<u32> {
    let (number, end) = pdf_number_at(dict, at)?;
    let (_, end) = pdf_number_at(dict, end)?;
    let marker = skip_ws(dict, end);
    (dict.get(marker) == Some(&b'R') && number >= 0.0 && number.fract() == 0.0)
        .then(|| u32::try_from(number as u64).ok())
        .flatten()
}

fn pdf_ref_value(dict: &[u8], key: &[u8]) -> Option<u32> {
    pdf_ref_at(dict, pdf_key(dict, key)?)
}

fn pdf_first_kid(dict: &[u8]) -> Option<u32> {
    let at = skip_ws(dict, pdf_key(dict, b"Kids")?);
    (dict.get(at) == Some(&b'['))
        .then(|| pdf_ref_at(dict, at + 1))
        .flatten()
}

fn pdf_array_numbers(text: &[u8], at: usize) -> Option<[f64; 4]> {
    let open = skip_ws(text, at);
    if text.get(open) != Some(&b'[') {
        return None;
    }
    let mut values = [0.0; 4];
    let mut cursor = open + 1;
    for value in &mut values {
        let (number, end) = pdf_number_at(text, cursor)?;
        *value = number;
        cursor = end;
    }
    Some(values)
}

/// A box given inline (`[0 0 612 792]`) or by reference to an array object.
fn pdf_box_value(dict: &[u8], key: &[u8], objects: &PdfObjects) -> Option<[f64; 4]> {
    let at = pdf_key(dict, key)?;
    pdf_array_numbers(dict, at).or_else(|| {
        let reference = pdf_ref_at(dict, at)?;
        pdf_array_numbers(objects.get(&reference)?, 0)
    })
}

// ---------------------------------------------------------------------------
// ISO BMFF (MP4 / MOV / M4A)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BmffInfo {
    pub duration_seconds_milli: Option<u64>,
    pub video: Option<(u64, u64, String)>,
    pub audio: Option<(String, u32, u16)>,
}

fn bmff_summary(container: &str, info: Option<BmffInfo>) -> MediaSummary {
    let Some(info) = info else {
        return MediaSummary {
            kind: container.to_string(),
            duration: Some(UNKNOWN.to_string()),
        };
    };
    let kind = match (&info.video, &info.audio) {
        (Some((width, height, codec)), _) => {
            format!("{container} · {} · {codec}", dims(*width, *height))
        }
        (None, Some((codec, rate, channels))) => format!(
            "{container} · {codec} · {} · {}",
            format_sample_rate(*rate),
            format_channels(*channels)
        ),
        (None, None) => container.to_string(),
    };
    MediaSummary {
        kind,
        duration: Some(format_duration(
            info.duration_seconds_milli
                .map(|millis| millis as f64 / 1000.0),
        )),
    }
}

/// Reads top-level box headers (seeking over `mdat`) to load `moov` only.
fn probe_bmff(file: &mut File, size: u64) -> Option<BmffInfo> {
    let mut offset = 0u64;
    for _ in 0..4096 {
        if offset.checked_add(8)? > size {
            return None;
        }
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let mut box_size = u64::from(be_u32(&header, 0)?);
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&header[4..8]);
        let mut header_len = 8u64;
        if box_size == 1 {
            file.read_exact(&mut header[8..16]).ok()?;
            box_size = be_u64(&header, 8)?;
            header_len = 16;
        } else if box_size == 0 {
            box_size = size - offset;
        }
        if box_size < header_len {
            return None;
        }
        if &kind == b"moov" {
            let body_len = box_size - header_len;
            if body_len > MP4_MOOV_LIMIT {
                return None;
            }
            let mut body = Vec::new();
            file.by_ref().take(body_len).read_to_end(&mut body).ok()?;
            return Some(parse_moov(&body));
        }
        offset = offset.checked_add(box_size)?;
    }
    None
}

/// Child boxes of a container body: `(type, body)`.
fn bmff_children(body: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut children = Vec::new();
    let mut at = 0usize;
    while at + 8 <= body.len() && children.len() < 4096 {
        let Some(size) = be_u32(body, at) else {
            break;
        };
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&body[at + 4..at + 8]);
        let (size, header) = match size {
            1 => match be_u64(body, at + 8).and_then(|size| usize::try_from(size).ok()) {
                Some(size) => (size, 16),
                None => break,
            },
            0 => (body.len() - at, 8),
            size => (size as usize, 8),
        };
        if size < header || at + size > body.len() {
            break;
        }
        children.push((kind, &body[at + header..at + size]));
        at += size;
    }
    children
}

fn bmff_child<'a>(body: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    bmff_children(body)
        .into_iter()
        .find(|(child, _)| child == kind)
        .map(|(_, body)| body)
}

/// (timescale, duration) from an `mvhd`/`mdhd` body.
fn bmff_time(body: &[u8]) -> Option<(u64, u64)> {
    match *body.first()? {
        0 => Some((u64::from(be_u32(body, 12)?), u64::from(be_u32(body, 16)?))),
        1 => Some((u64::from(be_u32(body, 20)?), be_u64(body, 24)?)),
        _ => None,
    }
}

pub(crate) fn parse_moov(moov: &[u8]) -> BmffInfo {
    let mut info = BmffInfo {
        duration_seconds_milli: bmff_child(moov, b"mvhd")
            .and_then(bmff_time)
            .and_then(|(timescale, duration)| ratio_seconds(duration, timescale))
            .map(|seconds| (seconds * 1000.0) as u64),
        ..BmffInfo::default()
    };
    for (kind, trak) in bmff_children(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = bmff_child(trak, b"mdia") else {
            continue;
        };
        let handler = bmff_child(mdia, b"hdlr").and_then(|hdlr| hdlr.get(8..12));
        let entry = bmff_child(mdia, b"minf")
            .and_then(|minf| bmff_child(minf, b"stbl"))
            .and_then(|stbl| bmff_child(stbl, b"stsd"))
            .and_then(|stsd| stsd.get(8..))
            .and_then(|entries| {
                let size = be_u32(entries, 0)? as usize;
                entries.get(..size.min(entries.len()))
            });
        let Some(entry) = entry else {
            continue;
        };
        let Some(fourcc) = entry.get(4..8) else {
            continue;
        };
        match handler {
            Some(b"vide") if info.video.is_none() => {
                let (mut width, mut height) = bmff_child(trak, b"tkhd")
                    .and_then(|tkhd| {
                        let at = if *tkhd.first()? == 1 { 88 } else { 76 };
                        Some((
                            u64::from(be_u32(tkhd, at)? >> 16),
                            u64::from(be_u32(tkhd, at + 4)? >> 16),
                        ))
                    })
                    .unwrap_or((0, 0));
                if width == 0 || height == 0 {
                    width = u64::from(be_u16(entry, 32).unwrap_or(0));
                    height = u64::from(be_u16(entry, 34).unwrap_or(0));
                }
                info.video = Some((width, height, video_codec_name(fourcc)));
            }
            Some(b"soun") if info.audio.is_none() => {
                let channels = be_u16(entry, 24).unwrap_or(0);
                let rate = be_u32(entry, 32).map_or(0, |rate| rate >> 16);
                info.audio = Some((audio_codec_name(fourcc, entry), rate, channels));
            }
            _ => {}
        }
    }
    info
}

fn video_codec_name(fourcc: &[u8]) -> String {
    match fourcc {
        b"avc1" | b"avc3" => "H.264",
        b"hvc1" | b"hev1" => "HEVC",
        b"av01" => "AV1",
        b"vp09" => "VP9",
        b"vp08" => "VP8",
        b"mp4v" => "MPEG-4",
        b"ap4h" | b"ap4x" | b"apch" | b"apcn" | b"apcs" | b"apco" => "ProRes",
        b"jpeg" | b"mjpa" | b"mjpb" => "MJPEG",
        other => return String::from_utf8_lossy(other).trim().to_string(),
    }
    .to_string()
}

fn audio_codec_name(fourcc: &[u8], entry: &[u8]) -> String {
    match fourcc {
        b"mp4a" => match esds_object_type(entry) {
            Some(0x40 | 0x66..=0x68) => "AAC",
            Some(0x69 | 0x6B) => "MP3",
            _ => "mp4a",
        },
        b"alac" => "ALAC",
        b"Opus" => "Opus",
        b"fLaC" => "FLAC",
        b"ac-3" => "AC-3",
        b"ec-3" => "E-AC-3",
        b"lpcm" | b"sowt" | b"twos" | b"ipcm" | b"fpcm" => "PCM",
        other => return String::from_utf8_lossy(other).trim().to_string(),
    }
    .to_string()
}

/// `objectTypeIndication` from the `esds` box inside an `mp4a` entry.
fn esds_object_type(entry: &[u8]) -> Option<u8> {
    // Sample entry header (8) + audio sample entry fields (28).
    let esds = bmff_child(entry.get(36..)?, b"esds")?;
    let mut at = 4; // version + flags
    let read_len = |at: &mut usize| -> Option<usize> {
        let mut length = 0usize;
        for _ in 0..4 {
            let byte = *esds.get(*at)?;
            *at += 1;
            length = (length << 7) | usize::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                break;
            }
        }
        Some(length)
    };
    if *esds.get(at)? != 0x03 {
        return None;
    }
    at += 1;
    read_len(&mut at)?;
    at += 2; // ES_ID
    let flags = *esds.get(at)?;
    at += 1;
    if flags & 0x80 != 0 {
        at += 2;
    }
    if flags & 0x40 != 0 {
        at += 1 + usize::from(*esds.get(at)?);
    }
    if flags & 0x20 != 0 {
        at += 2;
    }
    if *esds.get(at)? != 0x04 {
        return None;
    }
    at += 1;
    read_len(&mut at)?;
    esds.get(at).copied()
}

// ---------------------------------------------------------------------------
// WAV / FLAC / MP3
// ---------------------------------------------------------------------------

fn probe_wav(file: &mut File) -> MediaSummary {
    let fallback = MediaSummary {
        kind: "WAV".to_string(),
        duration: Some(UNKNOWN.to_string()),
    };
    let mut header = [0u8; 12];
    if file.read_exact(&mut header).is_err()
        || &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
    {
        return fallback;
    }
    let mut format = None;
    let mut data_size = None;
    for _ in 0..256 {
        let mut chunk = [0u8; 8];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        let size = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
        match &chunk[0..4] {
            b"fmt " => {
                let mut body = vec![0u8; size.min(64) as usize];
                if file.read_exact(&mut body).is_err() {
                    break;
                }
                format = Some(body);
                if file
                    .seek(SeekFrom::Current(
                        (size + (size & 1)) as i64 - size.min(64) as i64,
                    ))
                    .is_err()
                {
                    break;
                }
            }
            b"data" => {
                data_size = Some(size);
                break;
            }
            _ => {
                if file
                    .seek(SeekFrom::Current((size + (size & 1)) as i64))
                    .is_err()
                {
                    break;
                }
            }
        }
        if format.is_some() && data_size.is_some() {
            break;
        }
    }
    let Some(format) = format else {
        return fallback;
    };
    let tag = le_u16(&format, 0).unwrap_or(0);
    let channels = le_u16(&format, 2).unwrap_or(0);
    let rate = le_u32(&format, 4).unwrap_or(0);
    let byte_rate = le_u32(&format, 8).unwrap_or(0);
    let bits = le_u16(&format, 14).unwrap_or(0);
    let codec = match tag {
        1 | 0xFFFE => format!("PCM {bits}-bit"),
        3 => format!("Float {bits}-bit"),
        _ => UNKNOWN.to_string(),
    };
    MediaSummary {
        kind: format!(
            "WAV · {codec} · {} · {}",
            format_sample_rate(rate),
            format_channels(channels)
        ),
        duration: Some(format_duration(
            data_size.and_then(|size| ratio_seconds(size, u64::from(byte_rate))),
        )),
    }
}

pub(crate) fn probe_flac(bytes: &[u8]) -> MediaSummary {
    let parsed = (|| {
        if bytes.get(0..4)? != b"fLaC" || bytes.get(4)? & 0x7F != 0 {
            return None;
        }
        let info = bytes.get(8..8 + 34)?;
        let packed = be_u64(info, 10)?;
        let rate = (packed >> 44) as u32;
        let channels = ((packed >> 41) & 0x7) as u16 + 1;
        let bits = ((packed >> 36) & 0x1F) as u16 + 1;
        let samples = packed & 0xF_FFFF_FFFF;
        Some((rate, channels, bits, samples))
    })();
    match parsed {
        Some((rate, channels, bits, samples)) => MediaSummary {
            kind: format!(
                "FLAC · {} · {} · {bits}-bit",
                format_sample_rate(rate),
                format_channels(channels)
            ),
            duration: Some(format_duration(
                (samples > 0)
                    .then(|| ratio_seconds(samples, u64::from(rate)))
                    .flatten(),
            )),
        },
        None => MediaSummary {
            kind: "FLAC".to_string(),
            duration: Some(UNKNOWN.to_string()),
        },
    }
}

fn probe_mp3(file: &mut File) -> MediaSummary {
    let mut head = read_prefix(file, 10);
    let mut skip = 0u64;
    if head.starts_with(b"ID3") && head.len() == 10 {
        let size = head[6..10]
            .iter()
            .fold(0u64, |size, byte| (size << 7) | u64::from(byte & 0x7F));
        skip = 10 + size + if head[5] & 0x10 != 0 { 10 } else { 0 };
    }
    if file.seek(SeekFrom::Start(skip)).is_err() {
        head.clear();
    } else {
        head = read_prefix(file, 64 * 1024);
    }
    parse_mp3_frames(&head)
}

/// First MPEG audio frame header, plus a Xing/Info or VBRI frame count for an
/// exact duration. No count, no duration: bitrate-based estimates are not
/// shown.
pub(crate) fn parse_mp3_frames(bytes: &[u8]) -> MediaSummary {
    let fallback = MediaSummary {
        kind: "MP3".to_string(),
        duration: Some(UNKNOWN.to_string()),
    };
    let Some(at) = (0..bytes.len().saturating_sub(4)).find(|&at| {
        bytes[at] == 0xFF
            && bytes[at + 1] & 0xE0 == 0xE0
            && (bytes[at + 1] >> 3) & 0x3 != 1
            && (bytes[at + 1] >> 1) & 0x3 != 0
            && (bytes[at + 2] >> 4) != 0xF
            && (bytes[at + 2] >> 2) & 0x3 != 3
    }) else {
        return fallback;
    };
    let version = (bytes[at + 1] >> 3) & 0x3; // 3: MPEG1, 2: MPEG2, 0: MPEG2.5
    let layer = (bytes[at + 1] >> 1) & 0x3; // 1: III, 2: II, 3: I
    let rate_index = usize::from((bytes[at + 2] >> 2) & 0x3);
    let mono = (bytes[at + 3] >> 6) == 3;
    let rate = match version {
        3 => [44_100, 48_000, 32_000][rate_index],
        2 => [22_050, 24_000, 16_000][rate_index],
        _ => [11_025, 12_000, 8_000][rate_index],
    };
    let samples_per_frame: u64 = match (layer, version) {
        (3, _) => 384,
        (2, _) | (1, 3) => 1152,
        _ => 576,
    };
    let side_info = match (version == 3, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    let xing_at = at + 4 + side_info;
    let frames = if matches!(bytes.get(xing_at..xing_at + 4), Some(b"Xing" | b"Info")) {
        be_u32(bytes, xing_at + 4)
            .filter(|flags| flags & 1 == 1)
            .and_then(|_| be_u32(bytes, xing_at + 8))
    } else if bytes.get(at + 36..at + 40) == Some(b"VBRI") {
        be_u32(bytes, at + 36 + 14)
    } else {
        None
    };
    MediaSummary {
        kind: format!(
            "MP3 · {} · {}",
            format_sample_rate(rate),
            format_channels(if mono { 1 } else { 2 })
        ),
        duration: Some(format_duration(frames.and_then(|frames| {
            ratio_seconds(u64::from(frames) * samples_per_frame, u64::from(rate))
        }))),
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Streams the file in chunks: newline count and UTF-8 validity, without
/// holding the file in memory. Files over the scan limit show `-` lines.
fn probe_text(file: File, size: u64) -> MediaSummary {
    if size > TEXT_SCAN_LIMIT {
        return MediaSummary {
            kind: format!("TEXT · {UNKNOWN} · {UNKNOWN} lines"),
            duration: None,
        };
    }
    let (lines, utf8) = scan_text(file);
    MediaSummary {
        kind: format!(
            "TEXT · {} · {} lines",
            if utf8 { "UTF-8" } else { UNKNOWN },
            lines.map_or_else(|| UNKNOWN.to_string(), |lines| lines.to_string())
        ),
        duration: None,
    }
}

pub(crate) fn scan_text(mut reader: impl Read) -> (Option<u64>, bool) {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut carry = Vec::new();
    let mut newlines = 0u64;
    let mut utf8 = true;
    let mut last = None;
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return (None, false),
        };
        let chunk = &buffer[..read];
        total += read as u64;
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
        last = chunk.last().copied();
        if utf8 {
            carry.extend_from_slice(chunk);
            match std::str::from_utf8(&carry) {
                Ok(_) => carry.clear(),
                Err(err) if err.error_len().is_none() => {
                    let valid = err.valid_up_to();
                    carry.drain(..valid);
                }
                Err(_) => {
                    utf8 = false;
                    carry.clear();
                }
            }
        }
    }
    if !carry.is_empty() {
        utf8 = false;
    }
    let lines = if total == 0 {
        0
    } else {
        newlines + u64::from(last != Some(b'\n'))
    };
    (Some(lines), utf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_sizes_use_1024_steps_and_iec_units() {
        let cases = [
            (0, "0 B"),
            (1, "1 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1025, "1.0 KiB"),
            (1024 * 1024 - 1, "1.0 MiB"),
            (1024 * 1024, "1.0 MiB"),
            (567_091, "553.8 KiB"),
            (2_516_582, "2.4 MiB"),
            (1024 * 1024 * 1024, "1.0 GiB"),
            (1_288_490_189, "1.2 GiB"),
            (1024u64.pow(4), "1.0 TiB"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(format_binary_size(bytes), expected, "{bytes}");
        }
        for bytes in [u64::MAX, u64::MAX / 3, 1 << 62] {
            let text = format_binary_size(bytes);
            assert!(text.ends_with(" TiB") && !text.contains("NaN") && !text.contains("inf"));
        }
        for bytes in [0, 1023, 1024, 5_000_000, 9_000_000_000] {
            let text = format_binary_size(bytes);
            assert!(!text.contains("KB") && !text.contains("MB") && !text.contains("GB"));
        }
    }

    #[test]
    fn durations_are_mm_ss_or_hh_mm_ss_and_reject_bad_values() {
        assert_eq!(format_duration(Some(0.0)), "00:00");
        assert_eq!(format_duration(Some(92.9)), "01:32");
        assert_eq!(format_duration(Some(225.0)), "03:45");
        assert_eq!(format_duration(Some(3599.99)), "59:59");
        assert_eq!(format_duration(Some(3600.0)), "01:00:00");
        assert_eq!(format_duration(Some(5025.0)), "01:23:45");
        for bad in [
            None,
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(1e12),
        ] {
            assert_eq!(format_duration(bad), "-");
        }
        assert_eq!(ratio_seconds(10, 0), None);
    }

    fn pdf(objects: &[&str]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        for (index, body) in objects.iter().enumerate() {
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        out.extend_from_slice(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
        out
    }

    #[test]
    fn pdf_single_page_media_box_converts_points_to_96_dpi_pixels() {
        let bytes = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595.28 841.89] >>",
        ]);
        let info = parse_pdf(&bytes);
        assert_eq!(info.pages, Some(1));
        assert_eq!((info.width_px, info.height_px), (Some(794), Some(1123)));
        assert_eq!(pdf_points_to_px(930.0), Some(1240));
        assert_eq!(pdf_summary(info).kind, "PDF · 794×1123 px · 1p");
    }

    #[test]
    fn pdf_multi_page_tree_inherits_boxes_prefers_crop_box_and_applies_rotation() {
        let bytes = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 3 /MediaBox [0 0 612 792] >>",
            "<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 2 /Rotate 90 >>",
            "<< /Type /Page /Parent 3 0 R /CropBox [36 36 576 756] >>",
            "<< /Type /Page /Parent 2 0 R >>",
        ]);
        let info = parse_pdf(&bytes);
        assert_eq!(info.pages, Some(3));
        // CropBox 540×720 pt, rotated 90° → 720×540 pt → 960×720 px.
        assert_eq!((info.width_px, info.height_px), (Some(960), Some(720)));
    }

    #[test]
    fn pdf_indirect_media_box_and_270_rotation() {
        let bytes = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /MediaBox 4 0 R /Rotate 270 >>",
            "[0 0 72 144]",
        ]);
        let info = parse_pdf(&bytes);
        assert_eq!((info.width_px, info.height_px), (Some(192), Some(96)));
    }

    #[test]
    fn pdf_page_objects_inside_compressed_object_streams_are_found() {
        use std::io::Write;
        let inner =
            "<< /Type /Pages /Kids [4 0 R] /Count 1 >> << /Type /Page /MediaBox [0 0 300 150] >>";
        let header = "3 0 4 43 ";
        let payload = format!("{header}{inner}");
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut bytes = b"%PDF-1.5\n1 0 obj\n<< /Type /Catalog /Pages 3 0 R >>\nendobj\n".to_vec();
        bytes.extend_from_slice(
            format!(
                "2 0 obj\n<< /Type /ObjStm /N 2 /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
                header.len(),
                compressed.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");
        let info = parse_pdf(&bytes);
        assert_eq!(info.pages, Some(1));
        assert_eq!((info.width_px, info.height_px), (Some(400), Some(200)));
    }

    #[test]
    fn broken_encrypted_or_empty_pdfs_never_panic_and_show_dashes() {
        let encrypted = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /ObjStm /N 1 /First 4 /Filter /FlateDecode >>\nstream\n\x01\x02garbage\nendstream",
        ]);
        let truncated = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R".to_vec();
        let garbage = pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count -5 >>",
            "<< /Type /Page /MediaBox [0 0 -1 nan] >>",
        ]);
        for bytes in [
            Vec::new(),
            b"%PDF".to_vec(),
            encrypted,
            truncated,
            garbage,
            vec![0xFF; 4096],
        ] {
            let info = parse_pdf(&bytes);
            assert_eq!((info.width_px, info.height_px), (None, None));
            let kind = pdf_summary(info).kind;
            assert!(kind.starts_with("PDF · - · "), "{kind}");
        }
        assert_eq!(page_size_points(Some([0.0, 0.0, 0.0, 0.0]), None, 0), None);
        assert_eq!(page_size_points(None, None, 90), None);
        assert_eq!(pdf_points_to_px(f64::INFINITY), None);
        assert_eq!(pdf_points_to_px(-10.0), None);
    }

    #[test]
    fn pdf_name_matching_does_not_confuse_pages_and_page() {
        let dict = b"<< /Type /Pages /Count 3 >>";
        assert!(pdf_dict_has_type(dict, b"Pages"));
        assert!(!pdf_dict_has_type(dict, b"Page"));
        assert_eq!(
            pdf_int_value(b"<< /CountX 9 /Count 4 >>", b"Count"),
            Some(4)
        );
    }

    fn bmff_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn video_moov() -> Vec<u8> {
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&92_500u32.to_be_bytes());
        let mut tkhd = vec![0u8; 84];
        tkhd[76..80].copy_from_slice(&(1920u32 << 16).to_be_bytes());
        tkhd[80..84].copy_from_slice(&(1080u32 << 16).to_be_bytes());
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");
        let entry = bmff_box(b"avc1", &[0u8; 78]);
        let mut stsd = vec![0u8; 8];
        stsd.extend_from_slice(&entry);
        let stbl = bmff_box(b"stbl", &bmff_box(b"stsd", &stsd));
        let minf = bmff_box(b"minf", &stbl);
        let mut mdia_body = bmff_box(b"hdlr", &hdlr);
        mdia_body.extend_from_slice(&minf);
        let mut trak_body = bmff_box(b"tkhd", &tkhd);
        trak_body.extend_from_slice(&bmff_box(b"mdia", &mdia_body));
        let mut moov = bmff_box(b"mvhd", &mvhd);
        moov.extend_from_slice(&bmff_box(b"trak", &trak_body));
        moov
    }

    #[test]
    fn mp4_moov_gives_dimensions_codec_and_duration() {
        let info = parse_moov(&video_moov());
        assert_eq!(info.video, Some((1920, 1080, "H.264".to_string())));
        let summary = bmff_summary("MP4", Some(info));
        assert_eq!(summary.kind, "MP4 · 1920×1080 · H.264");
        assert_eq!(summary.duration.as_deref(), Some("01:32"));
    }

    #[test]
    fn object_stream_decompression_stops_at_the_given_budget() {
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![b' '; 4 * 1024 * 1024]).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut body = b"<< /Type /ObjStm /N 1 /First 4 /Filter /FlateDecode >>\nstream\n".to_vec();
        body.extend_from_slice(&compressed);
        body.extend_from_slice(b"\nendstream");
        let mut used = 0;
        let _ = pdf_object_stream(&body, 1000, &mut used);
        assert_eq!(used, 1000, "a 4 MiB bomb is cut at the budget");
        let _ = pdf_object_stream(&body, 0, &mut used);
        assert_eq!(used, 0);
        const { assert!(PDF_OBJSTM_TOTAL_LIMIT >= PDF_OBJSTM_LIMIT) };
    }

    #[test]
    fn mp4_file_with_moov_after_mdat_is_found_by_seeking() {
        let path = std::env::temp_dir().join(format!("herdr-meta-{}.mp4", std::process::id()));
        let mut bytes = bmff_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend_from_slice(&bmff_box(b"mdat", &vec![0u8; 100_000]));
        bytes.extend_from_slice(&bmff_box(b"moov", &video_moov()));
        std::fs::write(&path, &bytes).unwrap();
        let summary = probe(&path, "mp4", bytes.len() as u64).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary.kind, "MP4 · 1920×1080 · H.264");
        assert_eq!(summary.duration.as_deref(), Some("01:32"));
    }

    #[test]
    fn audio_only_mp4_reports_codec_rate_and_channels() {
        let mut esds = vec![0u8; 4];
        esds.extend_from_slice(&[0x03, 0x19, 0x00, 0x01, 0x00, 0x04, 0x11, 0x40]);
        let mut entry_body = vec![0u8; 28];
        entry_body[16..18].copy_from_slice(&2u16.to_be_bytes());
        entry_body[24..28].copy_from_slice(&(44_100u32 << 16).to_be_bytes());
        entry_body.extend_from_slice(&bmff_box(b"esds", &esds));
        let entry = bmff_box(b"mp4a", &entry_body);
        let mut stsd = vec![0u8; 8];
        stsd.extend_from_slice(&entry);
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"soun");
        let mut mdia = bmff_box(b"hdlr", &hdlr);
        mdia.extend_from_slice(&bmff_box(
            b"minf",
            &bmff_box(b"stbl", &bmff_box(b"stsd", &stsd)),
        ));
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&44_100u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&(225 * 44_100u32).to_be_bytes());
        let mut moov = bmff_box(b"mvhd", &mvhd);
        moov.extend_from_slice(&bmff_box(b"trak", &bmff_box(b"mdia", &mdia)));
        let summary = bmff_summary("M4A", Some(parse_moov(&moov)));
        assert_eq!(summary.kind, "M4A · AAC · 44.1 kHz · stereo");
        assert_eq!(summary.duration.as_deref(), Some("03:45"));
    }

    #[test]
    fn corrupt_media_headers_fall_back_without_panicking() {
        for moov in [Vec::new(), vec![0xFF; 64], bmff_box(b"mvhd", &[1u8; 3])] {
            let _ = bmff_summary("MP4", Some(parse_moov(&moov)));
        }
        assert_eq!(bmff_summary("MOV", None).kind, "MOV");
        let huge = {
            let mut mvhd = vec![0u8; 100];
            mvhd[12..16].copy_from_slice(&1u32.to_be_bytes());
            mvhd[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
            bmff_box(b"mvhd", &mvhd)
        };
        assert_eq!(
            bmff_summary("MP4", Some(parse_moov(&huge)))
                .duration
                .as_deref(),
            Some("-")
        );
        assert_eq!(parse_mp3_frames(&[]).kind, "MP3");
        assert_eq!(probe_flac(b"nope").kind, "FLAC");
        assert_eq!(probe_gif(b"GIF8"), None);
        assert_eq!(probe_bmp(b"BM"), None);
        assert_eq!(probe_webp(b"RIFF\0\0\0\0WEBPVP8 "), None);
    }

    #[test]
    fn flac_streaminfo_and_mp3_xing_give_exact_durations() {
        let mut flac = b"fLaC\0\0\0\x22".to_vec();
        let mut info = vec![0u8; 34];
        let packed: u64 = (44_100u64 << 44) | (1 << 41) | (15 << 36) | (44_100 * 225);
        info[10..18].copy_from_slice(&packed.to_be_bytes());
        flac.extend_from_slice(&info);
        let summary = probe_flac(&flac);
        assert_eq!(summary.kind, "FLAC · 44.1 kHz · stereo · 16-bit");
        assert_eq!(summary.duration.as_deref(), Some("03:45"));

        // MPEG1 Layer III, 128 kbps, 44.1 kHz, stereo + Xing with 1000 frames.
        let mut mp3 = vec![0xFF, 0xFB, 0x90, 0x00];
        mp3.extend_from_slice(&[0u8; 32]);
        mp3.extend_from_slice(b"Xing");
        mp3.extend_from_slice(&1u32.to_be_bytes());
        mp3.extend_from_slice(&1000u32.to_be_bytes());
        let summary = parse_mp3_frames(&mp3);
        assert_eq!(summary.kind, "MP3 · 44.1 kHz · stereo");
        assert_eq!(summary.duration.as_deref(), Some("00:26"));
        // Without a frame count the duration is not estimated.
        let summary = parse_mp3_frames(&[0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]);
        assert_eq!(summary.duration.as_deref(), Some("-"));
    }

    #[test]
    fn image_headers_give_dimensions() {
        let gif = b"GIF89a\x80\x07\x38\x04";
        assert_eq!(
            image_summary("GIF", probe_gif(gif)).kind,
            "GIF · 1920×1080 · Indexed"
        );
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0u8; 12]);
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&640i32.to_le_bytes());
        bmp.extend_from_slice(&(-480i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        assert_eq!(
            image_summary("BMP", probe_bmp(&bmp)).kind,
            "BMP · 640×480 · 24-bit"
        );
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend_from_slice(&[0u8; 4]);
        webp.extend_from_slice(&[0x10, 0, 0, 0]);
        webp.extend_from_slice(&[0x7F, 0x07, 0x00, 0x37, 0x04, 0x00]);
        assert_eq!(
            image_summary("WEBP", probe_webp(&webp)).kind,
            "WEBP · 1920×1080 · alpha"
        );
    }

    #[test]
    fn jpeg_sof_is_found_after_app_segments() {
        let path = std::env::temp_dir().join(format!("herdr-meta-{}.jpg", std::process::id()));
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10];
        bytes.extend_from_slice(&[0u8; 14]);
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 8, 0x04, 0x38, 0x07, 0x80, 3]);
        bytes.extend_from_slice(&[0u8; 9]);
        std::fs::write(&path, &bytes).unwrap();
        let summary = probe(&path, "jpg", bytes.len() as u64).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary.kind, "JPEG · 1920×1080 · YCbCr 8-bit");
    }

    #[test]
    fn text_scan_counts_lines_and_checks_utf8_across_chunks() {
        assert_eq!(scan_text(&b""[..]), (Some(0), true));
        assert_eq!(scan_text(&b"a\nb\n"[..]), (Some(2), true));
        assert_eq!(scan_text(&b"a\nb"[..]), (Some(2), true));
        assert_eq!(scan_text("日本語\n".as_bytes()), (Some(1), true));
        assert_eq!(scan_text(&[0x82u8, 0xA0, b'\n'][..]), (Some(1), false));
        // A multi-byte character split across the 64 KiB chunk boundary.
        let mut big = vec![b'a'; 64 * 1024 - 1];
        big.extend_from_slice("あ\n".as_bytes());
        assert_eq!(scan_text(&big[..]), (Some(1), true));
    }

    #[test]
    fn png_header_reports_color_mode_and_depth() {
        let path = std::env::temp_dir().join(format!("herdr-meta-{}.png", std::process::id()));
        {
            let file = File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 3, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0u8; 24]).unwrap();
        }
        let summary = probe(&path, "png", 0).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary.kind, "PNG · 3×2 · RGBA 8-bit");
    }
}
