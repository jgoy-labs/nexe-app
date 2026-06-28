// Step 3 — Download progress via fetch + ReadableStream.
// 2026-05-23: embedder fetch removed (sidecar resolves it lazily on the first
// RAG query). Stall watchdog + visible Cancel/Retry added so a silent
// backend doesn't leave the wizard hanging forever.
//
// EventSource does not work reliably from tauri://localhost to http://127.0.0.1
// (WebKit silently drops the connection). Using fetch with a readable stream
// instead — same SSE wire format, no CORS issues with the custom scheme.

import { invoke } from "@tauri-apps/api/core";
import { goToStep, state } from "./main.js";
import { t } from "./i18n.js";

// If we go this long without any SSE event (progress/keepalive/done/error)
// from the backend, treat the download as stalled and surface a Retry button.
// Real Hugging Face downloads emit a progress event every ~1–2 s, and the
// sidecar sends a keepalive at least every 15 s, so 90 s is a safe margin
// even on slow links.
const STALL_TIMEOUT_MS = 90_000;


/** Build a labelled progress block (label + <progress> + info paragraph). */
function _buildProgressBlock(parent, labelText) {
  const block = document.createElement("div");
  block.className = "download-block";

  const label = document.createElement("p");
  label.className = "download-label";
  label.textContent = labelText;
  block.appendChild(label);

  const bar = document.createElement("progress");
  bar.className = "download-bar";
  bar.max = 100;
  bar.value = 0;
  block.appendChild(bar);

  const info = document.createElement("p");
  info.className = "download-info";
  info.textContent = "0%";
  block.appendChild(info);

  parent.appendChild(block);
  return { bar, info };
}

/**
 * Stall watchdog: fires `abortCtrl` with a sentinel reason when too long
 * passes between SSE events, so a silent backend triggers a user-visible error
 * instead of leaving the wizard hanging. Returns the control surface used by
 * `_streamDownload`. Exported for unit testing.
 *
 * @param {AbortController} abortCtrl
 */
export function _createStallController(abortCtrl) {
  let stallTimer = null;
  const arm = () => {
    if (stallTimer) clearTimeout(stallTimer);
    stallTimer = setTimeout(() => {
      abortCtrl.abort(new DOMException("stalled", "AbortError"));
    }, STALL_TIMEOUT_MS);
  };
  const disarm = () => {
    if (stallTimer) clearTimeout(stallTimer);
    stallTimer = null;
  };
  const isUserCancel = () =>
    abortCtrl.signal.aborted && abortCtrl.signal.reason?.message !== "stalled";
  const isStall = () =>
    abortCtrl.signal.aborted && abortCtrl.signal.reason?.message === "stalled";
  // Maps an abort into a user-facing result, or null if it wasn't an abort.
  const wrapAbortError = () => {
    if (isStall()) return { ok: false, message: t("step3_stalled", state.lang), stalled: true };
    if (isUserCancel()) return { ok: false, message: t("step3_cancelled", state.lang), cancelled: true };
    return null;
  };
  return { arm, disarm, isStall, isUserCancel, wrapAbortError };
}

/** Build the result for a network/read error: abort-aware, else connection lost. */
function _streamErrorResult(stall, err) {
  const aborted = stall.wrapAbortError();
  if (aborted) return aborted;
  return { ok: false, message: t("step3_connection_lost", state.lang) + ": " + err.message };
}

/**
 * INST-002-FE: render a non-blocking warning line into the step3 warnings
 * container (yellow ⚠, parity with the CLI). Today's only warning is
 * SHA256_NOT_PINNED (model installed without weight verification); the message
 * is localised here rather than echoing the backend's English `data.message`.
 * The download keeps going — this only informs.
 */
function _renderShaWarning(data, container) {
  const model = state.selectedModel?.name || "";
  const line = document.createElement("p");
  line.className = "download-warning";
  line.textContent = "⚠ " + (model ? model + ": " : "") + t("step3_sha_warning", state.lang);
  container.appendChild(line);
}

/** Render a `progress` SSE event into the bar + info widgets. */
function _applyProgress(data, bar, info) {
  const pct = Math.round(data.percent ?? 0);
  bar.value = pct;
  const parts = [pct + "%"];
  if (data.cached) parts.push(t("step3_cached", state.lang) || "(cache)");
  if (data.speed && data.speed !== "—") parts.push(t("step3_speed", state.lang) + ": " + data.speed);
  if (data.eta   && data.eta   !== "—") parts.push(t("step3_eta",   state.lang) + ": " + data.eta);
  info.textContent = parts.join(" — ");
}

/** Cancel the stream reader, ignoring any error (already-closed, aborted). */
function _safeCancel(reader) {
  try { reader.cancel(); } catch (_) { /* noop */ }
}

