const $ = id => document.getElementById(id);
const key = document.querySelector('meta[name="meeting-key"]').content;
const sections = [...document.querySelectorAll("main section")];
const unknown = "ไม่ทราบผู้พูด";
const incidentNames = {
  mic_disconnected: "ไมค์หลุดระหว่างอัด",
  storage_full: "พื้นที่จัดเก็บเต็ม",
  write_too_slow: "บันทึกลงดิสก์ตามเสียงไม่ทัน",
  recording_error: "บันทึกเสียงขัดข้อง",
};
const dateFormat = new Intl.DateTimeFormat("th-TH", { dateStyle: "medium", timeStyle: "short" });
let currentPage = "history";
let recording;
let playback;
let pendingRecovery;
let micTest;
let starting = false;
let transcriptView;
let transcriptTimer;

function el(tag, text) {
  const element = document.createElement(tag);
  element.textContent = text;
  return element;
}

function button(text, action) {
  const control = el("button", text);
  control.type = "button";
  control.addEventListener("click", action);
  return control;
}

async function api(path, { method = "GET", body, headers = {} } = {}) {
  const response = await fetch(path, {
    method,
    headers: { "X-Meeting-Key": key, ...headers },
    body,
    signal: AbortSignal.timeout(5000),
  });
  if (!response.ok) {
    const result = await response.json().catch(() => ({}));
    const cause = new Error(result.error || "ติดต่อแอปในเครื่องไม่ได้");
    cause.status = response.status;
    cause.code = result.code;
    throw cause;
  }
  return response.json();
}

function post(path, data, headers = {}) {
  return api(path, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...headers },
    body: data === undefined ? undefined : JSON.stringify(data),
  });
}

function recordingPost(session, action, data) {
  return post(`/api/${session.meeting.id}/${action}`, data, { "X-Recording-Key": session.recordingKey });
}

function error(message = "") {
  $("error").textContent = message;
  $("error").hidden = !message;
}

function report(cause) {
  const messages = {
    NotAllowedError: "เปิดไมค์ไม่ได้ ให้ตรวจสิทธิ์ไมโครโฟนของ Windows และ Microsoft Edge",
    NotFoundError: "ไม่พบไมโครโฟนที่เลือก กรุณาเลือกไมค์ใหม่",
    NotReadableError: "ใช้ไมค์ไม่ได้ ตรวจสอบการเชื่อมต่อหรือปิดโปรแกรมอื่นที่กำลังใช้ไมค์",
    OverconstrainedError: "ไมค์ที่เลือกไม่พร้อมใช้งาน กรุณาเลือกอีกครั้ง",
  };
  error(messages[cause.name] || cause.message || "เกิดข้อผิดพลาด");
}

function time(frames, rate) {
  const seconds = Math.floor(frames / rate);
  return [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60]
    .map(value => String(value).padStart(2, "0")).join(":");
}

function speakerName(meeting, id) {
  return id === null ? unknown : meeting.speakers[id - 1];
}

async function show(page) {
  if (recording && page !== "record") return;
  if (starting && currentPage === "setup" && page !== "record") return;
  error();
  if (page !== "setup") stopMicTest();
  if (page !== "review") {
    clearTimeout(transcriptTimer);
    transcriptView = undefined;
    $("player").pause();
    $("player").removeAttribute("src");
    $("player").load();
  }
  currentPage = page;
  for (const section of sections) section.hidden = section.id !== page;
  for (const control of document.querySelectorAll(".app-header button")) control.disabled = page === "record";
  if (page === "history") await historyList();
  if (page === "setup") await listMicrophones();
  if (page === "settings") {
    const [files, model] = await Promise.all([api("/api/config"), api("/api/asr-config")]);
    if (currentPage !== "settings") return;
    $("storage-dir").textContent = files.directory;
    renderModelSettings(model);
  }
}

