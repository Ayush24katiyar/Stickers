use std::io::Cursor;

use anyhow::{anyhow, bail, Context, Result};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, Delay, Frame as ImageFrame, RgbaImage};
use webp_animation::{Encoder, EncoderOptions, EncodingConfig, EncodingType, LossyEncodingConfig};

use crate::webp_static;

pub const STICKER_SIZE: u32 = 512;
pub const MAX_BYTES_ANIMATED: usize = 500 * 1024;
pub const MAX_BYTES_STATIC: usize = 100 * 1024;
pub const MAX_DURATION_MS: u32 = 10_000;
pub const MIN_FRAME_MS: u32 = 10;

const STATIC_QUALITIES: [f32; 8] = [90.0, 80.0, 70.0, 60.0, 50.0, 40.0, 30.0, 20.0];

pub struct Sticker {
    pub bytes: Vec<u8>,
    pub frames: usize,
    pub duration_ms: u32,
    pub kind: &'static str,
    pub quality: u8,
    pub trimmed: bool,
}

struct Frame {
    rgba: RgbaImage,
    duration_ms: u32,
}

/// Decode any supported upload and turn it into a WhatsApp-compliant sticker.
pub fn convert(input: &[u8]) -> Result<Sticker> {
    if is_gif(input) {
        return convert_gif(input);
    }
    if is_animated_webp(input) {
        return convert_animated_webp(input);
    }
    if is_still_image(input) {
        return convert_still(input);
    }
    bail!("Unsupported file. Use a GIF, PNG, JPEG, animated WebP, or a short video.")
}

fn is_gif(input: &[u8]) -> bool {
    input.len() >= 6 && (&input[..6] == b"GIF87a" || &input[..6] == b"GIF89a")
}