/**
 * Parse and act on a single SSE frame. Updates the progress widgets in place.
 * Returns a terminal result (`{ok}`) for `done`/`error`, or `null` to keep
 * reading (keepalive, progress, warning, or unparsable frame). Exported for
 * testing.
 *
 * @param {string} frame
 * @param {HTMLProgressElement} bar
 * @param {HTMLElement} info
 * @param {ReadableStreamDefaultReader} reader
 * @param {(data: object) => void} [onWarning]  INST-002-FE: invoked for a
 *        non-blocking `warning` frame (e.g. SHA256_NOT_PINNED) so the caller can
 *        surface it. Optional — when absent the warning is simply not rendered.
 */
export function _handleSseFrame(frame, bar, info, reader, onWarning) {
  const dataLine = frame.split("\n").find(l => l.startsWith("data: "));
  if (!dataLine) return null;
  let data;
  try { data = JSON.parse(dataLine.slice(6)); } catch (_) { return null; }

  switch (data.type) {
    case "progress":
      _applyProgress(data, bar, info);
      return null;
    case "warning":
      // INST-002-FE: a non-blocking notice (e.g. SHA256_NOT_PINNED). Surface it
      // to the user (parity with the CLI's yellow ⚠) but keep reading — the
      // download still completes and a `done`/`error` frame follows.
      if (onWarning) onWarning(data);
      return null;
    case "done":
      _safeCancel(reader);
      bar.value = 100;
      return { ok: true };
    case "error":
      _safeCancel(reader);
      // Propagate the structured error code (e.g. GATED_NO_TOKEN) so step3 can
      // offer a real way out instead of a Retry that just repeats the error.
      return { ok: false, message: data.message || t("step3_error", state.lang), code: data.code };
    default:
      return null;  // keepalive or unknown type — keep reading
  }
}

/** Walk the SSE frames in a chunk; return the first terminal result, or null. */
function _consumeFrames(frames, bar, info, reader, onWarning) {
  for (const frame of frames) {
    const result = _handleSseFrame(frame, bar, info, reader, onWarning);
    if (result) return result;
  }
  return null;
}

/**
 * Consume a single /installer/download SSE stream and update the given
 * progress widgets. Returns when the server emits `done` or `error`.
 *
 * @param {string} url        — full SSE endpoint URL with query params.
 * @param {HTMLProgressElement} bar
 * @param {HTMLElement} info
 * @param {AbortController} abortCtrl
 * @param {(data: object) => void} [onWarning]  forwarded to `_handleSseFrame`
 *        for non-blocking `warning` frames (INST-002-FE).
 * @returns {Promise<{ok: true} | {ok: false, message: string}>}
 */
