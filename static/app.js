const $ = (sel) => document.querySelector(sel);

const els = {
  tabLink: $("#tab-link"),
  tabUpload: $("#tab-upload"),
  panelLink: $("#panel-link"),
  panelUpload: $("#panel-upload"),
  urlInput: $("#url-input"),
  convertUrl: $("#convert-url"),
  drop: $("#drop"),
  fileInput: $("#file-input"),
  fileName: $("#file-name"),
  convertFile: $("#convert-file"),
  status: $("#status"),
  result: $("#result"),
  beforeImg: $("#before-img"),
  afterImg: $("#after-img"),
  meta: $("#meta"),
  download: $("#download"),
  reset: $("#reset"),
  trim: $("#trim"),
  trimVideo: $("#trim-video"),
  trimStart: $("#trim-start"),
  trimCaption: $("#trim-caption"),
  trimRangeRow: $("#trim-range-row"),
};

const VIDEO_EXT = /\.(mp4|webm|mov|m4v)$/i;
const IMAGE_EXT = /\.(gif|png|jpe?g|webp)$/i;
const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
// Local videos are only ever decoded in the browser, so the ceiling is about
// what the tab can handle rather than what the server will accept.
const MAX_LOCAL_VIDEO_BYTES = 250 * 1024 * 1024;

let selectedFile = null;
let selectedUrl = "";
let beforeUrl = null;
let afterUrl = null;
let busy = false;
/// The clip a conversion will use, kept loaded so the preview and the sampler
/// share one video element (and one download).
let pendingVideo = null;
/// Which part of the source video ended up in the sticker, for the result chip.
let sourceRange = null;

/* ---------------- tabs ---------------- */

function setTab(name) {
  const isLink = name === "link";
  els.tabLink.classList.toggle("is-active", isLink);
  els.tabUpload.classList.toggle("is-active", !isLink);
  els.tabLink.setAttribute("aria-selected", String(isLink));
  els.tabUpload.setAttribute("aria-selected", String(!isLink));
  els.panelLink.classList.toggle("is-hidden", !isLink);
  els.panelUpload.classList.toggle("is-hidden", isLink);
}

els.tabLink.addEventListener("click", () => setTab("link"));
els.tabUpload.addEventListener("click", () => setTab("upload"));

/* ---------------- helpers ---------------- */

