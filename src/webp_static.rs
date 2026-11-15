use std::ffi::c_int;

use anyhow::{bail, Result};
use image::RgbaImage;
use libwebp_sys::{WebPEncodeRGBA, WebPFree};

/// Encode a single frame as a lossy static WebP with libwebp.
pub fn encode(image: &RgbaImage, quality: f32) -> Result<Vec<u8>> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        bail!("Cannot encode an empty image.");
    }

    let mut output: *mut u8 = std::ptr::null_mut();
    let size = unsafe {
        WebPEncodeRGBA(
            image.as_raw().as_ptr(),
            width as c_int,
            height as c_int,
            (width * 4) as c_int,
            quality,
            &mut output,
        )
    };

    if size == 0 || output.is_null() {
        bail!("libwebp could not encode this sticker.");
    }

    let bytes = unsafe { std::slice::from_raw_parts(output, size).to_vec() };
    unsafe { WebPFree(output.cast()) };
    Ok(bytes)
}
