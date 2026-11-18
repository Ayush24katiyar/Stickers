use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::CONTENT_TYPE;
use reqwest::Url;

pub const MAX_GIF_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_VIDEO_BYTES: usize = 40 * 1024 * 1024;

pub struct VideoMedia {
    pub url: String,
    pub poster: Option<String>,
}

pub enum Media {
    /// A video the browser has to decode into frames.
    Video(VideoMedia),
    /// An image or GIF the server can convert directly.
    Bytes(Vec<u8>),
}

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("sticker/0.1 (+local sticker converter)")
        .build()
        .context("Could not build the HTTP client.")
}

/// A few extra attempts for flaky CDN nodes. Some YouTube preview URLs flip
/// between 404 and 200 on the edge servers that answer the request — a short
/// burst usually lands on a healthy node.
async fn get_with_retry(
    client: &reqwest::Client,
    url: Url,
    timeout: Option<Duration>,
) -> Result<reqwest::Response> {
    let attempt = |url: Url| {
        let mut request = client.get(url);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        request.send()
    };

    let mut last_response: Option<reqwest::Response> = None;

    for round in 0..6 {
        let response = attempt(url.clone()).await.context("Could not reach that link.")?;

        if response.status().is_success() {
            return Ok(response);
        }

        last_response = Some(response);
        if round < 5 {
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
    }

    Ok(last_response.expect("six attempts produced no response"))
}

/// Stable cache key: the parts of the URL that don't change when YouTube
/// rotates the signed `sqp`/`rs` token on its preview images.
fn cache_key(url: &Url) -> String {
    let stable = format!("{}://{}{}", url.scheme(), url.host_str().unwrap_or(""), url.path());
    let mut hasher = DefaultHasher::new();
    stable.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn cache_path(url: &Url) -> PathBuf {
    std::env::temp_dir()
        .join("sticker-link-cache")
        .join(format!("{}.webp", cache_key(url)))
}

fn cache_get(url: &Url) -> Option<Vec<u8>> {
    std::fs::read(cache_path(url)).ok()
}

fn cache_put(url: &Url, data: &[u8]) {
    let path = cache_path(url);
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // Ignore failures — a cache miss just means the link must still be live.
    let _ = std::fs::write(path, data);
}

/// Work out what a pasted link points at.
pub async fn resolve(client: &reqwest::Client, raw_url: &str) -> Result<Media> {
    let url = validate(raw_url)?;

    if let Some(tweet_id) = x_status_id(&url) {
        return resolve_x(client, &tweet_id).await;
    }

    if has_video_extension(&url) {
        return Ok(Media::Video(VideoMedia {
            url: url.to_string(),
            poster: None,
        }));
    }

    let response = get_with_retry(client, url.clone(), None)
        .await
        .context("Could not reach that link. Check the URL and try again.")?;

    if !response.status().is_success() {
        // YouTube signs its preview URLs with short-lived tokens. If the link
        // rotated out from under us but we already fetched this exact asset,
        // use the copy we kept — same bytes, fresh token or not.
        if let Some(data) = cache_get(&url) {
            return Ok(Media::Bytes(data));
        }
        bail!(
            "That link returned HTTP {}. If it is a YouTube preview URL it may have expired — save the file and drop it in the Upload tab.",
            response.status().as_u16()
        );
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.starts_with("video/") {
        return Ok(Media::Video(VideoMedia {
            url: url.to_string(),
            poster: None,
        }));
    }

    let data = read_capped(response, MAX_GIF_BYTES).await?;
    if !looks_like_media(&data) {
        bail!("That link is not a GIF, image, or video.");
    }

    cache_put(&url, &data);
    Ok(Media::Bytes(data))
}

/// Download a video for the browser to sample. The proxy keeps the canvas
/// untainted, so frames can be read back out.
pub async fn download_video(client: &reqwest::Client, raw_url: &str) -> Result<(Vec<u8>, String)> {
    let url = validate(raw_url)?;

    let response = get_with_retry(client, url, Some(Duration::from_secs(120)))
        .await
        .context("Could not reach that video.")?;

    if !response.status().is_success() {
        bail!("That video returned HTTP {}.", response.status().as_u16());
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("video/mp4")
        .to_string();

    if !content_type.starts_with("video/") && content_type != "application/octet-stream" {
        bail!("That link did not return a video.");
    }

    let bytes = read_capped(response, MAX_VIDEO_BYTES).await?;
    Ok((bytes, content_type))
}

/// X (Twitter) "GIFs" are MP4 files. Ask the public syndication endpoint for
/// the media behind a post instead of scraping the page.
async fn resolve_x(client: &reqwest::Client, tweet_id: &str) -> Result<Media> {
    let endpoint = format!(
        "https://cdn.syndication.twimg.com/tweet-result?id={tweet_id}&lang=en&token=sticker"
    );

    let response = client
        .get(endpoint)
        .send()
        .await
        .context("Could not reach X.")?;

    if !response.status().is_success() {
        bail!("Could not read that X post (HTTP {}).", response.status().as_u16());
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .context("X returned something unexpected.")?;

    let media = payload
        .get("mediaDetails")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .context("That X post has no GIF, image, or video.")?;

    if let Some(variants) = media
        .pointer("/video_info/variants")
        .and_then(|value| value.as_array())
    {
        let best = variants
            .iter()
            .filter(|variant| variant["content_type"] == "video/mp4")
            .max_by_key(|variant| variant["bitrate"].as_u64().unwrap_or(0));

        if let Some(variant) = best {
            if let Some(video_url) = variant["url"].as_str() {
                return Ok(Media::Video(VideoMedia {
                    url: video_url.to_string(),
                    poster: media["media_url_https"].as_str().map(str::to_string),
                }));
            }
        }
    }

    if let Some(photo_url) = media["media_url_https"].as_str() {
        let separator = if photo_url.contains('?') { '&' } else { '?' };
        let full_url = format!("{photo_url}{separator}name=large");
        let response = client
            .get(&full_url)
            .send()
            .await
            .context("Could not download that X image.")?;

        if !response.status().is_success() {
            bail!("X would not hand over that image.");
        }

        let data = read_capped(response, MAX_GIF_BYTES).await?;
        return Ok(Media::Bytes(data));
    }

    bail!("That X post has no GIF, image, or video.")
}

fn validate(raw_url: &str) -> Result<Url> {
    let url = Url::parse(raw_url.trim()).context("That is not a valid URL.")?;

    if url.scheme() != "https" {
        bail!("Only https:// links are supported.");
    }

    let host = url.host_str().context("That link has no host.")?;
    if is_private_host(host) {
        bail!("That host is not reachable from the converter.");
    }

    Ok(url)
}

fn x_status_id(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let is_x = matches!(
        host.as_str(),
        "x.com" | "www.x.com" | "mobile.x.com" | "twitter.com" | "www.twitter.com" | "mobile.twitter.com"
    );
    if !is_x {
        return None;
    }

    let segments: Vec<&str> = url.path_segments()?.collect();
    let position = segments.iter().position(|segment| *segment == "status")?;
    let id = segments.get(position + 1)?;
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
        Some((*id).to_string())
    } else {
        None
    }
}

fn has_video_extension(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    [".mp4", ".webm", ".mov", ".m4v"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

fn looks_like_media(data: &[u8]) -> bool {
    data.starts_with(b"GIF8")
        || data.starts_with(&[0x89, b'P', b'N', b'G'])
        || data.starts_with(&[0xFF, 0xD8, 0xFF])
        || (data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP")
}

async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        if length as usize > cap {
            bail!("That file is larger than {} MB.", cap / (1024 * 1024));
        }
    }

    let mut data = Vec::with_capacity(response.content_length().unwrap_or(0).min(cap as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .context("The download stopped before the file was complete.")?
    {
        if data.len() + chunk.len() > cap {
            bail!("That file is larger than {} MB.", cap / (1024 * 1024));
        }
        data.extend_from_slice(&chunk);
    }

    Ok(data)
}

fn is_private_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }

    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
        }
        Ok(IpAddr::V6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => false,
    }
}
