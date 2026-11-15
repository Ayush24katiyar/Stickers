use std::net::IpAddr;
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

    let response = client
        .get(url.clone())
        .send()
        .await
        .context("Could not reach that link. Check the URL and try again.")?;

    if !response.status().is_success() {
        bail!("That link returned HTTP {}.", response.status().as_u16());
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

    Ok(Media::Bytes(data))
}

/// Download a video for the browser to sample. The proxy keeps the canvas
/// untainted, so frames can be read back out.
pub async fn download_video(client: &reqwest::Client, raw_url: &str) -> Result<(Vec<u8>, String)> {
    let url = validate(raw_url)?;

    let response = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
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