async function historyList() {
  const list = $("meeting-list");
  const meetings = await api("/api/meetings");
  const busy = meetings.some(meeting => meeting.recording);
  list.replaceChildren();
  if (!meetings.length) list.append(el("li", "ยังไม่มีการประชุม เลือก “ประชุมใหม่” เพื่อเริ่มบันทึก"));
  for (const meeting of meetings) {
    const item = el("li", "");
    const summary = el("div", "");
    summary.append(
      el("strong", meeting.title),
      el("small", dateFormat.format(new Date(meeting.created_at * 1000))
        + " · " + time(meeting.frames, meeting.sample_rate)
        + " · ผู้พูด " + meeting.speakers.length + " คน"),
    );
    const action = meeting.recording
      ? el("small", "กำลังอัดในอีกหน้าต่าง")
      : meeting.finished
        ? button("เปิดฟัง", () => void review(meeting).catch(report))
        : busy
          ? el("small", "กู้ได้หลังหยุดการอัด")
          : button("ตรวจไฟล์ที่ค้าง", () => void recoverPage(meeting).catch(report));
    item.append(summary, action);
    list.append(item);
  }
}

async function listMicrophones() {
  if (!navigator.mediaDevices) throw new Error("เบราว์เซอร์นี้เปิดไมโครโฟนไม่ได้");
  const select = $("mic-select");
  const previous = select.value;
  const devices = await navigator.mediaDevices.enumerateDevices();
  select.replaceChildren(new Option("ไมค์เริ่มต้นของ Windows", ""));
  let number = 0;
  for (const device of devices) {
    if (device.kind !== "audioinput" || !device.deviceId || device.deviceId === "default") continue;
    select.add(new Option(device.label || "ไมโครโฟน " + ++number, device.deviceId));
  }
  if ([...select.options].some(option => option.value === previous)) select.value = previous;
}

function openMic() {
  const device = $("mic-select").value;
  return navigator.mediaDevices.getUserMedia({
    audio: device ? { deviceId: { exact: device }, channelCount: 1 } : { channelCount: 1 },
  });
}

function stopMicTest() {
  if (!micTest) return;
  cancelAnimationFrame(micTest.animation);
  micTest.stream.getTracks().forEach(track => track.stop());
  void micTest.context.close();
  micTest = undefined;
  $("test-level").value = 0;
  $("test-mic").textContent = "ทดสอบไมค์";
}

async function testMic() {
  if (micTest) { stopMicTest(); return; }
  const control = $("test-mic");
  control.disabled = true;
  const context = new AudioContext();
  let stream;
  try {
    stream = await openMic();
    if (currentPage !== "setup" || starting) return;
    await context.resume();
    if (currentPage !== "setup" || starting) return;
    const analyser = context.createAnalyser();
    analyser.fftSize = 1024;
    context.createMediaStreamSource(stream).connect(analyser);
    const signal = new Float32Array(analyser.fftSize);
    micTest = { context, stream, animation: 0 };
    function meter() {
      analyser.getFloatTimeDomainData(signal);
      $("test-level").value = signal.reduce((peak, sample) => Math.max(peak, Math.abs(sample)), 0);
      micTest.animation = requestAnimationFrame(meter);
    }
    $("test-mic").textContent = "หยุดทดสอบไมค์";
    meter();
    await listMicrophones();
  } finally {
    if (micTest?.context !== context) {
      stream?.getTracks().forEach(track => track.stop());
      await context.close().catch(() => {});
    }
    control.disabled = false;
  }
}

