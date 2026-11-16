mod convert;
mod fetch;
mod webp_static;

use anyhow::Context as _;
use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

const INDEX: &str = include_str!("../static/index.html");
const STYLE: &str = include_str!("../static/style.css");
const SCRIPT: &str = include_str!("../static/app.js");

const MAX_UPLOAD_BYTES: usize = 40 * 1024 * 1024;
const MAX_FRAMES: usize = 150;

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = AppState {
        client: fetch::client()?,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/style.css", get(style))
        .route("/app.js", get(script))
        .route("/api/convert", post(convert_upload))
        .route("/api/convert-url", post(convert_from_url))
        .route("/api/convert-frames", post(convert_frames_upload))
        .route("/api/video", get(proxy_video))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3000);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("\n  sticker is running → http://localhost:{port}\n");
    axum::serve(listener, app).await?;
    Ok(())
}

const NO_STORE: &str = "no-store, must-revalidate";

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, NO_STORE),
        ],
        INDEX,
    )
}

async fn style() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, NO_STORE),
        ],
        STYLE,
    )
}

async fn script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, NO_STORE),
        ],
        SCRIPT,
    )
}

#[derive(Deserialize)]
struct UrlPayload {
    url: String,
}

#[derive(Deserialize)]
struct VideoQuery {
    url: String,
}

async fn convert_from_url(State(state): State<AppState>, Json(payload): Json<UrlPayload>) -> Response {
    let media = match fetch::resolve(&state.client, &payload.url).await {
        Ok(media) => media,
        Err(error) => return bad_request(format!("{error:#}")).into_response(),
    };

    match media {
        fetch::Media::Bytes(bytes) => convert_bytes(bytes, &payload.url).await,
        fetch::Media::Video(video) => {
            let encoded: String =
                url::form_urlencoded::byte_serialize(video.url.as_bytes()).collect();
            Json(json!({
                "kind": "video",
                "videoUrl": format!("/api/video?url={encoded}"),
                "poster": video.poster,
            }))
            .into_response()
        }
    }
}

async fn convert_upload(mut multipart: Multipart) -> Response {
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() != Some("file") {
                    continue;
                }
                let name = field.file_name().unwrap_or("upload").to_string();
                return match field.bytes().await {
                    Ok(bytes) => convert_bytes(bytes.to_vec(), &name).await,
                    Err(error) => {
                        bad_request(format!("Could not read the upload: {error}")).into_response()
                    }
                };
            }
            Ok(None) => break,
            Err(error) => {
                return bad_request(format!("Could not read the upload: {error}")).into_response()
            }
        }
    }

    bad_request("No file was uploaded.").into_response()
}

/// Frames sampled from a video in the browser, already encoded as images.
async fn convert_frames_upload(mut multipart: Multipart) -> Response {
    let mut durations: Vec<u32> = Vec::new();
    let mut raw_frames: Vec<Vec<u8>> = Vec::new();

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => match field.name() {
                Some("durations") => {
                    if let Ok(text) = field.text().await {
                        durations = serde_json::from_str(&text).unwrap_or_default();
                    }
                }
                Some("frame") => match field.bytes().await {
                    Ok(bytes) => {
                        raw_frames.push(bytes.to_vec());
                        if raw_frames.len() > MAX_FRAMES {
                            return bad_request("That is too many frames. Try a shorter clip.")
                                .into_response();
                        }
                    }
                    Err(error) => {
                        return bad_request(format!("Could not read a frame: {error}"))
                            .into_response()
                    }
                },
                _ => {}
            },
            Ok(None) => break,
            Err(error) => {
                return bad_request(format!("Could not read the upload: {error}")).into_response()
            }
        }
    }

    if raw_frames.is_empty() {
        return bad_request("No frames were received.").into_response();
    }

    let decoded = tokio::task::spawn_blocking(move || {
        let mut frames = Vec::with_capacity(raw_frames.len());
        for (index, bytes) in raw_frames.into_iter().enumerate() {
            let image = image::load_from_memory(&bytes)
                .with_context(|| format!("Frame {} could not be decoded.", index + 1))?;
            let duration = durations.get(index).copied().unwrap_or(100);
            frames.push((image.to_rgba8(), duration));
        }
        convert::convert_frames(frames)
    })
    .await;

    finish_conversion(decoded, "video").await
}

async fn proxy_video(State(state): State<AppState>, Query(query): Query<VideoQuery>) -> Response {
    match fetch::download_video(&state.client, &query.url).await {
        Ok((bytes, content_type)) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("video/mp4")),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            (headers, bytes).into_response()
        }
        Err(error) => bad_request(format!("{error:#}")).into_response(),
    }
}

async fn convert_bytes(bytes: Vec<u8>, source_name: &str) -> Response {
    if bytes.len() > fetch::MAX_GIF_BYTES {
        return bad_request("That file is larger than 20 MB.").into_response();
    }

    let decoded = tokio::task::spawn_blocking(move || convert::convert(&bytes)).await;
    finish_conversion(decoded, source_name).await
}

async fn finish_conversion(
    result: Result<anyhow::Result<convert::Sticker>, tokio::task::JoinError>,
    source_name: &str,
) -> Response {
    let sticker = match result {
        Ok(Ok(sticker)) => sticker,
        Ok(Err(error)) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": format!("{error:#}") })),
            )
                .into_response()
        }
        Err(error) => {
            eprintln!("conversion task panicked: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "The converter crashed on this file. Try another one." })),
            )
                .into_response();
        }
    };

    let filename = sticker_filename(source_name);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"sticker.webp\"")),
    );
    headers.insert("x-sticker-bytes", int_header(sticker.bytes.len()));
    headers.insert("x-sticker-frames", int_header(sticker.frames));
    headers.insert("x-sticker-ms", int_header(sticker.duration_ms));
    headers.insert("x-sticker-kind", HeaderValue::from_static(sticker.kind));
    headers.insert("x-sticker-quality", int_header(sticker.quality));
    headers.insert(
        "x-sticker-trimmed",
        HeaderValue::from_static(if sticker.trimmed { "1" } else { "0" }),
    );

    (headers, sticker.bytes).into_response()
}

fn sticker_filename(source_name: &str) -> String {
    let mut stem = source_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("sticker")
        .to_string();

    if let Some(position) = stem.rfind('.') {
        let extension = stem[position + 1..].to_ascii_lowercase();
        if matches!(
            extension.as_str(),
            "gif" | "png" | "jpg" | "jpeg" | "webp" | "mp4" | "webm" | "mov" | "m4v"
        ) {
            stem.truncate(position);
        }
    }

    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "sticker.webp".to_string()
    } else {
        format!("{cleaned}.webp")
    }
}

fn int_header(value: impl std::fmt::Display) -> HeaderValue {
    HeaderValue::from_str(&value.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0"))
}

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
}
