//! Turning images and PDFs into ZPL, and fetching files by URL, for BrowserPrint's
//! `convertAndSendFile`, `convert`, `sendUrl` and `sendFile`.
//!
//! Images become one ^GFA graphic per label, black where the picture is darker than
//! mid-grey. PDF pages are rendered with `pdftoppm` (poppler-utils) first.

use std::process::{Command, Stdio};
use std::time::Duration;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, GrayImage, Luma};
use serde::Deserialize;

/// Largest file we fetch or convert.
pub const MAX_FILE: u64 = 32 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DPI: u32 = 203;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Image,
    /// ZPL or anything else the printer understands as is.
    Raw,
}

/// What a file is, from its first bytes.
pub fn sniff(data: &[u8]) -> Kind {
    const IMAGE_MAGIC: &[&[u8]] = &[b"\x89PNG", b"\xFF\xD8\xFF", b"GIF8", b"BM"];
    if data.starts_with(b"%PDF") {
        Kind::Pdf
    } else if IMAGE_MAGIC.iter().any(|m| data.starts_with(m)) {
        Kind::Image
    } else {
        Kind::Raw
    }
}

/// BrowserPrint conversion options (the parts we use).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Options {
    /// "print" (default) sends the result to the printer; "return" hands it back.
    pub action: Option<String>,
    pub to_format: Option<String>,
    /// Largest size in printer dots; the picture keeps its shape.
    pub fit_to: Option<FitTo>,
    /// Resolution for PDF pages; defaults to 203 dpi (8 dots/mm).
    pub dpi: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct FitTo {
    pub width: u32,
    pub height: u32,
}

/// Turn an image or PDF into ZPL labels (one per image or page).
pub fn to_zpl(data: &[u8], opts: &Options) -> Result<String, String> {
    if let Some(format) = opts.to_format.as_deref() {
        if !format.eq_ignore_ascii_case("zpl") {
            return Err(format!("converting to {format} is not supported; use toFormat \"zpl\""));
        }
    }
    let pages = match sniff(data) {
        Kind::Pdf => pdf_pages(data, opts.dpi.unwrap_or(DEFAULT_DPI))?,
        Kind::Image => vec![image::load_from_memory(data).map_err(|e| format!("can't read the image: {e}"))?],
        Kind::Raw => return Err("not an image or PDF (PNG, JPEG, GIF, BMP or PDF expected)".into()),
    };
    Ok(pages.iter().map(|page| label(page, opts.fit_to)).collect())
}

/// One label with the picture at the top left.
fn label(img: &DynamicImage, fit: Option<FitTo>) -> String {
    let img = match fit {
        Some(f) if f.width > 0 && f.height > 0 => img.resize(f.width, f.height, FilterType::Triangle),
        _ => img.clone(),
    };
    let (width, height) = img.dimensions();
    let bytes_per_row = width.div_ceil(8);
    let gray = on_white(&img);
    let mut hex = String::with_capacity((bytes_per_row * height * 2) as usize);
    for y in 0..height {
        for chunk in 0..bytes_per_row {
            let mut byte = 0u8;
            for bit in 0..8 {
                let x = chunk * 8 + bit;
                if x < width && gray.get_pixel(x, y).0[0] < 128 {
                    byte |= 0x80 >> bit;
                }
            }
            hex.push_str(&format!("{byte:02X}"));
        }
    }
    let total = bytes_per_row * height;
    format!("^XA^FO0,0^GFA,{total},{total},{bytes_per_row},{hex}^FS^XZ\n")
}

/// Grey levels with transparent parts treated as white paper.
fn on_white(img: &DynamicImage) -> GrayImage {
    let rgba = img.to_rgba8();
    GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let luma = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
        let alpha = a as f32 / 255.0;
        Luma([((luma * alpha + (1.0 - alpha)) * 255.0).round() as u8])
    })
}

/// Render every PDF page with pdftoppm.
fn pdf_pages(data: &[u8], dpi: u32) -> Result<Vec<DynamicImage>, String> {
    let dir = std::env::temp_dir().join(format!("zprint-pdf-{}-{}", std::process::id(), unique()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't make a temporary folder: {e}"))?;
    let result = (|| {
        let pdf = dir.join("in.pdf");
        std::fs::write(&pdf, data).map_err(|e| format!("can't write the PDF: {e}"))?;
        let status = Command::new("pdftoppm")
            .args(["-png", "-r", &dpi.clamp(72, 600).to_string()])
            .arg(&pdf)
            .arg(dir.join("page"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "printing PDFs needs pdftoppm (sudo apt install poppler-utils)".to_owned()
                }
                _ => format!("can't run pdftoppm: {e}"),
            })?;
        if !status.success() {
            return Err("pdftoppm could not read the PDF".to_owned());
        }
        let mut pages: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "png"))
            .collect();
        // page-1.png, page-2.png ... page-10.png: sort by number, not text.
        pages.sort_by_key(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit('-').next())
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0)
        });
        pages
            .iter()
            .map(|p| image::open(p).map_err(|e| format!("can't read a rendered page: {e}")))
            .collect()
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Download a file for sendUrl (http/https, at most MAX_FILE).
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http:// and https:// addresses can be printed".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|e| format!("can't download {url}: {e}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_FILE)
        .read_to_vec()
        .map_err(|e| format!("can't download {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn png(img: RgbaImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn sniffs_files() {
        assert_eq!(sniff(b"%PDF-1.7"), Kind::Pdf);
        assert_eq!(sniff(b"\x89PNG\r\n"), Kind::Image);
        assert_eq!(sniff(b"^XA^XZ"), Kind::Raw);
    }

    #[test]
    fn image_becomes_gfa() {
        // 10x2: left 4 columns black, rest white, a transparent pixel counts as white.
        let mut img = RgbaImage::from_pixel(10, 2, Rgba([255, 255, 255, 255]));
        for y in 0..2 {
            for x in 0..4 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        img.put_pixel(0, 1, Rgba([0, 0, 0, 0]));
        let zpl = to_zpl(&png(img), &Options::default()).unwrap();
        // 2 bytes per row, 2 rows: F0 00 / 70 00
        assert_eq!(zpl, "^XA^FO0,0^GFA,4,4,2,F00070 00^FS^XZ\n".replace(' ', ""));
    }

    #[test]
    fn fit_to_keeps_shape() {
        let img = RgbaImage::from_pixel(400, 200, Rgba([0, 0, 0, 255]));
        let opts = Options { fit_to: Some(FitTo { width: 100, height: 100 }), ..Options::default() };
        let zpl = to_zpl(&png(img), &opts).unwrap();
        // 100x50 dots: 13 bytes per row, 650 bytes.
        assert!(zpl.starts_with("^XA^FO0,0^GFA,650,650,13,"), "{}", &zpl[..40]);
    }

    #[test]
    fn rejects_other_formats() {
        assert!(to_zpl(b"^XA^XZ", &Options::default()).is_err());
        let opts = Options { to_format: Some("cpcl".into()), ..Options::default() };
        assert!(to_zpl(b"\x89PNG", &opts).is_err());
    }
}
