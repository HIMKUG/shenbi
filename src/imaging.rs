use crate::error::AppError;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use image::imageops::FilterType;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Operation {
    Resize {
        width: Option<u32>,
        height: Option<u32>,
        #[serde(default)]
        fit: Option<Fit>,
        #[serde(default, rename = "withoutEnlargement")]
        without_enlargement: bool,
    },
    Crop {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    AspectCrop {
        ratio: String,
        #[serde(default)]
        gravity: Option<Gravity>,
    },
    CircleMask,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    Cover,
    Contain,
    Fill,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Gravity {
    Center,
    North,
    South,
    East,
    West,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub channels: u8,
    #[serde(rename = "hasAlpha")]
    pub has_alpha: bool,
}

pub struct Processed {
    pub bytes: Vec<u8>,
    pub original: ImageInfo,
    pub output: ImageInfo,
    pub applied: Vec<String>,
}

pub fn decode(bytes: &[u8]) -> Result<DynamicImage, AppError> {
    image::load_from_memory(bytes).map_err(|err| {
        AppError::unsupported(format!("unsupported or corrupt image input: {err}"))
    })
}

pub fn info(bytes: &[u8]) -> Result<ImageInfo, AppError> {
    let format = image::guess_format(bytes)
        .map(|f| format!("{f:?}").to_lowercase())
        .unwrap_or_else(|_| "unknown".into());
    let image = decode(bytes)?;
    Ok(ImageInfo {
        width: image.width(),
        height: image.height(),
        format,
        channels: if image.color().has_alpha() { 4 } else { 3 },
        has_alpha: image.color().has_alpha(),
    })
}

pub fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, AppError> {
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|err| AppError::provider(format!("PNG encode failed: {err}"), false))?;
    Ok(out.into_inner())
}

pub fn apply(bytes: &[u8], operations: &[Operation]) -> Result<Processed, AppError> {
    if operations.len() > 16 {
        return Err(AppError::constraint(format!(
            "operation chain exceeds 16 steps (got {})",
            operations.len()
        )));
    }
    let mut current = decode(bytes)?;
    let original = image_info_of(&current, bytes);
    let mut applied = Vec::new();

    for (index, op) in operations.iter().enumerate() {
        let step = run(&current, op).map_err(|err| {
            AppError::new(
                err.code,
                format!("operation {}/{} ({}) failed: {}", index + 1, operations.len(), op_name(op), err.message),
                err.retryable,
            )
        })?;
        applied.push(describe(op));
        current = step;
    }

    let output = image_info_of(&current, &[]);
    Ok(Processed { bytes: encode_png(&current)?, original, output, applied })
}

fn image_info_of(image: &DynamicImage, raw: &[u8]) -> ImageInfo {
    let format = if raw.is_empty() {
        "png".to_string()
    } else {
        image::guess_format(raw).map(|f| format!("{f:?}").to_lowercase()).unwrap_or_else(|_| "unknown".into())
    };
    ImageInfo {
        width: image.width(),
        height: image.height(),
        format,
        channels: if image.color().has_alpha() { 4 } else { 3 },
        has_alpha: image.color().has_alpha(),
    }
}

fn op_name(op: &Operation) -> &'static str {
    match op {
        Operation::Resize { .. } => "resize",
        Operation::Crop { .. } => "crop",
        Operation::AspectCrop { .. } => "aspectCrop",
        Operation::CircleMask => "circleMask",
    }
}

fn describe(op: &Operation) -> String {
    match op {
        Operation::Resize { width, height, fit, .. } => format!(
            "resize({}x{}, {})",
            width.map(|w| w.to_string()).unwrap_or_else(|| "auto".into()),
            height.map(|h| h.to_string()).unwrap_or_else(|| "auto".into()),
            match fit.unwrap_or(Fit::Cover) {
                Fit::Cover => "cover",
                Fit::Contain => "contain",
                Fit::Fill => "fill",
            }
        ),
        Operation::Crop { x, y, width, height } => format!("crop({x},{y},{width}x{height})"),
        Operation::AspectCrop { ratio, gravity } => format!(
            "aspectCrop({ratio}, {})",
            match gravity.unwrap_or(Gravity::Center) {
                Gravity::Center => "center",
                Gravity::North => "north",
                Gravity::South => "south",
                Gravity::East => "east",
                Gravity::West => "west",
            }
        ),
        Operation::CircleMask => "circleMask".into(),
    }
}

fn run(image: &DynamicImage, op: &Operation) -> Result<DynamicImage, AppError> {
    match op {
        Operation::Resize { width, height, fit, without_enlargement } => {
            resize(image, *width, *height, fit.unwrap_or(Fit::Cover), *without_enlargement)
        }
        Operation::Crop { x, y, width, height } => crop(image, *x, *y, *width, *height),
        Operation::AspectCrop { ratio, gravity } => aspect_crop(image, ratio, gravity.unwrap_or(Gravity::Center)),
        Operation::CircleMask => circle_mask(image),
    }
}

