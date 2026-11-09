// DOM and Tauri wiring for the flashcp GUI. All decisions live in logic.js.
import {
  formatBytes,
  formatDuration,
  progressFraction,
  progressText,
  stageText,
  baseName,
  expectedConfirm,
  driveTitle,
  nonExfatText,
  selectedDrive,
  jobActive,
  canCopy,
  canErase,
  mergeSources,
  removeSource,
  defaultSelection,
  normalizeDestDir,
  summaryText,
  beginJob,
  applyJobUpdate,
  dismissJob,
  resetForm,
  eraseSourceHint,
} from "./logic.js";

const tauri = window.__TAURI__ || null;
const hasTauri = !!(tauri && tauri.core && tauri.event);

// Preview data for opening index.html in a plain browser.
const DEMO_DRIVES = [
  {
    id: "/demo/block/sdz1",
    device: "/dev/sdz1",
    label: "FAKEUSB",
    fsType: "exfat",
    isExfat: true,
    size: 32_000_000_000,
    mountPoint: "/run/media/demo/FAKEUSB",
    model: "SanDisk Ultra",
    wholeDisk: false,
  },
  {
    id: "/demo/block/sdy1",
    device: "/dev/sdy1",
    label: "",
    fsType: "vfat",
    isExfat: false,
    size: 8_000_000_000,
    mountPoint: null,
    model: "Kingston DataTraveler",
    wholeDisk: false,
  },
];
const DEMO_PATHS = ["/home/demo/Photos", "/home/demo/notes.txt"];

let demoSettings = { autostart: false, notifyOnPlug: true };

let state = {
  drives: [],
  selectedId: null,
  sources: [],
  destDir: "/",
  skipExisting: false,
  job: null,
  settings: { autostart: false, notifyOnPlug: false },
  eraseTyped: "",
  notice: null,
};

const ids = [
  "banner", "drives", "drives-empty", "nonexfat", "dropzone", "add-files", "add-folder",
  "sources-head", "sources-count", "clear-sources", "sources", "dest", "dest-hint",
  "skip-existing", "copy", "progress", "stage", "bar", "progress-text", "done", "summary",
  "wall", "copy-more", "error", "error-message", "dismiss", "notice", "advanced",
  "erase-hint", "erase-expected", "erase-confirm", "erase", "settings", "set-autostart",
  "set-notify",
];
const els = Object.fromEntries(ids.map((id) => [id, document.getElementById(id)]));

// Signatures of the last rendered lists, so focus survives progress updates.
let drivesSig = null;
let sourcesSig = null;

