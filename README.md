# sticker

Turn a GIF, image, or video into a WhatsApp sticker. Rust does the conversion; the frontend is plain HTML, CSS and JS.

```
GIF / image / video  →  512×512 canvas  →  WebP  →  static ≤100 KB · animated ≤500 KB
```

Everything runs locally. There is no third-party conversion service: decoding, resizing, padding and WebP encoding all happen on your machine. libwebp is compiled into the binary; the only network calls are downloading a link you paste (and X's public lookup for finding the media behind a post).

## Run it

```powershell
cargo run --release
```

Then open <http://localhost:3000>.

Cargo builds libwebp from source, so on Windows you need the MSVC build tools (the same ones Rust for Windows already uses). No ffmpeg, ImageMagick or system libwebp required.

Set a different port with `PORT=8080 cargo run --release`.

## How to use

- **Paste link** — works with direct `https://...gif` / image / video links, and with X/Twitter post links. X stores "GIFs" as MP4, so those are resolved to the underlying media automatically.
- **Upload file** — pick or drag a GIF, PNG, JPEG, WebP, MP4, WebM or MOV (20 MB image cap; local videos up to 250 MB), or press `Ctrl/Cmd+V` after copying something. For videos longer than 10 seconds a preview appears with a start slider — drag to keep the part you want.
- Preview the original and the sticker side by side, check the size badge, then download the `.webp`.

To actually use it in WhatsApp, import the downloaded file with a sticker-maker app (Sticker Maker, Sticker.ly, etc.). WhatsApp has no web API for adding a single sticker directly from a browser.

## What the converter does

1. **GIF** — decodes every frame with `image`'s GIF decoder (handles disposal and partial frames), normalises frame delays, enforces WhatsApp's 8 ms minimum, trims to 10 seconds.
2. **Images** (PNG/JPEG/WebP) — decoded and padded to a square; transparency is preserved.
3. **Videos** — the browser samples frames at ~12 fps (`<video>` + canvas), capped at a 10-second window, and posts them to Rust as WebP images. Sampling happens during playback (up to 3× speed) because seeking can return stale frames; nothing is uploaded anywhere and no ffmpeg is needed. Longer videos show a trim slider: drag to choose which 10 seconds become the sticker.
4. Every frame is scaled so the longest edge is 512 px and centered on a transparent 512×512 canvas, so landscape and portrait clips get transparent bars instead of being cropped.
5. Animated stickers are encoded with libwebp through `webp-animation` (lossy, alpha preserved). Instead of walking a fixed quality ladder, the encoder starts at quality 60 and predicts the next quality from how far it overshot, so most stickers take one or two passes. If even low quality will not fit 500 KB, the frame rate is halved and retried.
6. Single-frame results are encoded as lossy **static** WebP via libwebp's `WebPEncodeRGBA`, targeting the stricter 100 KB limit.

Long or busy videos are the hard case: WhatsApp's 500 KB animated limit is what dictates how much motion fits, so a full 10-second clip with lots of detail will end up at a lower frame rate and quality. Short clips (2–4 s) stay smooth at 12 fps.

Frame ordering note: WhatsApp stops an animated sticker on its first frame, so the first sampled frame is what people see afterwards.

## WhatsApp sticker spec (used by this tool)

| | Static | Animated |
|---|---|---|
| Size | exactly 512 × 512 px | exactly 512 × 512 px |
| Format | WebP | animated WebP |
| Max file size | 100 KB | 500 KB |
| Duration | — | ≤ 10 s, frames ≥ 8 ms |
| Background | transparent recommended | transparent recommended |

Source: [WhatsApp/stickers](https://github.com/WhatsApp/stickers) (official) and [sticko's dev reference](https://sticko-app.github.io/whatsapp-sticker-resources/).

## API

- `POST /api/convert` — multipart form with a `file` field (GIF, PNG, JPEG, WebP).
- `POST /api/convert-url` — JSON `{ "url": "https://..." }`. Returns WebP directly for GIFs/images, or `{ "kind": "video", "videoUrl": "...", "poster": "..." }` when the link is a video (including X posts).
- `POST /api/convert-frames` — multipart with a `durations` JSON array and repeated `frame` image fields; used by the browser's video sampler.
- `GET /api/video?url=...` — same-origin proxy for video bytes, so the canvas stays readable.

WebP responses carry metadata headers:

```
x-sticker-bytes, x-sticker-frames, x-sticker-ms, x-sticker-kind,
x-sticker-quality, x-sticker-trimmed
```

Errors come back as `{ "error": "..." }` with a 4xx/5xx status.

## Layout

```
src/
  main.rs        axum server, API handlers, embedded frontend
  convert.rs     GIF/image/frame → sticker pipeline + tests
  webp_static.rs libwebp static WebP encoding
  fetch.rs       link resolution (X posts, direct media) + limits
static/
  index.html, style.css, app.js
```

Run the tests with `cargo test`.