async function start(event) {
  event.preventDefault();
  if (starting || recording) return;
  starting = true;
  $("start").disabled = true;
  $("test-mic").disabled = true;
  error();
  stopMicTest();
  const inputs = new FormData(event.currentTarget);
  const title = inputs.get("title").trim();
  const names = inputs.getAll("speaker").map(name => name.trim());
  const last = names.findLastIndex(Boolean);
  const speakers = last < 0 ? [] : names.slice(0, last + 1);
  let context;
  let stream;
  try {
    if (!title || !speakers.length || speakers.some(name => !name)) {
      throw new Error("กรุณากรอกชื่อการประชุมและชื่อผู้พูดเรียงจากปุ่ม 1 โดยไม่เว้นช่อง");
    }
    context = new AudioContext();
    stream = await openMic();
    if (currentPage !== "setup") return;
    await context.audioWorklet.addModule("/capture.js");
    await context.resume();
    if (currentPage !== "setup") return;
    const node = new AudioWorkletNode(context, "capture-pcm", { channelCount: 1, channelCountMode: "explicit" });
    const source = context.createMediaStreamSource(stream);
    const mute = context.createGain();
    mute.gain.value = 0;
    const { meeting, recording_key: recordingKey } = await post("/api/start", {
      title, speakers, sample_rate: context.sampleRate,
    });
    if (currentPage !== "setup") {
      await recordingPost({ meeting, recordingKey }, "abandon");
      return;
    }
    const session = {
      meeting, recordingKey, stream, context, node, parts: [], pendingBytes: 0, bufferedBytes: 0, frames: 0, sent: 0,
      chain: Promise.resolve(), speaker: null, changes: [], markers: [], stopping: false, failed: false,
    };
    recording = session;
    session.heartbeat = setInterval(() => {
      if (!session.stopping && !session.failed) void recordingPost(session, "heartbeat").catch(() => {});
    }, 5000);
    node.port.onmessage = ({data}) => receive(session, data);
    node.addEventListener("processorerror", () => void interrupt(session, "ไมค์หยุดส่งเสียง"));
    stream.getAudioTracks().forEach(track => track.addEventListener("ended", () => void stop("mic_disconnected")));
    source.connect(node).connect(mute).connect(context.destination);
    $("record-title").textContent = meeting.title;
    $("clock").textContent = "00:00:00";
    $("record-level").value = 0;
    $("record-info").textContent = "";
    $("stop").disabled = false;
    $("mark").disabled = false;
    const keypad = $("speaker-buttons");
    keypad.replaceChildren();
    meeting.speakers.forEach((name, index) => {
      const id = index + 1;
      const control = button("", () => changeSpeaker(id));
      control.dataset.speaker = id;
      control.setAttribute("aria-keyshortcuts", String(id));
      control.append(el("kbd", id), document.createTextNode(" " + name));
      keypad.append(control);
    });
    updateSpeaker(session);
    renderTimes(session, false);
    await show("record");
    keypad.querySelector("button")?.focus();
  } catch (cause) {
    if (recording) await interrupt(recording, cause.message);
    else report(cause);
  } finally {
    if (!recording || recording.context !== context) {
      stream?.getTracks().forEach(track => track.stop());
      if (context && context.state !== "closed") await context.close().catch(() => {});
    }
    starting = false;
    $("start").disabled = false;
    $("test-mic").disabled = false;
  }
}

function receive(session, data) {
  if (recording !== session || session.failed) return;
  if (data.stopped) { session.ack?.(); return; }
  const bytes = new Uint8Array(data.pcm);
  session.parts.push(bytes);
  session.pendingBytes += bytes.length;
  session.bufferedBytes += bytes.length;
  session.frames += bytes.length / 2;
  if (session.bufferedBytes > session.meeting.sample_rate * 2 * 12) {
    void interrupt(session, incidentNames.write_too_slow, "write_too_slow");
    return;
  }
  $("clock").textContent = time(session.frames, session.meeting.sample_rate);
  $("record-level").value = data.peak;
  if (session.pendingBytes >= session.meeting.sample_rate / 2) flush(session);
}

function queue(session, operation) {
  session.chain = session.chain.then(() => {
    if (!session.failed) return operation();
  }).catch(cause => {
    if (cause.name === "TimeoutError") {
      return interrupt(session, incidentNames.write_too_slow, "write_too_slow");
    }
    return interrupt(session, cause.message, cause.code === "storage_full" ? cause.code : "recording_error");
  });
}