fn resize(
    image: &DynamicImage,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    without_enlargement: bool,
) -> Result<DynamicImage, AppError> {
    let (sw, sh) = image.dimensions();
    let tw = width.unwrap_or(0);
    let th = height.unwrap_or(0);
    if tw == 0 && th == 0 {
        return Err(AppError::constraint("resize requires width or height"));
    }

    let (mut target_w, mut target_h) = match (tw, th) {
        (0, h) => {
            let scale = h as f32 / sh as f32;
            (((sw as f32) * scale).round() as u32, h)
        }
        (w, 0) => {
            let scale = w as f32 / sw as f32;
            (w, ((sh as f32) * scale).round() as u32)
        }
        (w, h) => (w, h),
    };
    target_w = target_w.max(1);
    target_h = target_h.max(1);

    if without_enlargement && (target_w > sw || target_h > sh) {
        return Ok(image.clone());
    }

    let mut out = match fit {
        Fit::Fill => image.resize_exact(target_w, target_h, FilterType::Lanczos3),
        Fit::Cover | Fit::Contain => {
            let scale = match fit {
                Fit::Cover => (target_w as f32 / sw as f32).max(target_h as f32 / sh as f32),
                _ => (target_w as f32 / sw as f32).min(target_h as f32 / sh as f32),
            };
            let scaled = image.resize(
                ((sw as f32) * scale).round().max(1.0) as u32,
                ((sh as f32) * scale).round().max(1.0) as u32,
                FilterType::Lanczos3,
            );
            if matches!(fit, Fit::Cover) {
                let (cw, ch) = scaled.dimensions();
                scaled.crop_imm((cw.saturating_sub(target_w)) / 2, (ch.saturating_sub(target_h)) / 2, target_w.min(cw), target_h.min(ch))
            } else {
                let mut canvas = RgbaImage::new(target_w, target_h);
                let (cw, ch) = scaled.dimensions();
                let left = (target_w.saturating_sub(cw)) / 2;
                let top = (target_h.saturating_sub(ch)) / 2;
                image::imageops::overlay(&mut canvas, &scaled.to_rgba8(), left as i64, top as i64);
                DynamicImage::ImageRgba8(canvas)
            }
        }
    };
    if out.width() != target_w || out.height() != target_h {
        out = out.resize_exact(target_w, target_h, FilterType::Lanczos3);
    }
    Ok(out)
}

fn crop(image: &DynamicImage, x: u32, y: u32, width: u32, height: u32) -> Result<DynamicImage, AppError> {
    let (sw, sh) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(AppError::constraint("crop width and height must be positive"));
    }
    if x + width > sw || y + height > sh {
        return Err(AppError::constraint(format!(
            "crop region ({x},{y} {width}x{height}) exceeds source dimensions ({sw}x{sh})"
        )));
    }
    Ok(image.crop_imm(x, y, width, height))
}

pub fn aspect_crop(image: &DynamicImage, ratio: &str, gravity: Gravity) -> Result<DynamicImage, AppError> {
    let (rw, rh) = ratio
        .split_once(':')
        .and_then(|(a, b)| Some((a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?)))
        .filter(|(a, b)| *a > 0.0 && *b > 0.0)
        .ok_or_else(|| AppError::constraint(format!("invalid aspect ratio '{ratio}', expected e.g. 16:9")))?;
    let target = rw / rh;
    let (sw, sh) = image.dimensions();
    let source = sw as f32 / sh as f32;

    let (cw, ch, left, top) = if source > target {
        let ch = sh;
        let cw = ((ch as f32) * target).round() as u32;
        let cw = cw.min(sw);
        let left = match gravity {
            Gravity::East => sw - cw,
            Gravity::West => 0,
            _ => (sw - cw) / 2,
        };
        (cw, ch, left, 0)
    } else {
        let cw = sw;
        let ch = ((cw as f32) / target).round() as u32;
        let ch = ch.min(sh);
        let top = match gravity {
            Gravity::North => 0,
            Gravity::South => sh - ch,
            _ => (sh - ch) / 2,
        };
        (cw, ch, 0, top)
    };

    Ok(image.crop_imm(left, top, cw.max(1), ch.max(1)))
}

pub fn circle_mask(image: &DynamicImage) -> Result<DynamicImage, AppError> {
    let (sw, sh) = image.dimensions();
    let diameter = sw.min(sh).max(1);
    // Center-crop before masking so non-square inputs are not stretched.
    let square = aspect_crop(image, "1:1", Gravity::Center)?
        .resize_exact(diameter, diameter, FilterType::Lanczos3)
        .to_rgba8();
    let mut out = RgbaImage::new(diameter, diameter);
    let radius = diameter as f32 / 2.0;
    let center = radius;

    for y in 0..diameter {
        for x in 0..diameter {
            let px = square.get_pixel(x, y);
            let dx = (x as f32 + 0.5) - center;
            let dy = (y as f32 + 0.5) - center;
            let dist = (dx * dx + dy * dy).sqrt();
            // 1px anti-aliased edge so small avatars do not look jagged.
            let coverage = (radius - dist + 0.5).clamp(0.0, 1.0);
            let alpha = (px[3] as f32 * coverage).round() as u8;
            out.put_pixel(x, y, Rgba([px[0], px[1], px[2], alpha]));
        }
    }
    Ok(DynamicImage::ImageRgba8(out))
}