async function _streamDownload(url, bar, info, abortCtrl, onWarning) {
  const stall = _createStallController(abortCtrl);

  let response;
  stall.arm();
  try {
    response = await fetch(url, {
      method: "GET",
      headers: { "Accept": "text/event-stream" },
      signal: abortCtrl.signal,
    });
  } catch (err) {
    stall.disarm();
    return _streamErrorResult(stall, err);
  }

  if (!response.ok) {
    stall.disarm();
    return { ok: false, message: `HTTP ${response.status}` };
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";

  try {
    while (true) {
      let chunk;
      try {
        chunk = await reader.read();
      } catch (err) {
        return _streamErrorResult(stall, err);
      }
      const { done, value } = chunk;
      if (done) break;
      stall.arm();

      buf += decoder.decode(value, { stream: true });
      const frames = buf.split("\n\n");
      buf = frames.pop() ?? "";

      const result = _consumeFrames(frames, bar, info, reader, onWarning);
      if (result) return result;
    }
    // Reaching here means the HTTP stream closed (reader `done`) WITHOUT a
    // terminal `done`/`error` SSE frame — a successful transfer always returns
    // earlier via `_handleSseFrame` ("done" → {ok:true}). `bar.value` may read
    // 100 from the last `progress` frame, but without a `done` frame the
    // download never confirmed completion → treat a truncated stream as failure.
    return { ok: false, message: t("step3_error", state.lang) };
  } finally {
    stall.disarm();
  }
}

/**
 * B054: hand the HF token to the sidecar BEFORE a gated download so the gated
 * preflight + snapshot_download authenticate. The token travels in a POST
 * body (never a query param → not in the access log). On failure it renders
 * the error + Retry and returns false so the caller aborts the download.
 *
 * @returns {Promise<boolean>} true if the token was accepted (or none needed).
 */
export async function _handOverHfToken(port, token, errorEl, cancelBtn, retryBtn) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/installer/hf-token`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    });
    if (r.ok) return true;
    errorEl.textContent =
      (t("step3_token_failed", state.lang) || "Could not set the Hugging Face token") +
      `: HTTP ${r.status}`;
  } catch (err) {
    errorEl.textContent = t("step3_connection_lost", state.lang) + ": " + err.message;
  }
  cancelBtn.style.display = "none";
  retryBtn.style.display = "";
  return false;
}

export async function step3() {
  const app = document.getElementById("onboarding-app");
  app.replaceChildren();

  // INST-002-FE: fresh attempt — clear any SHA256 warnings from a prior
  // download so a Retry (which re-runs step3) doesn't show stale notices.
  state.shaWarnings = [];

  const wrapper = document.createElement("div");
  wrapper.className = "step step3";

  const title = document.createElement("h2");
  title.textContent =
    t("step3_downloading", state.lang) + ": " + (state.selectedModel?.name || "");
  wrapper.appendChild(title);

  // LLM model progress block. The embedder (semantic memory) is no longer
  // downloaded from the wizard — the sidecar resolves it on demand on the
  // first RAG query, so the user sees a single progress bar here.
  const llmLabel = (state.selectedModel?.name || t("step3_llm_label", state.lang) || "Model");
  const { bar: llmBar, info: llmInfo } = _buildProgressBlock(wrapper, llmLabel);

  const waitHint = document.createElement("p");
  waitHint.className = "step3-wait-hint";
  waitHint.textContent = t("step3_wait_hint", state.lang) || "";
  wrapper.appendChild(waitHint);

  // INST-002-FE: non-blocking warnings (SHA256_NOT_PINNED) accumulate here as
  // they stream in, below the progress bar.
  const warningsEl = document.createElement("div");
  warningsEl.className = "step3-warnings";
  wrapper.appendChild(warningsEl);

  const errorEl = document.createElement("p");
  errorEl.className = "error-msg";
  wrapper.appendChild(errorEl);

  // Action row: visible Cancel while the download runs, swapped for Retry
  // when the stream errors out (network, stall watchdog, backend error).
  const actions = document.createElement("div");
  actions.className = "step3-actions";
  const cancelBtn = document.createElement("button");
  cancelBtn.className = "btn-secondary";
  cancelBtn.textContent = t("btn_cancel", state.lang) || "Cancel";
  const retryBtn = document.createElement("button");
  retryBtn.className = "btn-primary";
  retryBtn.textContent = t("btn_retry", state.lang) || "Retry";
  retryBtn.style.display = "none";
  retryBtn.addEventListener("click", () => step3());
  actions.appendChild(cancelBtn);
  actions.appendChild(retryBtn);
  wrapper.appendChild(actions);

  app.appendChild(wrapper);

  if (!state.selectedModel) {
    errorEl.textContent = t("step3_error", state.lang);
    cancelBtn.style.display = "none";
    retryBtn.style.display = "";
    return;
  }

  const port = await invoke("get_sidecar_port");
  const { engine, model_id } = state.selectedModel;

  // B054: a gated HF model (mlx/gguf) needs the token loaded into the sidecar
  // env before the download streams, or the preflight fails GATED_NO_TOKEN.
  // Ollama pulls the same model without a token, so skip it there.
  if (state.selectedModel.gated && engine !== "ollama" && state.hfToken) {
    const ok = await _handOverHfToken(port, state.hfToken, errorEl, cancelBtn, retryBtn);
    if (!ok) return;
  }

  // Single AbortController shared by the fetch + the stall watchdog. The
  // backend ThreadPoolExecutor is max_workers=1 so serial downloads here
  // match that contract.
  const abortCtrl = new AbortController();
  cancelBtn.addEventListener("click", () => {
    cancelBtn.disabled = true;
    abortCtrl.abort();
  });

  // INST-002-FE: a SHA256_NOT_PINNED warning frame is non-blocking — record it
  // (so step4 can recap it) and render it inline while the download continues.
  const onWarning = (data) => {
    state.shaWarnings.push({ model: state.selectedModel?.name || "", code: data.code });
    _renderShaWarning(data, warningsEl);
  };

  // Download the chosen LLM model. Embedder fetch deferred to the sidecar.
  const llmUrl = new URL(`http://127.0.0.1:${port}/installer/download`);
  llmUrl.searchParams.set("engine", engine);
  llmUrl.searchParams.set("model_id", model_id);
  const llmResult = await _streamDownload(llmUrl.toString(), llmBar, llmInfo, abortCtrl, onWarning);
  if (!llmResult.ok) {
    errorEl.textContent = llmResult.message;
    waitHint.style.display = "none";
    cancelBtn.style.display = "none";
    retryBtn.style.display = "";
    // B054: a gated model with no/invalid token dead-ends here — Retry alone
    // would just repeat the error. Offer a way back to the model+token step
    // (where the required token input now lives).
    if (llmResult.code === "GATED_NO_TOKEN") {
      const backBtn = document.createElement("button");
      backBtn.className = "btn-secondary";
      backBtn.textContent = t("step3_back_to_select", state.lang) || "← Back to model selection";
      backBtn.addEventListener("click", () => goToStep(2));
      actions.appendChild(backBtn);
    }
    return;
  }

  state.downloadProgress = 100;
  goToStep(4);
}