function flush(session) {
  if (!session.pendingBytes) return;
  const pcm = new Uint8Array(session.pendingBytes);
  let index = 0;
  for (const part of session.parts) { pcm.set(part, index); index += part.length; }
  session.parts.length = 0;
  session.pendingBytes = 0;
  const offset = session.sent;
  session.sent += pcm.length / 2;
  queue(session, async () => {
    const upload = () => api(`/api/${session.meeting.id}/audio`, {
      method: "POST",
      headers: {
        "Content-Type": "application/octet-stream",
        "X-Frame-Offset": String(offset),
        "X-Recording-Key": session.recordingKey,
      },
      body: pcm,
    });
    try {
      await upload();
    } catch (cause) {
      // Retry an uncertain delivery. An explicit server error applies to the chunk as sent.
      if (cause.status !== undefined || session.failed) throw cause;
      await upload();
    }
    session.bufferedBytes -= pcm.length;
  });
}

function updateSpeaker(session) {
  $("current-speaker").textContent = speakerName(session.meeting, session.speaker);
  $("unknown").setAttribute("aria-pressed", String(session.speaker === null));
  for (const control of $("speaker-buttons").children) {
    control.setAttribute("aria-pressed", String(Number(control.dataset.speaker) === session.speaker));
  }
}

function changeSpeaker(id) {
  const session = recording;
  if (!session || session.stopping || session.speaker === id) return;
  session.speaker = id;
  const event = { frame: session.frames, speaker: id };
  const previous = session.changes.at(-1);
  if (previous?.frame === event.frame) session.changes.pop();
  if ((session.changes.at(-1)?.speaker ?? null) !== id) session.changes.push(event);
  updateSpeaker(session);
  renderTimes(session, false);
  flush(session);
  queue(session, () => recordingPost(session, "speaker", event));
}

function mark() {
  const session = recording;
  if (!session || session.stopping || session.markers.at(-1) === session.frames) return;
  const frame = session.frames;
  session.markers.push(frame);
  $("record-info").textContent = "ปักช่วง " + time(frame, session.meeting.sample_rate) + " แล้ว";
  renderTimes(session, false);
  flush(session);
  queue(session, () => recordingPost(session, "mark", frame));
}

async function closeMic(session) {
  clearInterval(session.heartbeat);
  session.stream.getTracks().forEach(track => track.stop());
  if (session.context.state !== "closed") await session.context.close().catch(() => {});
}

async function interrupt(session, message, reason = "recording_error") {
  if (session.failed) return;
  session.failed = true;
  await closeMic(session);
  if (recording !== session) return;
  recording = undefined;
  await recordingPost(session, "abandon", reason).catch(() => {});
  try {
    await show("history");
    error("การบันทึกหยุด: " + message + " · เลือกตรวจไฟล์ที่ค้างเพื่อกู้เสียง");
  } catch {
    error("การบันทึกหยุด: " + message + " · เปิดแอปอีกครั้งเพื่อกู้เสียง");
  }
}

async function stop(reason) {
  const session = recording;
  if (!session || session.stopping) return;
  session.stopping = true;
  $("stop").disabled = true;
  $("mark").disabled = true;
  $("record-info").textContent = "กำลังบันทึกข้อมูลเสียงช่วงท้าย…";
  const ack = new Promise(resolve => { session.ack = resolve; });
  session.node.port.postMessage("stop");
  let timeout;
  try {
    const stopped = await Promise.race([
      ack.then(() => true),
      new Promise(resolve => { timeout = setTimeout(() => resolve(false), 1500); }),
    ]);
    await closeMic(session);
    flush(session);
    await session.chain;
    if (session.failed) return;
    const incident = reason || (!stopped ? "recording_error" : undefined);
    if (incident) await recordingPost(session, "incident", incident);
    const meeting = await recordingPost(session, "finish");
    recording = undefined;
    await review(meeting);
    if (!stopped) error("เก็บเสียงที่ได้รับแล้ว แต่อาจขาดช่วงท้าย");
  } catch (cause) {
    if (recording === session) await interrupt(session, cause.message, cause.code === "storage_full" ? cause.code : reason);
    else report(cause);
  } finally {
    clearTimeout(timeout);
  }
}