function messageOf(err) {
  if (typeof err === "string") return err;
  if (err && typeof err.message === "string") return err.message;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

// ---- backend access ----------------------------------------------------

function call(cmd, args) {
  if (hasTauri) return tauri.core.invoke(cmd, args);
  return demoInvoke(cmd, args);
}

async function demoInvoke(cmd, args) {
  switch (cmd) {
    case "list_drives":
      return DEMO_DRIVES.map((d) => ({ ...d }));
    case "get_settings":
      return { ...demoSettings };
    case "set_settings":
      demoSettings = { ...args.settings };
      return { ...demoSettings };
    case "take_pending_files":
      return [];
    case "start_copy":
      runDemoJob(args.sources.length, "add");
      return null;
    case "start_erase":
      runDemoJob(1, "build");
      return null;
    default:
      throw new Error(`Preview mode: ${cmd} is not simulated`);
  }
}

// Simulated job for the browser preview: same stage sequence the backend emits.
function runDemoJob(total, op) {
  const planFiles = 40;
  const planBytes = 120_000_000;
  const base = { op, total, index: 0, filesDone: 0, bytesDone: 0, planFiles, planBytes, message: null, summary: null };
  const steps = [
    { stage: "unmounting" },
    { stage: "authorizing" },
  ];
  for (let i = 0; i < total; i++) {
    for (let k = 1; k <= 4; k++) {
      const frac = (i * 4 + k) / (total * 4);
      steps.push({
        stage: "running",
        index: i,
        filesDone: Math.round(planFiles * frac),
        bytesDone: Math.round(planBytes * frac),
      });
    }
  }
  steps.push({ stage: "remounting", filesDone: planFiles, bytesDone: planBytes });
  steps.push({
    stage: "done",
    filesDone: planFiles,
    bytesDone: planBytes,
    summary: { files: planFiles, bytes: planBytes, secs: 1.8, wallSecs: 4.1, mountPoint: DEMO_DRIVES[0].mountPoint },
  });
  steps.forEach((step, n) => {
    setTimeout(() => onJob({ ...base, ...step, planFiles, planBytes }), 350 * (n + 1));
  });
}

// ---- state transitions ---------------------------------------------------

function setState(next) {
  state = next;
  render();
}

function setNotice(text) {
  setState({ ...state, notice: text });
}

function onJob(update) {
  setState(applyJobUpdate(state, update));
}

function failJob(message) {
  const job = state.job || {};
  onJob({
    stage: "error",
    op: job.op || "add",
    index: job.index || 0,
    total: job.total || 0,
    filesDone: job.filesDone || 0,
    bytesDone: job.bytesDone || 0,
    planFiles: job.planFiles || 0,
    planBytes: job.planBytes || 0,
    message,
    summary: null,
  });
}

async function refreshDrives() {
  try {
    const drives = await call("list_drives");
    setState({ ...state, drives, selectedId: defaultSelection(drives, state.selectedId) });
  } catch (err) {
    setNotice(`Could not list drives: ${messageOf(err)}`);
  }
}

function addSources(paths) {
  if (!Array.isArray(paths) || paths.length === 0) return;
  setState({ ...state, sources: mergeSources(state.sources, paths) });
}

async function pickPaths(directory) {
  if (!hasTauri) {
    addSources(DEMO_PATHS);
    return;
  }
  try {
    const picked = await tauri.dialog.open({ multiple: true, directory });
    if (!picked) return;
    addSources(Array.isArray(picked) ? picked : [picked]);
  } catch (err) {
    setNotice(`Could not open the file picker: ${messageOf(err)}`);
  }
}

async function startCopy() {
  if (!canCopy(state)) return;
  const drive = selectedDrive(state);
  const args = {
    driveId: drive.id,
    sources: state.sources.slice(),
    destDir: normalizeDestDir(state.destDir),
    skipExisting: state.skipExisting,
  };
  setState(beginJob(state, "add", state.sources.length));
  try {
    await call("start_copy", args);
  } catch (err) {
    failJob(messageOf(err));
  }
}

async function startErase() {
  if (!canErase(state, state.eraseTyped)) return;
  const drive = selectedDrive(state);
  const args = {
    driveId: drive.id,
    sources: state.sources.slice(),
    confirm: state.eraseTyped,
  };
  setState({ ...beginJob(state, "build", 1), eraseTyped: "" });
  try {
    await call("start_erase", args);
  } catch (err) {
    failJob(messageOf(err));
  }
}

async function changeSetting(patch) {
  const settings = { ...state.settings, ...patch };
  setState({ ...state, settings });
  try {
    const saved = await call("set_settings", {
      settings: { autostart: settings.autostart, notifyOnPlug: settings.notifyOnPlug },
    });
    setState({ ...state, settings: saved });
  } catch (err) {
    setNotice(`Could not save settings: ${messageOf(err)}`);
  }
}

// ---- rendering -------------------------------------------------------------

function render() {
  const busy = jobActive(state);
  const drive = selectedDrive(state);
  const job = state.job;

  // Drives
  renderDrives();
  els["drives-empty"].hidden = state.drives.length > 0;
  const nonExfat = drive && !drive.isExfat;
  els.nonexfat.hidden = !nonExfat;
  if (nonExfat) els.nonexfat.textContent = nonExfatText(drive);

  // Sources
  renderSources(busy);
  els["add-files"].disabled = busy;
  els["add-folder"].disabled = busy;

  // Destination
  const normalized = normalizeDestDir(state.destDir);
  if (els.dest.value !== state.destDir) els.dest.value = state.destDir;
  els.dest.disabled = busy;
  els.dest.setAttribute("aria-invalid", normalized === null ? "true" : "false");
  els["dest-hint"].textContent =
    normalized === null
      ? "Use a folder inside the drive. “..” is not allowed."
      : normalized === "/"
        ? "Copies to the top level of the drive."
        : `Copies into ${normalized}`;
  els["skip-existing"].checked = state.skipExisting;
  els["skip-existing"].disabled = busy;

  // Copy button
  els.copy.textContent = drive ? `Fast copy to ${driveTitle(drive)}` : "Fast copy";
  els.copy.disabled = !canCopy(state);

  // Progress
  els.progress.hidden = !busy;
  if (busy) {
    els.stage.textContent = stageText(job);
    els.bar.value = progressFraction(job);
    els["progress-text"].textContent = progressText(job);
  }

  // Done
  const done = job && job.stage === "done" && job.summary;
  els.done.hidden = !done;
  if (done) {
    els.summary.textContent = summaryText(job.summary);
    const mount = job.summary.mountPoint ? ` Drive is mounted at ${job.summary.mountPoint}.` : "";
    els.wall.textContent = `Whole job including unmount and remount: ${formatDuration(job.summary.wallSecs)}.${mount}`;
  }

  // Error
  const failed = job && job.stage === "error";
  els.error.hidden = !failed;
  if (failed) els["error-message"].textContent = stageText(job);

  // Notice
  els.notice.hidden = !state.notice;
  els.notice.textContent = state.notice || "";

  // Advanced: erase
  els["erase-hint"].textContent = eraseSourceHint(state.sources.length);
  els["erase-expected"].textContent = drive ? expectedConfirm(drive) || "the drive name" : "select a drive first";
  if (els["erase-confirm"].value !== state.eraseTyped) els["erase-confirm"].value = state.eraseTyped;
  els["erase-confirm"].disabled = !drive || busy;
  els.erase.textContent = drive ? `Erase ${driveTitle(drive)} and copy` : "Erase and copy";
  els.erase.disabled = !canErase(state, state.eraseTyped);

  // Settings
  els["set-autostart"].checked = state.settings.autostart;
  els["set-notify"].checked = state.settings.notifyOnPlug;

  // Preview banner
  els.banner.hidden = hasTauri;
}

function renderDrives() {
  const sig = JSON.stringify([state.drives, state.selectedId, jobActive(state)]);
  if (sig === drivesSig) return;
  drivesSig = sig;
  const busy = jobActive(state);
  const container = els.drives;
  container.replaceChildren();
  for (const d of state.drives) {
    const card = document.createElement("label");
    card.className = "drive-card";

    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "drive";
    radio.value = d.id;
    radio.checked = d.id === state.selectedId;
    radio.disabled = busy;
    radio.addEventListener("change", () => {
      if (radio.checked) setState({ ...state, selectedId: d.id, eraseTyped: "" });
    });

    const body = document.createElement("span");
    body.className = "drive-body";

    const title = document.createElement("span");
    title.className = "drive-title";
    title.textContent = driveTitle(d);

    const device = document.createElement("span");
    device.className = "drive-device";
    device.textContent = d.device;

    const meta = document.createElement("span");
    meta.className = "drive-meta";
    const size = document.createElement("span");
    size.textContent = formatBytes(d.size);
    const badge = document.createElement("span");
    badge.className = d.isExfat ? "badge exfat" : "badge";
    badge.textContent = d.fsType ? d.fsType.toUpperCase() : "NO FS";
    meta.append(size, badge);

    body.append(title, device, meta);
    if (d.mountPoint) {
      const mounted = document.createElement("span");
      mounted.className = "drive-mounted";
      mounted.textContent = `mounted at ${d.mountPoint}`;
      body.append(mounted);
    }
    if (d.model) {
      const model = document.createElement("span");
      model.className = "drive-model";
      model.textContent = d.model;
      body.append(model);
    }

    card.append(radio, body);
    container.append(card);
  }
}

function renderSources(busy) {
  const sig = JSON.stringify([state.sources, busy]);
  const head = els["sources-head"];
  head.hidden = state.sources.length === 0;
  els["sources-count"].textContent =
    state.sources.length === 1 ? "1 item selected" : `${state.sources.length} items selected`;
  els["clear-sources"].disabled = busy;
  if (sig === sourcesSig) return;
  sourcesSig = sig;

  const list = els.sources;
  list.replaceChildren();
  state.sources.forEach((path, index) => {
    const name = baseName(path);
    const li = document.createElement("li");

    const text = document.createElement("span");
    text.className = "src-text";
    text.title = path;
    const nameEl = document.createElement("span");
    nameEl.className = "src-name";
    nameEl.textContent = name;
    const pathEl = document.createElement("span");
    pathEl.className = "src-path";
    pathEl.textContent = path;
    text.append(nameEl, pathEl);

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "remove";
    remove.textContent = "×";
    remove.setAttribute("aria-label", `Remove ${name}`);
    remove.disabled = busy;
    remove.addEventListener("click", () => {
      setState({ ...state, sources: removeSource(state.sources, index) });
    });

    li.append(text, remove);
    list.append(li);
  });
}

// ---- wiring ------------------------------------------------------------------

function wire() {
  els["add-files"].addEventListener("click", () => pickPaths(false));
  els["add-folder"].addEventListener("click", () => pickPaths(true));
  els["clear-sources"].addEventListener("click", () => setState({ ...state, sources: [] }));

  els.dest.addEventListener("input", () => {
    state = { ...state, destDir: els.dest.value };
    render();
  });
  els["skip-existing"].addEventListener("change", () => {
    setState({ ...state, skipExisting: els["skip-existing"].checked });
  });

  els.copy.addEventListener("click", startCopy);
  els["copy-more"].addEventListener("click", () => setState(resetForm(state)));
  els.dismiss.addEventListener("click", () => setState(dismissJob(state)));

  els["erase-confirm"].addEventListener("input", () => {
    state = { ...state, eraseTyped: els["erase-confirm"].value };
    render();
  });
  els.erase.addEventListener("click", startErase);

  els["set-autostart"].addEventListener("change", () =>
    changeSetting({ autostart: els["set-autostart"].checked }),
  );
  els["set-notify"].addEventListener("change", () =>
    changeSetting({ notifyOnPlug: els["set-notify"].checked }),
  );
}

function wireDragDrop() {
  if (!hasTauri) return;
  tauri.webview.getCurrentWebview().onDragDropEvent((event) => {
    const payload = event.payload;
    switch (payload.type) {
      case "enter":
      case "over":
        els.dropzone.classList.add("dragover");
        break;
      case "leave":
        els.dropzone.classList.remove("dragover");
        break;
      case "drop":
        els.dropzone.classList.remove("dragover");
        if (!jobActive(state)) addSources(payload.paths || []);
        break;
      default:
        break;
    }
  });
}

async function listenToBackend() {
  if (!hasTauri) return;
  await tauri.event.listen("job", (e) => onJob(e.payload));
  await tauri.event.listen("drives-changed", () => refreshDrives());
  await tauri.event.listen("files-received", (e) => addSources(e.payload));
}

async function init() {
  wire();
  wireDragDrop();
  render();

  await listenToBackend();
  await refreshDrives();

  try {
    const settings = await call("get_settings");
    setState({ ...state, settings });
  } catch (err) {
    setNotice(`Could not read settings: ${messageOf(err)}`);
  }

  try {
    addSources(await call("take_pending_files"));
  } catch (err) {
    setNotice(`Could not read files from the command line: ${messageOf(err)}`);
  }
}

init();