function setStatus(message, kind = "") {
  els.status.className = "status" + (kind ? " " + kind : "");
  els.status.innerHTML =
    kind === "busy" ? `<span class="spin"></span>${escapeHtml(message)}` : escapeHtml(message || "");
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

function prettyBytes(bytes) {
  const n = Number(bytes);
  if (!Number.isFinite(n)) return "—";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10 * 1024 ? 1 : 0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(2)} MB`;
}

function fmtDuration(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n)) return "—";
  return n >= 1000 ? `${(n / 1000).toFixed(n % 1000 === 0 ? 0 : 2)} s` : `${n} ms`;
}

function isVideoFile(file) {
  return Boolean(file) && (file.type.startsWith("video/") || VIDEO_EXT.test(file.name || ""));
}

function isStickerSource(file) {
  return Boolean(file) && (file.type.startsWith("image/") || IMAGE_EXT.test(file.name || ""));
}

function setBusy(value, label) {
  busy = value;
  els.convertUrl.disabled = value || !els.urlInput.value.trim();
  els.convertFile.disabled = value || !selectedFile;
  els.urlInput.disabled = value;
  if (value && label) setStatus(label, "busy");
}

/* ---------------- file picker + drag & drop ---------------- */

async function pickFile(file) {
  if (!file) return;

  const video = isVideoFile(file);
  if (!video && !isStickerSource(file)) {
    setStatus("Use a GIF, PNG, JPEG, WebP, or a short video.", "error");
    return;
  }

  const limit = video ? MAX_LOCAL_VIDEO_BYTES : MAX_IMAGE_BYTES;
  if (file.size > limit) {
    setStatus(`That file is over ${Math.round(limit / (1024 * 1024))} MB — trim it down first.`, "error");
    return;
  }

  releasePendingVideo();

  selectedFile = file;
  selectedUrl = "";
  els.fileName.textContent = `${file.name} · ${prettyBytes(file.size)}`;
  els.fileName.classList.remove("is-hidden");
  els.convertFile.disabled = busy;

  if (!video) {
    setStatus("Ready when you are.");
    return;
  }

  const objectUrl = URL.createObjectURL(file);
  els.convertFile.disabled = true;
  setStatus("Loading video…", "busy");

  try {
    const element = await openVideo(objectUrl);
    pendingVideo = {
      element,
      duration: Number.isFinite(element.duration) ? element.duration : 0,
      poster: null,
      objectUrl,
      sourceKey: fileKey(file),
    };
    showTrim(pendingVideo);
    setStatus(
      canTrim(pendingVideo)
        ? "Pick the 10 seconds to keep, then press Convert file."
        : "Ready.",
    );
    els.convertFile.disabled = busy;
  } catch (error) {
    URL.revokeObjectURL(objectUrl);
    pendingVideo = null;
    hideTrim();
    setStatus(friendlyError(error), "error");
  }
}

els.drop.addEventListener("click", () => els.fileInput.click());

els.drop.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    els.fileInput.click();
  }
});

els.fileInput.addEventListener("change", () => pickFile(els.fileInput.files[0]));

["dragenter", "dragover"].forEach((type) => {
  els.drop.addEventListener(type, (event) => {
    event.preventDefault();
    els.drop.classList.add("is-over");
  });
});

["dragleave", "dragend"].forEach((type) => {
  els.drop.addEventListener(type, () => els.drop.classList.remove("is-over"));
});

els.drop.addEventListener("drop", (event) => {
  event.preventDefault();
  els.drop.classList.remove("is-over");
  const file = [...(event.dataTransfer?.files ?? [])].find(
    (candidate) => isVideoFile(candidate) || isStickerSource(candidate),
  );
  if (file) {
    setTab("upload");
    pickFile(file);
  } else {
    setStatus("Drop a GIF, image, or short video here.", "error");
  }
});

// Stop the browser from opening a file dropped outside the drop zone.
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("drop", (e) => e.preventDefault());

// Paste a GIF from the clipboard (Ctrl/Cmd+V), or paste a link.
document.addEventListener("paste", (event) => {
  const items = [...(event.clipboardData?.items ?? [])];
  const image = items.find(
    (item) => item.kind === "file" && /^image\//i.test(item.type),
  );
  if (image) {
    event.preventDefault();
    setTab("upload");
    pickFile(image.getAsFile());
    return;
  }
  const text = event.clipboardData?.getData("text")?.trim();
  if (text && /^https?:\/\//i.test(text) && !els.urlInput.value) {
    els.urlInput.value = text;
    onUrlInput();
  }
});

/* ---------------- convert ---------------- */

function onUrlInput() {
  els.convertUrl.disabled = busy || !els.urlInput.value.trim();
}

els.urlInput.addEventListener("input", onUrlInput);
els.urlInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !els.convertUrl.disabled) convertFromUrl();
});

els.convertUrl.addEventListener("click", convertFromUrl);
els.convertFile.addEventListener("click", convertFromFile);

function resetResult() {
  els.result.classList.add("is-hidden");
  sourceRange = null;
  if (afterUrl) URL.revokeObjectURL(afterUrl);
  afterUrl = null;
  els.afterImg.removeAttribute("src");
  els.beforeImg.removeAttribute("src");
  if (beforeUrl) URL.revokeObjectURL(beforeUrl);
  beforeUrl = null;
}

async function readError(response) {
  try {
    const data = await response.json();
    if (data?.error) return data.error;
  } catch {
    /* fall through */
  }
  return `Conversion failed (HTTP ${response.status}).`;
}

async function convertFromUrl() {
  const url = els.urlInput.value.trim();
  if (!url || busy) return;

  try {
    new URL(url);
  } catch {
    setStatus("That link is not a valid URL.", "error");
    return;
  }

  // The link was already resolved and its video is loaded: this press converts
  // the trimmed clip the user picked.
  if (pendingVideo && pendingVideo.sourceKey === urlKey(url)) {
    await convertPendingVideo();
    return;
  }

  releasePendingVideo();
  selectedUrl = url;
  selectedFile = null;
  resetResult();
  setBusy(true, "Building your sticker…");

  try {
    const response = await fetch("/api/convert-url", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ url }),
    });

    if (!response.ok) throw new Error(await readError(response));

    const type = response.headers.get("content-type") || "";
    if (type.includes("application/json")) {
      const info = await response.json();
      if (info.kind !== "video" || !info.videoUrl) {
        throw new Error("The converter returned something unexpected.");
      }

      setStatus("Loading the video…", "busy");
      const element = await openVideo(info.videoUrl);
      pendingVideo = {
        element,
        duration: Number.isFinite(element.duration) ? element.duration : 0,
        poster: info.poster || null,
        objectUrl: null,
        sourceKey: urlKey(url),
      };
      showTrim(pendingVideo);

      if (!canTrim(pendingVideo)) {
        await convertPendingVideo();
        return;
      }

      setStatus("Pick the 10 seconds to keep, then press Convert link again.");
      return;
    }

    els.beforeImg.src = url;
    await showResult(await response.arrayBuffer(), response.headers);
  } catch (error) {
    setStatus(friendlyError(error), "error");
  } finally {
    setBusy(false);
  }
}

async function convertFromFile() {
  if (!selectedFile || busy) return;

  if (isVideoFile(selectedFile)) {
    if (!pendingVideo || pendingVideo.sourceKey !== fileKey(selectedFile)) {
      const objectUrl = URL.createObjectURL(selectedFile);
      try {
        const element = await openVideo(objectUrl);
        pendingVideo = {
          element,
          duration: Number.isFinite(element.duration) ? element.duration : 0,
          poster: null,
          objectUrl,
          sourceKey: fileKey(selectedFile),
        };
        showTrim(pendingVideo);
      } catch (error) {
        URL.revokeObjectURL(objectUrl);
        setStatus(friendlyError(error), "error");
        return;
      }
    }

    await convertPendingVideo();
    return;
  }

  resetResult();
  setBusy(true, "Converting…");

  try {
    const form = new FormData();
    form.append("file", selectedFile, selectedFile.name || "input");

    const response = await fetch("/api/convert", { method: "POST", body: form });
    if (!response.ok) throw new Error(await readError(response));

    if (beforeUrl) URL.revokeObjectURL(beforeUrl);
    beforeUrl = URL.createObjectURL(selectedFile);
    els.beforeImg.src = beforeUrl;

    await showResult(await response.arrayBuffer(), response.headers);
  } catch (error) {
    setStatus(friendlyError(error), "error");
  } finally {
    setBusy(false);
  }
}

/**
 * Videos (including X/Twitter "GIFs", which are MP4) are decoded by the
 * browser. Frames are sampled and sent to Rust for the actual WebP work.
 * `startAt` is the offset, in seconds, where the sticker begins.
 */
async function convertVideo(video, poster, startAt = 0) {
  const rawDuration = Number.isFinite(video.duration) ? video.duration : 0;
  const start = Math.min(Math.max(0, startAt), Math.max(0, rawDuration - 0.2));
  const duration = Math.min(rawDuration - start, 10);

  if (!duration || duration < 0.15) {
    throw new Error("That video is too short to turn into a sticker.");
  }

  sourceRange = rawDuration - duration > 0.05 ? { start, end: start + duration } : null;

  const fps = 12;
  const count = Math.max(2, Math.min(120, Math.round(duration * fps)));
  const perFrame = Math.max(10, Math.round((duration * 1000) / count));

  const maxSide = 512;
  const sourceW = video.videoWidth || 512;
  const sourceH = video.videoHeight || 512;
  const scale = Math.min(1, maxSide / Math.max(sourceW, sourceH));

  const canvas = document.createElement("canvas");
  canvas.width = Math.max(2, Math.round(sourceW * scale));
  canvas.height = Math.max(2, Math.round(sourceH * scale));
  const context = canvas.getContext("2d");

  const frames = await sampleVideoFrames(video, canvas, context, duration, count, start);

  if (poster) {
    els.beforeImg.src = poster;
  } else if (frames[0]) {
    els.beforeImg.src = await blobToDataUrl(frames[0]);
  }

  setStatus("Building the sticker…", "busy");

  const totalBytes = frames.reduce((sum, blob) => sum + blob.size, 0);
  if (totalBytes > 30 * 1024 * 1024) {
    throw new Error("Those frames are too big to upload. Try a shorter clip.");
  }

  const form = new FormData();
  form.append("durations", JSON.stringify(frames.map(() => perFrame)));
  frames.forEach((blob, index) => form.append("frame", blob, `frame-${index}.bin`));

  const response = await fetch("/api/convert-frames", { method: "POST", body: form });
  if (!response.ok) throw new Error(await readError(response));

  await showResult(await response.arrayBuffer(), response.headers);
}

/** Every video lives in the same preview/sampler element. */
async function openVideo(src) {
  const video = els.trimVideo;
  video.pause();
  video.removeAttribute("src");
  video.load();
  video.src = src;
  await waitForVideo(video);
  return video;
}

function fileKey(file) {
  return `file:${file.name}:${file.size}:${file.lastModified}`;
}

function urlKey(url) {
  return `url:${url}`;
}

function waitForVideo(video) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error("The video took too long to load."));
    }, 25000);

    const cleanup = () => {
      clearTimeout(timer);
      video.removeEventListener("loadeddata", onReady);
      video.removeEventListener("error", onError);
    };

    const onReady = () => {
      cleanup();
      resolve();
    };

    const onError = () => {
      cleanup();
      reject(new Error("This video could not be loaded. It may be blocked or use an unsupported format."));
    };

    video.addEventListener("loadeddata", onReady);
    video.addEventListener("error", onError);
  });
}

/**
 * Sample frames by playing the video and grabbing the canvas at fixed
 * intervals. Seeking is not reliable here: right after a seek, browsers can
 * still hand back the previously decoded frame, which yields a stack of
 * identical stills instead of an animation.
 */
async function sampleVideoFrames(video, canvas, context, duration, count, startAt = 0) {
  const interval = duration / count;
  const frames = [];

  // Long clips play faster than real time; the sampled timeline stays exact
  // because frames are taken from the actual playhead position.
  video.playbackRate = duration > 2.5 ? Math.min(3, duration / 2) : 1;

  // Jump to the chosen start. Sampling itself still happens during playback,
  // which keeps the decoded frames fresh.
  if (Math.abs(video.currentTime - startAt) > 0.05) {
    await seekVideo(video, startAt);
  }

  try {
    await video.play();
  } catch {
    throw new Error("The browser blocked video playback, so frames could not be sampled.");
  }

  for (let index = 0; index < count; index++) {
    const target = startAt + (index + 0.5) * interval;
    const reached = await waitForPlayhead(video, target);

    video.pause();
    context.drawImage(video, 0, 0, canvas.width, canvas.height);
    // WebP keeps the upload ~10x smaller than PNG and still carries alpha;
    // browsers without a WebP encoder quietly return PNG instead.
    frames.push(await canvasBlob(canvas, "image/webp", 0.92));
    setStatus(`Sampling frames… ${index + 1}/${count}`, "busy");

    if (!reached || video.ended) break;

    try {
      await video.play();
    } catch {
      break;
    }
  }

  video.pause();

  if (frames.length < 2) {
    throw new Error("That video did not produce enough frames to animate.");
  }

  return frames;
}

function waitForPlayhead(video, time) {
  return new Promise((resolve) => {
    const started = performance.now();
    const tick = () => {
      if (video.currentTime >= time - 0.01) {
        resolve(true);
        return;
      }
      if (video.ended || performance.now() - started > 20000) {
        resolve(false);
        return;
      }
      setTimeout(tick, 8);
    };
    tick();
  });
}

function seekVideo(video, time) {
  return new Promise((resolve) => {
    const finish = () => {
      clearTimeout(timer);
      video.removeEventListener("seeked", finish);
      resolve();
    };
    const timer = setTimeout(finish, 5000);
    video.addEventListener("seeked", finish);
    video.currentTime = time;
  });
}

/* ---------------- trimming ---------------- */

function canTrim(pending) {
  return Boolean(pending) && pending.duration > 10.05;
}

function trimStart(pending) {
  if (!canTrim(pending)) return 0;
  const value = Number(els.trimStart.value);
  return Number.isFinite(value) ? value : 0;
}

function fmtClock(seconds) {
  const value = Math.max(0, Number(seconds) || 0);
  const minutes = Math.floor(value / 60);
  const rest = value - minutes * 60;
  return `${minutes}:${rest < 10 ? "0" : ""}${rest.toFixed(rest < 10 && rest % 1 !== 0 ? 1 : 0)}`;
}

function showTrim(pending) {
  els.trim.classList.remove("is-hidden");
  els.trimVideo.currentTime = 0;

  const trimmable = canTrim(pending);
  els.trimRangeRow.classList.toggle("is-hidden", !trimmable);
  els.trimStart.disabled = !trimmable;
  els.trimStart.max = trimmable ? (pending.duration - 10).toFixed(1) : "0";
  els.trimStart.value = "0";

  updateTrimCaption();
}

function hideTrim() {
  els.trim.classList.add("is-hidden");
  els.trimRangeRow.classList.add("is-hidden");
  els.trimCaption.textContent = "";
}

function updateTrimCaption() {
  if (!pendingVideo) return;
  const start = trimStart(pendingVideo);
  const end = Math.min(start + 10, pendingVideo.duration);

  els.trimCaption.textContent = canTrim(pendingVideo)
    ? `Sticker covers ${fmtClock(start)} – ${fmtClock(end)} of ${fmtClock(pendingVideo.duration)}`
    : `Full clip · ${fmtClock(pendingVideo.duration)} (under the 10 s limit)`;
}

async function convertPendingVideo() {
  if (!pendingVideo) return;

  const start = trimStart(pendingVideo);
  resetResult();
  setBusy(true, "Building your sticker…");

  try {
    await convertVideo(pendingVideo.element, pendingVideo.poster, start);
  } catch (error) {
    setStatus(friendlyError(error), "error");
  } finally {
    setBusy(false);
    releasePendingVideo();
  }
}

function releasePendingVideo() {
  if (pendingVideo) {
    const video = pendingVideo.element;
    video.pause();
    video.removeAttribute("src");
    video.load();
    if (pendingVideo.objectUrl) URL.revokeObjectURL(pendingVideo.objectUrl);
    pendingVideo = null;
  }
  hideTrim();
}

els.trimStart.addEventListener("input", () => {
  if (!pendingVideo) return;
  els.trimVideo.currentTime = trimStart(pendingVideo);
  updateTrimCaption();
});

function canvasBlob(canvas, type, quality) {
  return new Promise((resolve, reject) => {
    canvas.toBlob(
      (blob) => (blob ? resolve(blob) : reject(new Error("Could not export a video frame."))),
      type,
      quality,
    );
  });
}

function blobToDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(new Error("Could not read the first frame."));
    reader.readAsDataURL(blob);
  });
}

function friendlyError(error) {
  const message = error instanceof Error ? error.message : String(error);
  if (/failed to fetch|networkerror|connection aborted/i.test(message)) {
    return "The upload did not finish. Check that the server is running, and try a shorter clip if it keeps happening.";
  }
  return message;
}

async function showResult(buffer, headers) {
  const blob = new Blob([buffer], { type: "image/webp" });
  afterUrl = URL.createObjectURL(blob);

  els.afterImg.src = afterUrl;
  els.download.href = afterUrl;
  els.download.download = filenameFor("sticker");

  const bytes = Number(headers.get("x-sticker-bytes") ?? buffer.byteLength);
  const frames = Number(headers.get("x-sticker-frames") ?? 0);
  const duration = Number(headers.get("x-sticker-ms") ?? 0);
  const kind = headers.get("x-sticker-kind") ?? "animated";
  const trimmed = headers.get("x-sticker-trimmed") === "1";
  const quality = headers.get("x-sticker-quality");
  const fits = bytes <= (kind === "static" ? 100 : 500) * 1024;

  const chips = [
    '<span class="chip">512 × 512</span>',
    `<span class="chip ${fits ? "ok" : ""}"><b>${prettyBytes(bytes)}</b> ${fits ? "· fits WhatsApp" : "· over the limit"}</span>`,
    kind === "animated"
      ? `<span class="chip"><b>${frames}</b> frames · ${fmtDuration(duration)}</span>`
      : '<span class="chip">static</span>',
  ];
  if (trimmed) chips.push('<span class="chip">trimmed to 10 s</span>');
  else if (sourceRange) chips.push(`<span class="chip">clip ${fmtClock(sourceRange.start)}–${fmtClock(sourceRange.end)}</span>`);
  if (quality) chips.push(`<span class="chip">quality ${escapeHtml(quality)}</span>`);

  els.meta.innerHTML = chips.join("");
  els.result.classList.remove("is-hidden");
  setStatus("Sticker is ready.");
  els.result.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function filenameFor(base) {
  const source = selectedFile?.name || selectedUrl || "";
  const name = source.split(/[\\/]/).pop()?.replace(/\.[a-z0-9]{2,5}$/i, "").replace(/[^\w.-]+/g, "-") || base;
  return `${name}.webp`;
}

/* ---------------- reset ---------------- */

els.reset.addEventListener("click", () => {
  resetResult();
  releasePendingVideo();
  els.urlInput.value = "";
  els.fileInput.value = "";
  selectedFile = null;
  selectedUrl = "";
  els.fileName.classList.add("is-hidden");
  els.convertFile.disabled = true;
  onUrlInput();
  setStatus("");
});