function renderTimes(session, isReview) {
  const meeting = session.meeting;
  const markers = $(isReview ? "review-markers" : "live-markers");
  const events = $(isReview ? "review-events" : "event-list");
  const display = frame => time(frame, meeting.sample_rate);
  const stamp = frame => isReview
    ? button(display(frame), () => { $("player").currentTime = frame / meeting.sample_rate; })
    : el("time", display(frame));
  markers.replaceChildren();
  events.replaceChildren();
  const shown = isReview ? session.markers : session.markers.slice(-6);
  if (!shown.length) markers.append(el("li", "ยังไม่ได้ปัก"));
  for (const frame of shown) {
    const item = el("li", "");
    item.append(stamp(frame));
    markers.append(item);
  }
  const changes = isReview ? session.changes : session.changes.slice(-8);
  if (!changes.length) events.append(el("li", "ยังไม่มีการเปลี่ยนผู้พูด"));
  for (const event of changes) {
    const item = el("li", "");
    item.append(stamp(event.frame), el("span", speakerName(meeting, event.speaker)));
    events.append(item);
  }
}

async function review(meeting) {
  playback = meeting;
  await show("review");
  $("review-title").textContent = meeting.title;
  $("review-incident").textContent = incidentText(meeting);
  $("review-incident").hidden = !meeting.incident;
  const src = `/api/${meeting.id}/audio?key=${key}`;
  $("player").src = src;
  $("download-wav").href = src;
  $("download-wav").download = "meeting-" + meeting.id + ".wav";
  renderTimes({ meeting, changes: meeting.changes, markers: meeting.markers }, true);
  clearTimeout(transcriptTimer);
  transcriptView = { meeting, path: `/api/${meeting.id}/transcript` };
  $("transcribe").disabled = true;
  $("transcribe").textContent = "ถอดเสียงภาษาไทย";
  $("transcript-progress").hidden = true;
  $("transcript-status").textContent = "กำลังเปิดผลถอดเสียง…";
  $("transcript-lines").replaceChildren();
  void refreshTranscript(transcriptView);
}

async function refreshTranscript(view, start = false) {
  try {
    const result = start ? await post(view.path) : await api(view.path);
    if (view !== transcriptView) return;
    const { meeting } = view;
    const running = result.state === "running";
    const complete = result.state === "complete";
    const progress = $("transcript-progress");
    const status = $("transcript-status");
    const control = $("transcribe");
    control.disabled = running || !meeting.frames;
    control.textContent = complete || result.state === "failed" ? "ถอดเสียงอีกครั้ง" : "ถอดเสียงภาษาไทย";
    progress.hidden = !running;
    if (running) {
      const percent = Math.min(99, Math.floor(100 * result.processed_frames / meeting.frames));
      progress.value = percent;
      const message = result.processed_frames ? `กำลังถอดเสียงบนเครื่อง ${percent}%` : "กำลังเตรียมโมเดลและถอดเสียงบนเครื่อง…";
      if (status.textContent !== message) status.textContent = message;
      transcriptTimer = setTimeout(() => void refreshTranscript(view), 700);
      return;
    }
    if (complete) {
      const list = $("transcript-lines");
      list.replaceChildren();
      const { segments, sample_rate: rate } = result.transcript;
      status.textContent = segments.length ? `ถอดเสียงเสร็จแล้ว · ${segments.length} ช่วง` : "ถอดเสียงเสร็จแล้ว · ไม่พบข้อความใน WAV";
      for (const segment of segments) {
        const startTime = segment.start_frame / rate;
        const tenth = Math.floor(10 * (segment.start_frame % rate) / rate);
        const stamp = `${time(segment.start_frame, rate)}.${tenth}`;
        const item = el("li", "");
        const line = el("div", "");
        const seek = button(stamp, () => { $("player").currentTime = startTime; });
        seek.setAttribute("aria-label", `เลื่อนไปเวลา ${stamp}`);
        line.append(seek, el("strong", speakerName(meeting, segment.speaker)));
        item.append(line, el("p", segment.text));
        list.append(item);
      }
    } else if (result.state === "failed") {
      status.textContent = result.message;
    } else {
      status.textContent = meeting.frames ? "ใช้ OpenThai FP32 เพื่อถอดไฟล์ WAV ที่บันทึกไว้" : "การประชุมนี้ไม่มีเสียงให้ถอด";
    }
  } catch (cause) {
    if (view !== transcriptView) return;
    $("transcript-status").textContent = cause.message;
    $("transcript-progress").hidden = true;
    $("transcribe").disabled = !view.meeting.frames;
  }
}