/// Animated WebPs carry an ANIM chunk in the RIFF container (flagged in the
/// VP8X header as well). Walk the chunk table instead of trusting the header.
fn is_animated_webp(input: &[u8]) -> bool {
    if input.len() < 30 || !input.starts_with(b"RIFF") || &input[8..12] != b"WEBP" {
        return false;
    }

    let mut i = 12;
    while i + 8 <= input.len() {
        let id = &input[i..i + 4];
        let size = u32::from_le_bytes(input[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"ANIM" || id == b"ANMF" {
            return true;
        }
        i = i.saturating_add(8).saturating_add(size).saturating_add(size & 1);
    }
    false
}

fn is_still_image(input: &[u8]) -> bool {
    input.starts_with(&[0x89, b'P', b'N', b'G'])
        || input.starts_with(&[0xFF, 0xD8, 0xFF])
        || (input.len() > 12 && input.starts_with(b"RIFF") && &input[8..12] == b"WEBP")
}

/// Turn decoded animation frames into our padded, trimmed frame list.
fn delayed_frames(image_frames: Vec<image::Frame>) -> Vec<Frame> {
    image_frames
        .into_iter()
        .map(|frame| {
            let (numer, denom) = frame.delay().numer_denom_ms();
            let delay_ms = if denom == 0 {
                100
            } else {
                (numer / denom).max(MIN_FRAME_MS)
            };
            Frame {
                rgba: frame.into_buffer(),
                duration_ms: delay_ms,
            }
        })
        .collect()
}

/// Decode a GIF and turn it into an animated (or static) sticker.
pub fn convert_gif(input: &[u8]) -> Result<Sticker> {
    if !is_gif(input) {
        bail!("That file is not a GIF.");
    }

    let decoder = GifDecoder::new(Cursor::new(input)).context("Could not decode this GIF.")?;
    let gif_frames = decoder
        .into_frames()
        .collect_frames()
        .context("Could not read the frames of this GIF.")?;

    if gif_frames.is_empty() {
        bail!("This GIF has no frames.");
    }

    let frames = delayed_frames(gif_frames);

    let original_duration: u32 = frames.iter().map(|f| f.duration_ms).sum();
    let trimmed = original_duration > MAX_DURATION_MS;
    assemble(frames, trimmed)
}

/// Decode an animated WebP and turn it into an animated sticker.
pub fn convert_animated_webp(input: &[u8]) -> Result<Sticker> {
    let decoder =
        WebPDecoder::new(Cursor::new(input)).context("Could not decode this animated WebP.")?;

    let webp_frames = decoder
        .into_frames()
        .collect_frames()
        .context("Could not read the frames of this WebP.")?;

    if webp_frames.is_empty() {
        bail!("This WebP has no frames.");
    }

    let frames = delayed_frames(webp_frames);

    let original_duration: u32 = frames.iter().map(|f| f.duration_ms).sum();
    let trimmed = original_duration > MAX_DURATION_MS;
    assemble(frames, trimmed)
}

/// Turn any still image (PNG, JPEG, WebP) into a static sticker.
pub fn convert_still(input: &[u8]) -> Result<Sticker> {
    let image = image::load_from_memory(input).context("Could not decode this image.")?;
    let canvas = fit_on_canvas(&image.to_rgba8(), STICKER_SIZE);
    let (bytes, quality) = fit_static(canvas, MAX_BYTES_STATIC)?;

    Ok(Sticker {
        bytes,
        frames: 1,
        duration_ms: 0,
        kind: "static",
        quality,
        trimmed: false,
    })
}

/// Build an animated sticker from frames decoded elsewhere (e.g. a video in the browser).
pub fn convert_frames(frames: Vec<(RgbaImage, u32)>) -> Result<Sticker> {
    if frames.is_empty() {
        bail!("No frames were received.");
    }
    if frames.len() > 150 {
        bail!("That is too many frames. Try a shorter clip.");
    }

    let frames = frames
        .into_iter()
        .map(|(rgba, duration_ms)| Frame {
            rgba,
            duration_ms: duration_ms.max(MIN_FRAME_MS),
        })
        .collect();

    assemble(frames, false)
}

fn assemble(frames: Vec<Frame>, trimmed: bool) -> Result<Sticker> {
    // WhatsApp: no sticker longer than 10 seconds.
    let mut total = 0u32;
    let mut kept = Vec::with_capacity(frames.len());
    for mut frame in frames {
        if total >= MAX_DURATION_MS {
            break;
        }
        let remaining = MAX_DURATION_MS - total;
        frame.duration_ms = frame.duration_ms.min(remaining);
        if frame.duration_ms == 0 {
            break;
        }
        total += frame.duration_ms;
        kept.push(frame);
    }

    // Hundreds of frames are wasted on a chat sticker; collapse them until the
    // count is manageable.
    let mut frames = cap_frame_count(kept, 120);

    // Resize + pad every frame onto a transparent 512x512 canvas.
    for frame in &mut frames {
        frame.rgba = fit_on_canvas(&frame.rgba, STICKER_SIZE);
    }

    let animated = frames.len() > 1;
    let budget = if animated {
        MAX_BYTES_ANIMATED
    } else {
        MAX_BYTES_STATIC
    };

    let (bytes, quality) = if animated {
        fit_animated(frames, budget)?
    } else {
        fit_static(frames.remove(0).rgba, budget)?
    };

    // libwebp collapses runs of identical frames into a single still, so label
    // the result from the encoded file rather than from the input frame count.
    let frame_count = if animated {
        count_animated_frames(&bytes)
    } else {
        1
    };
    let animated = animated && frame_count > 1;

    Ok(Sticker {
        bytes,
        frames: frame_count,
        duration_ms: if animated { total } else { 0 },
        kind: if animated { "animated" } else { "static" },
        quality,
        trimmed,
    })
}

fn cap_frame_count(mut frames: Vec<Frame>, max_frames: usize) -> Vec<Frame> {
    while frames.len() > max_frames {
        let mut collapsed = Vec::with_capacity(frames.len() / 2 + 1);
        let mut iter = frames.into_iter();
        while let Some(mut frame) = iter.next() {
            if let Some(next) = iter.next() {
                frame.duration_ms = frame.duration_ms.saturating_add(next.duration_ms);
            }
            collapsed.push(frame);
        }
        frames = collapsed;
    }
    frames
}

/// Scale so the longest edge is `size`, then center on a transparent square.
fn fit_on_canvas(source: &RgbaImage, size: u32) -> RgbaImage {
    let (w, h) = source.dimensions();
    if w == 0 || h == 0 {
        return RgbaImage::new(size, size);
    }

    let scale = size as f32 / w.max(h) as f32;
    let new_w = ((w as f32 * scale).round() as u32).clamp(1, size);
    let new_h = ((h as f32 * scale).round() as u32).clamp(1, size);

    let scaled = image::imageops::resize(source, new_w, new_h, FilterType::Lanczos3);
    let mut canvas = RgbaImage::new(size, size);
    image::imageops::replace(
        &mut canvas,
        &scaled,
        ((size - new_w) / 2) as i64,
        ((size - new_h) / 2) as i64,
    );
    canvas
}

fn encode_animated(frames: &[Frame], quality: f32) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_with_options(
        (STICKER_SIZE, STICKER_SIZE),
        EncoderOptions {
            kmin: 3,
            kmax: 5,
            encoding_config: Some(EncodingConfig {
                quality,
                // Method 3 is the sweet spot here: animated stickers re-encode
                // the whole run for every quality step, and the source is
                // usually already smaller than 512 px.
                method: 3,
                encoding_type: EncodingType::Lossy(LossyEncodingConfig {
                    alpha_compression: true,
                    alpha_quality: 100,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .context("WebP encoder could not start.")?;

    let mut timestamp_ms: i32 = 0;
    for frame in frames {
        encoder
            .add_frame(frame.rgba.as_raw(), timestamp_ms)
            .context("WebP encoder rejected a frame.")?;
        timestamp_ms += frame.duration_ms as i32;
    }

    let data = encoder
        .finalize(timestamp_ms)
        .context("WebP encoding failed.")?;
    Ok(data.to_vec())
}

/// Walk a descending quality ladder at a fixed frame count. The first rung
/// that fits is the highest usable quality, so we stop there. `best` tracks
/// the smallest encode seen so a later frame reduction can fall back to it.
fn try_quality_ladder(
    frames: &[Frame],
    budget: usize,
    best: &mut Option<(Vec<u8>, u8)>,
) -> Result<Option<(Vec<u8>, u8)>> {
    // A long animation can't fit the budget at very high quality, so skip
    // those rungs instead of paying for a full encode just to prove it. Short
    // ones keep them to squeeze out the best quality available.
    let rungs: &[u8] = if frames.len() >= 40 {
        &[60, 50, 45, 40, 35, 30, 25, 20, 15, 10]
    } else {
        &[80, 70, 60, 52, 45, 40, 35, 30, 25, 20, 15, 10]
    };
    for &quality in rungs {
        let bytes = encode_animated(frames, quality as f32)?;
        if best
            .as_ref()
            .map_or(true, |(current, _)| bytes.len() < current.len())
        {
            *best = Some((bytes.clone(), quality));
        }
        if bytes.len() <= budget {
            return Ok(Some((bytes, quality)));
        }
    }
    Ok(None)
}

/// Find the best quality that fits the byte budget while keeping every frame.
/// Dropping frames is a last resort: only when even the lowest quality rung
/// overflows do we halve the count and retry the ladder. This preserves far
/// more of the source animation than bailing on quality after a few steps.
fn fit_animated(mut frames: Vec<Frame>, budget: usize) -> Result<(Vec<u8>, u8)> {
    let mut best: Option<(Vec<u8>, u8)> = None;

    if let Some(result) = try_quality_ladder(&frames, budget, &mut best)? {
        return Ok(result);
    }

    // Even quality 10 is too heavy: trade frame rate for size, keeping the
    // best (smallest) result from every round as a fallback.
    while frames.len() > 2 {
        let next_len = frames.len() / 2;
        frames = cap_frame_count(frames, next_len);
        if let Some(result) = try_quality_ladder(&frames, budget, &mut best)? {
            return Ok(result);
        }
    }

    let (bytes, quality) = best.ok_or_else(|| anyhow!("WebP encoding produced no output."))?;
    Ok((bytes, quality))
}

fn fit_static(rgba: RgbaImage, budget: usize) -> Result<(Vec<u8>, u8)> {
    let mut best: Option<(Vec<u8>, u8)> = None;
    for quality in STATIC_QUALITIES {
        let bytes = webp_static::encode(&rgba, quality)?;
        let better = best.as_ref().map_or(true, |(current, _)| bytes.len() < current.len());
        if better {
            best = Some((bytes.clone(), quality as u8));
        }
        if bytes.len() <= budget {
            return Ok((bytes, quality as u8));
        }
    }

    let (bytes, quality) = best.ok_or_else(|| anyhow!("WebP encoding produced no output."))?;
    Ok((bytes, quality))
}

/// Count ANMF chunks in an animated WebP so the UI can show a real frame count.
fn count_animated_frames(webp: &[u8]) -> usize {
    if webp.len() < 12 {
        return 1;
    }
    let mut count = 0;
    let mut i = 12;
    while i + 8 <= webp.len() {
        let chunk = &webp[i..i + 4];
        let size = u32::from_le_bytes(webp[i + 4..i + 8].try_into().unwrap()) as usize;
        if chunk == b"ANMF" {
            count += 1;
        }
        i = i.saturating_add(8).saturating_add(size).saturating_add(size & 1);
    }
    count.max(1)
}

/// Re-encode an animated WebP as a GIF. WhatsApp Web's "Create sticker" tool
/// only keeps the first frame of any file you give it, but a lot of animated
/// sticker-pack makers accept GIF (and some refuse animated WebP), so this
/// gives people a working route into those apps. GIF is paletted, so frames
/// are downscaled to keep the export a practical size to upload.
pub fn webp_to_gif(bytes: &[u8]) -> Result<Vec<u8>> {
    const SIDE: u32 = 256;

    let decoder =
        WebPDecoder::new(Cursor::new(bytes)).context("Could not read the sticker animation.")?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .context("Could not read the sticker frames.")?;
    if frames.len() < 2 {
        bail!("This sticker has no animation to export.");
    }

    let mut out = Vec::new();
    {
        let mut encoder = GifEncoder::new_with_speed(&mut out, 5);
        encoder
            .set_repeat(Repeat::Infinite)
            .context("Could not start the GIF encoder.")?;

        for frame in frames {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let duration_ms = if denominator == 0 {
                80
            } else {
                (numerator / denominator).max(20)
            };
            let scaled = image::imageops::resize(
                &frame.into_buffer(),
                SIDE,
                SIDE,
                FilterType::Lanczos3,
            );
            encoder
                .encode_frame(ImageFrame::from_parts(
                    scaled,
                    0,
                    0,
                    Delay::from_numer_denom_ms(duration_ms, 1),
                ))
                .context("Could not encode a GIF frame.")?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Frame as GifFrame;
    use image::Rgba;

    fn sample_gif(frames: usize, width: u32, height: u32, delay_ms: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for index in 0..frames {
                let mut image = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 0]));
                let shift = (index as u32 * 4) % (width / 2).max(1);
                for y in height / 4..height * 3 / 4 {
                    for x in shift..(shift + width / 3).min(width) {
                        image.put_pixel(x, y, Rgba([37, 211, 102, 255]));
                    }
                }
                let frame = GifFrame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
                encoder.encode_frame(frame).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn converts_animated_gif() {
        let gif = sample_gif(12, 160, 120, 80);
        let sticker = convert_gif(&gif).unwrap();

        assert_eq!(sticker.kind, "animated");
        assert_eq!(sticker.frames, 12);
        assert_eq!(&sticker.bytes[..4], b"RIFF");
        assert_eq!(&sticker.bytes[8..12], b"WEBP");
        assert!(sticker.bytes.len() <= MAX_BYTES_ANIMATED, "{} bytes", sticker.bytes.len());

        let (width, height, flags) = vp8x(&sticker.bytes);
        assert_eq!((width, height), (STICKER_SIZE, STICKER_SIZE));
        assert_eq!(flags & 0x02, 0x02, "animated bit should be set");
    }

    #[test]
    fn converts_animated_webp_to_animated_sticker() {
        let frames: Vec<Frame> = (0..5u8)
            .map(|index| Frame {
                rgba: RgbaImage::from_pixel(512, 512, Rgba([index * 50, 100, 200, 255])),
                duration_ms: 90,
            })
            .collect();

        // Round-trip through our own animated encoder to get a real animated WebP.
        let webp = encode_animated(&frames, 80.0).unwrap();
        assert!(is_animated_webp(&webp));

        let sticker = convert(&webp).unwrap();
        assert_eq!(sticker.kind, "animated");
        assert_eq!(sticker.frames, 5);
        assert!(sticker.bytes.len() <= MAX_BYTES_ANIMATED, "{} bytes", sticker.bytes.len());
        let (width, height, flags) = vp8x(&sticker.bytes);
        assert_eq!((width, height), (STICKER_SIZE, STICKER_SIZE));
        assert_eq!(flags & 0x02, 0x02, "animated bit should be set");
    }

    #[test]
    fn animated_webp_is_not_treated_as_still() {
        let frames: Vec<Frame> = (0..3u8)
            .map(|index| Frame {
                rgba: RgbaImage::from_pixel(512, 512, Rgba([200, index * 80, 40, 255])),
                duration_ms: 120,
            })
            .collect();

        let webp = encode_animated(&frames, 80.0).unwrap();
        let sticker = convert(&webp).unwrap();

        // If the file were routed through the still-image path we would get one frame.
        assert_eq!(sticker.frames, 3);
        assert_eq!(sticker.kind, "animated");
    }

    #[test]
    fn converts_static_gif_under_100kb() {
        let gif = sample_gif(1, 160, 120, 100);
        let sticker = convert_gif(&gif).unwrap();

        assert_eq!(sticker.kind, "static");
        assert_eq!(sticker.frames, 1);
        assert!(sticker.bytes.len() <= MAX_BYTES_STATIC, "{} bytes", sticker.bytes.len());
        assert!(!sticker.trimmed);
        // A static sticker is a still WebP: no animation chunk and no animated flag.
        assert!(!sticker.bytes.windows(4).any(|window| window == b"ANIM"));
        if &sticker.bytes[12..16] == b"VP8X" {
            assert_eq!(sticker.bytes[20] & 0x02, 0, "animated bit must not be set");
        }
    }

    /// Read canvas size and flags from the VP8X chunk of an extended WebP file.
    fn vp8x(webp: &[u8]) -> (u32, u32, u8) {
        assert!(webp.len() >= 30, "file is too short for VP8X");
        assert_eq!(&webp[12..16], b"VP8X");
        let width = 1 + u32::from_le_bytes([webp[24], webp[25], webp[26], 0]);
        let height = 1 + u32::from_le_bytes([webp[27], webp[28], webp[29], 0]);
        (width, height, webp[20])
    }

    #[test]
    fn trims_to_ten_seconds() {
        let gif = sample_gif(30, 120, 90, 500); // 15 seconds
        let sticker = convert_gif(&gif).unwrap();

        assert!(sticker.trimmed);
        assert_eq!(sticker.duration_ms, MAX_DURATION_MS);
    }

    #[test]
    fn converts_png_to_static_sticker() {
        let image = RgbaImage::from_fn(300, 220, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, 120, if x < 40 { 0 } else { 255 }])
        });

        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let sticker = convert(&png).unwrap();
        assert_eq!(sticker.kind, "static");
        assert_eq!(sticker.frames, 1);
        assert!(sticker.bytes.len() <= MAX_BYTES_STATIC, "{} bytes", sticker.bytes.len());
    }

    #[test]
    fn converts_sampled_video_frames() {
        let frames = (0..6u8)
            .map(|index| {
                (
                    RgbaImage::from_pixel(320, 180, Rgba([index * 30, 180, 90, 255])),
                    83,
                )
            })
            .collect();

        let sticker = convert_frames(frames).unwrap();
        assert_eq!(sticker.kind, "animated");
        assert_eq!(sticker.frames, 6);
        assert_eq!(sticker.duration_ms, 6 * 83);
    }

    #[test]
    fn identical_frames_become_a_static_sticker() {
        let frames = (0..8)
            .map(|_| (RgbaImage::from_pixel(220, 220, Rgba([37, 211, 102, 255])), 100))
            .collect();

        let sticker = convert_frames(frames).unwrap();
        assert_eq!(sticker.kind, "static");
        assert_eq!(sticker.frames, 1);
        assert_eq!(sticker.duration_ms, 0);
    }

    #[test]
    fn rejects_non_gif_data() {
        assert!(convert(b"definitely not a gif").is_err());
    }
}