pub fn palette(bytes: &[u8], count: usize, max_count: usize) -> Result<Vec<serde_json::Value>, AppError> {
    if count == 0 || count > max_count {
        return Err(AppError::constraint(format!("palette count must be 1-{max_count}")));
    }
    let small = decode(bytes)?.resize(64, 64, FilterType::Triangle).to_rgba8();
    let mut buckets: std::collections::HashMap<u32, (u64, u64, u64, u64)> = std::collections::HashMap::new();
    for pixel in small.pixels() {
        if pixel[3] < 8 {
            continue;
        }
        let key = ((pixel[0] as u32 >> 3) << 10) | ((pixel[1] as u32 >> 3) << 5) | (pixel[2] as u32 >> 3);
        let entry = buckets.entry(key).or_insert((0, 0, 0, 0));
        entry.0 += pixel[0] as u64;
        entry.1 += pixel[1] as u64;
        entry.2 += pixel[2] as u64;
        entry.3 += 1;
    }
    let total: u64 = buckets.values().map(|v| v.3).sum();
    let mut colors: Vec<_> = buckets
        .into_values()
        .map(|(r, g, b, n)| {
            (
                (r / n) as u8,
                (g / n) as u8,
                (b / n) as u8,
                if total == 0 { 0.0 } else { n as f64 / total as f64 },
            )
        })
        .collect();
    colors.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    colors.truncate(count);
    Ok(colors
        .into_iter()
        .map(|(r, g, b, weight)| {
            serde_json::json!({
                "hex": format!("#{r:02x}{g:02x}{b:02x}"),
                "r": r, "g": g, "b": b, "weight": weight
            })
        })
        .collect())
}

pub fn composite(
    canvas_w: u32,
    canvas_h: u32,
    background: [u8; 4],
    layers: &[CompositeLayer],
) -> Result<Vec<u8>, AppError> {
    if canvas_w == 0 || canvas_h == 0 {
        return Err(AppError::constraint("canvas dimensions must be positive"));
    }
    if canvas_w as u64 * canvas_h as u64 > 16_000_000 {
        return Err(AppError::new(
            crate::error::ErrorCode::InputTooLarge,
            "canvas exceeds the 16MP cap",
            false,
        ));
    }
    if layers.is_empty() || layers.len() > 16 {
        return Err(AppError::constraint("composite requires 1-16 layers"));
    }

    let mut out = RgbaImage::from_pixel(canvas_w, canvas_h, Rgba(background));
    for layer in layers {
        let raw = std::fs::read(&layer.input)?;
        let mut overlay = decode(&raw)?.to_rgba8();
        let (lw, lh) = overlay.dimensions();
        let scale = layer.scale.unwrap_or(1.0);
        if !(0.05..=10.0).contains(&scale) {
            return Err(AppError::constraint("layer scale must be between 0.05 and 10"));
        }
        let opacity = layer.opacity.unwrap_or(1.0);
        if !(0.0..=1.0).contains(&opacity) {
            return Err(AppError::constraint("layer opacity must be between 0 and 1"));
        }
        if scale != 1.0 {
            let w = ((lw as f32 * scale).round() as u32).max(1);
            let h = ((lh as f32 * scale).round() as u32).max(1);
            overlay = image::imageops::resize(&overlay, w, h, FilterType::Lanczos3);
        }
        if opacity < 1.0 {
            for pixel in overlay.pixels_mut() {
                pixel[3] = (pixel[3] as f32 * opacity).round() as u8;
            }
        }
        let (ow, oh) = overlay.dimensions();
        let x = layer.x.unwrap_or(0.0);
        let y = layer.y.unwrap_or(0.0);
        let (left, top) = match layer.anchor.unwrap_or(Anchor::TopLeft) {
            Anchor::TopLeft => (x, y),
            Anchor::Center => (x - ow as f32 / 2.0, y - oh as f32 / 2.0),
            Anchor::TopRight => (x - ow as f32, y),
            Anchor::BottomLeft => (x, y - oh as f32),
            Anchor::BottomRight => (x - ow as f32, y - oh as f32),
        };
        image::imageops::overlay(&mut out, &overlay, left.round() as i64, top.round() as i64);
    }
    encode_png(&DynamicImage::ImageRgba8(out))
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub enum Anchor {
    #[serde(rename = "top-left")]
    TopLeft,
    #[serde(rename = "center")]
    Center,
    #[serde(rename = "top-right")]
    TopRight,
    #[serde(rename = "bottom-left")]
    BottomLeft,
    #[serde(rename = "bottom-right")]
    BottomRight,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompositeLayer {
    pub input: String,
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub scale: Option<f32>,
    #[serde(default)]
    pub opacity: Option<f32>,
    #[serde(default)]
    pub anchor: Option<Anchor>,
}