function renderModelSettings(model) {
  $("model-folder").value = model.folder.startsWith("\\\\?\\") ? model.folder.slice(4) : model.folder;
  $("model-status").textContent = model.message;
}

async function recoverPage(meeting) {
  pendingRecovery = meeting;
  await show("recovery");
  $("recovery-title").textContent = meeting.title;
  $("available-audio").textContent = "พบเสียงบนดิสก์ " + time(meeting.frames, meeting.sample_rate)
    + " · ช่วงท้ายอาจบันทึกไม่ครบ";
  $("recovery-incident").textContent = incidentText(meeting);
  $("recovery-incident").hidden = !meeting.incident;
}

function incidentText(meeting) {
  if (!meeting.incident) return "";
  const { reason, frame } = meeting.incident;
  return "การบันทึกหยุด: " + (incidentNames[reason] || incidentNames.recording_error)
    + " · เสียงที่เก็บได้ถึง " + time(frame, meeting.sample_rate);
}

function downloadJson() {
  if (!playback) return;
  const url = URL.createObjectURL(new Blob([JSON.stringify(playback, null, 2)], { type: "application/json" }));
  const link = el("a", "");
  link.href = url;
  link.download = "meeting-" + playback.id + ".json";
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

for (const control of document.querySelectorAll("[data-page]")) {
  control.addEventListener("click", () => void show(control.dataset.page).catch(report));
}
$("setup-form").addEventListener("submit", start);
$("test-mic").addEventListener("click", () => { error(); void testMic().catch(report); });
$("mic-select").addEventListener("change", stopMicTest);
$("unknown").addEventListener("click", () => changeSpeaker(null));
$("mark").addEventListener("click", mark);
$("stop").addEventListener("click", () => void stop());
$("recover").addEventListener("click", async () => {
  if (!pendingRecovery) return;
  $("recover").disabled = true;
  try { await review(await post(`/api/${pendingRecovery.id}/recover`)); }
  catch (cause) { report(cause); }
  finally { $("recover").disabled = false; }
});
$("export-json").addEventListener("click", downloadJson);
$("transcribe").addEventListener("click", () => {
  if (!transcriptView) return;
  $("transcribe").disabled = true;
  void refreshTranscript(transcriptView, true);
});
$("asr-settings").addEventListener("submit", async event => {
  event.preventDefault();
  const control = event.submitter;
  if (control) control.disabled = true;
  try { renderModelSettings(await post("/api/asr-config", $("model-folder").value.trim())); }
  catch (cause) { $("model-status").textContent = cause.message; }
  finally { if (control) control.disabled = false; }
});
$("quit").addEventListener("click", async () => {
  try {
    await post("/api/quit");
    document.querySelector("main").replaceChildren(el("p", "ปิดแอปแล้ว คุณสามารถปิดหน้าต่างนี้ได้"));
    for (const control of document.querySelectorAll(".app-header button")) control.disabled = true;
  } catch (cause) { report(cause); }
});
document.addEventListener("keydown", event => {
  if (!recording || event.repeat || event.ctrlKey || event.altKey || event.metaKey
      || event.target.matches("input, textarea, select, [contenteditable]")) return;
  if (event.key.toLowerCase() === "m") { mark(); return; }
  if (/^[1-5]$/.test(event.key) && Number(event.key) <= recording.meeting.speakers.length) {
    changeSpeaker(Number(event.key));
  }
});
window.addEventListener("beforeunload", event => {
  if (recording) { event.preventDefault(); event.returnValue = ""; }
});
void show("history").catch(report);
