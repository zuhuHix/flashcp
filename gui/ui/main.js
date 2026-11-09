// DOM and Tauri wiring for the flashcp GUI. All decisions live in logic.js.
import {
  formatBytes,
  formatDuration,
  progressFraction,
  progressText,
  stageText,
  baseName,
  confirmHint,
  trustState,
  isTrusted,
  canTrust,
  trustNotice,
  trustDetails,
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
  selectDrive,
  openTrust,
  closeTrust,
  canPull,
  pullArgs,
  pullDestination,
  normalizeFrom,
  parentFrom,
  breadcrumbs,
  folderRows,
  browseKey,
  setFolders,
  setMode,
  setPullFrom,
  setPullDest,
  resetPullDest,
  applyPullRequest,
  pullSummaryText,
  showCollision,
  clearCollision,
  collisionText,
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
    serial: "FAKE0001",
    identity: { keyKind: "serial", key: "FAKE0001", wwn: "-", model: "Ultra", diskSize: 32_015_122_432 },
    trusted: true,
  },
  {
    id: "/demo/block/sdy1",
    device: "/dev/sdy1",
    label: "OLDSTICK",
    fsType: "vfat",
    isExfat: false,
    size: 8_000_000_000,
    mountPoint: null,
    model: "Kingston DataTraveler",
    wholeDisk: false,
    serial: "OLD0002",
    identity: {
      keyKind: "serial",
      key: "OLD0002",
      wwn: "0x50014ee2b5a1c3d7",
      model: "DataTraveler",
      diskSize: 8_004_829_184,
    },
    trusted: false,
  },
];
const DEMO_PATHS = ["/home/demo/Photos", "/home/demo/notes.txt"];
// Folder tree of the demo drive, same shape as the fake tree in the backend.
const DEMO_TREE = {
  "": ["DCIM", "Documents", "Music"],
  DCIM: ["100CANON", "101CANON"],
  Documents: ["2026"],
};
const DEMO_DOWNLOADS = "/home/demo/Downloads";

let demoSettings = { autostart: false, notifyOnPlug: true };
// ?pull=conflict: the first demo pull ends with the "already exist" error.
const demoParams = new URLSearchParams(window.location.search);
let demoConflictPending = !hasTauri && demoParams.get("pull") === "conflict";

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
  trustOpen: false,
  trustBusy: false,
  trustError: null,
  mode: "copy",
  downloads: null,
  pullFrom: "",
  pullFolders: null,
  pullDestOverride: null,
  pullCollision: null,
};

const ids = [
  "banner", "drives", "drives-empty", "nonexfat", "trust", "trust-text", "trust-open-row",
  "trust-open", "trust-confirm", "trust-facts", "trust-yes", "trust-cancel", "trust-error",
  "mode-copy", "mode-pull",
  "copy-files", "dropzone", "add-files", "add-folder",
  "sources-head", "sources-count", "clear-sources", "sources", "copy-dest", "dest", "dest-hint",
  "skip-existing", "copy-actions", "copy", "progress", "stage", "bar", "progress-text", "done",
  "summary", "done-warning", "wall", "copy-more", "error", "error-message", "dismiss", "notice",
  "advanced", "erase-hint", "erase-label", "erase-confirm", "erase", "settings", "set-autostart",
  "set-notify",
  "pull-source", "pull-browser", "crumbs", "pull-up", "pull-folder-status", "pull-folders",
  "pull-typed", "pull-from", "pull-typed-hint", "pull-current", "pull-whole",
  "pull-dest-card", "pull-dest-path", "pull-change", "pull-default",
  "collision", "collision-title", "collision-text", "collision-skip", "collision-overwrite",
  "collision-cancel", "pull-actions", "pull-start",
];
const els = Object.fromEntries(ids.map((id) => [id, document.getElementById(id)]));

// Signatures of the last rendered lists, so focus survives progress updates.
let drivesSig = null;
let sourcesSig = null;
let folderSig = null;
// Folder listing currently being fetched (a folderKey), so it is requested once.
let foldersInFlight = null;
// Whether the collision panel was open on the last render (to move focus into it once).
let collisionOpen = false;

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
    case "take_pending_pull":
      return null;
    case "pull_defaults":
      return { downloads: DEMO_DOWNLOADS };
    case "dest_status": {
      // Demo rule: a destination ending in "/Existing" already has files; any other path is empty.
      const exists = String(args.path || "").endsWith("/Existing");
      return { exists, isDir: exists, empty: !exists };
    }
    case "list_drive_folders": {
      const drive = DEMO_DRIVES.find((d) => d.id === args.driveId);
      if (!drive || !drive.mountPoint) {
        throw new Error("The drive is not mounted, type a folder name instead");
      }
      return [...(DEMO_TREE[args.rel] || [])];
    }
    case "start_copy":
      runDemoJob(args.sources.length, "add");
      return null;
    case "start_erase":
      runDemoJob(1, "build");
      return null;
    case "start_pull": {
      if (demoConflictPending && args.existing === "fail") {
        demoConflictPending = false;
        runDemoJob(1, "pull", { dest: args.dest, conflict: true });
      } else {
        runDemoJob(1, "pull", { dest: args.dest });
      }
      return null;
    }
    case "trust_drive": {
      // Simulated: the real helper asks for admin authentication here.
      await new Promise((resolve) => setTimeout(resolve, 400));
      const drive = DEMO_DRIVES.find((d) => d.id === args.driveId);
      if (!drive) throw new Error("Preview mode: unknown drive");
      drive.trusted = true;
      return null;
    }
    default:
      throw new Error(`Preview mode: ${cmd} is not simulated`);
  }
}

// Simulated job for the browser preview: same stage sequence the backend emits.
// extra.conflict: stop before copying with the "already exist" error (backend behaviour).
function runDemoJob(total, op, extra = {}) {
  const planFiles = 40;
  const planBytes = 120_000_000;
  const base = { op, total, index: 0, filesDone: 0, bytesDone: 0, planFiles, planBytes, message: null, summary: null, conflict: false };
  let steps;
  if (extra.conflict) {
    steps = [
      { stage: "unmounting" },
      { stage: "remounting" },
      {
        stage: "error",
        conflict: true,
        message: `${planFiles} file(s) already exist in the destination ${extra.dest}, e.g. a, b; pass --skip-existing or --overwrite`,
      },
    ];
  } else {
    steps = op === "pull" ? [{ stage: "unmounting" }] : [{ stage: "unmounting" }, { stage: "authorizing" }];
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
      summary: {
        files: planFiles,
        bytes: planBytes,
        secs: 1.8,
        wallSecs: 4.1,
        mountPoint: op === "pull" ? null : DEMO_DRIVES[0].mountPoint,
        dest: op === "pull" ? extra.dest : null,
        warning: null,
      },
    });
  }
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
  let next = applyJobUpdate(state, update);
  // A pull stopped by the collision check: offer skip / overwrite / cancel instead of a plain error.
  if (update && update.op === "pull" && update.stage === "error" && update.conflict === true) {
    next = showCollision(next, pullDestination(state), "conflict");
  }
  setState(next);
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
    const next = selectDrive({ ...state, drives }, defaultSelection(drives, state.selectedId));
    setState(next);
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

// choice: null for a first try (the destination is checked before anything is written),
// or "skip" / "overwrite" from the collision panel.
async function startPull(choice) {
  if (!canPull(state)) return;
  const dest = pullDestination(state);
  if (choice === null) {
    let status;
    try {
      status = await call("dest_status", { path: dest });
    } catch (err) {
      setNotice(`Could not check ${dest}: ${messageOf(err)}`);
      return;
    }
    if (status.exists && !status.isDir) {
      setNotice(`${dest} is a file, not a folder. Choose another folder with Change…`);
      return;
    }
    if (status.exists && !status.empty) {
      setState(showCollision(state, dest, "preflight"));
      return;
    }
  }
  const args = pullArgs(state, choice || "fail");
  if (!args) return;
  setState(beginJob(clearCollision(state), "pull", 1));
  try {
    await call("start_pull", args);
  } catch (err) {
    failJob(messageOf(err));
  }
}

function chooseCollision(choice) {
  if (!state.pullCollision || jobActive(state)) return;
  setState(clearCollision(dismissJob(state)));
  startPull(choice);
}

function cancelCollision() {
  setState(clearCollision(dismissJob(state)));
}

async function changePullDest() {
  if (jobActive(state)) return;
  const current = pullDestination(state) || "";
  if (!hasTauri) {
    const typed = window.prompt(
      "Preview only: the folder to copy into, as a full path. Ending it with /Existing shows the collision panel.",
      current,
    );
    if (typed) setState(setPullDest(state, typed));
    return;
  }
  try {
    const picked = await tauri.dialog.open({ directory: true, multiple: false, defaultPath: current || undefined });
    if (typeof picked === "string" && picked) setState(setPullDest(state, picked));
  } catch (err) {
    setNotice(`Could not open the folder picker: ${messageOf(err)}`);
  }
}

function navigatePull(rel) {
  setState(setPullFrom(state, rel));
  focusFolderBrowser();
}

// Keep keyboard focus in the browser after a navigation rebuilds the list.
function focusFolderBrowser() {
  const up = els["pull-up"];
  if (!up.disabled) {
    up.focus();
    return;
  }
  const first = els["pull-folders"].querySelector("button");
  if (first && !first.disabled) first.focus();
}

// Fetch the folder list for the current drive and folder, once per key.
function maybeLoadFolders() {
  if (state.mode !== "pull") return;
  const key = browseKey(state);
  if (key === null) return;
  if ((state.pullFolders && state.pullFolders.key === key) || foldersInFlight === key) return;
  const drive = selectedDrive(state);
  const rel = normalizeFrom(state.pullFrom);
  foldersInFlight = key;
  const finish = () => {
    if (foldersInFlight === key) foldersInFlight = null;
  };
  call("list_drive_folders", { driveId: drive.id, rel }).then(
    (names) => {
      finish();
      if (browseKey(state) === key) setState(setFolders(state, key, names, null));
    },
    (err) => {
      finish();
      if (browseKey(state) === key) setState(setFolders(state, key, [], messageOf(err)));
    },
  );
}

function showTrustPanel() {
  const drive = selectedDrive(state);
  if (!canTrust(drive) || jobActive(state) || state.trustBusy) return;
  setState(openTrust(state));
}

function cancelTrust() {
  if (state.trustBusy) return;
  setState(closeTrust(state));
}

async function startTrust() {
  const drive = selectedDrive(state);
  if (!canTrust(drive) || jobActive(state) || state.trustBusy) return;
  setState({ ...state, trustBusy: true, trustError: null });
  try {
    await call("trust_drive", { driveId: drive.id });
    setState(closeTrust(state));
    await refreshDrives();
  } catch (err) {
    setState({ ...state, trustBusy: false, trustError: messageOf(err) });
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

function onPullRequest(req) {
  setState(applyPullRequest(state, req));
}

// ---- rendering -------------------------------------------------------------

function render() {
  const busy = jobActive(state);
  const drive = selectedDrive(state);
  const job = state.job;
  const pull = state.mode === "pull";
  const collisionShown = pull && !!state.pullCollision;

  // Direction switch
  els["mode-copy"].setAttribute("aria-pressed", String(!pull));
  els["mode-pull"].setAttribute("aria-pressed", String(pull));

  // Drives
  renderDrives();
  els["drives-empty"].hidden = state.drives.length > 0;
  const nonExfat = drive && !drive.isExfat;
  els.nonexfat.hidden = !nonExfat;
  if (nonExfat) els.nonexfat.textContent = nonExfatText(drive);

  // Copy to USB: sources and destination
  els["copy-files"].hidden = pull;
  els["copy-dest"].hidden = pull;
  els["copy-actions"].hidden = pull;
  els.advanced.hidden = pull;
  renderSources(busy);
  els["add-files"].disabled = busy;
  els["add-folder"].disabled = busy;

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

  // Copy from USB: source on the drive, destination, collision panel, start
  els["pull-source"].hidden = !pull;
  els["pull-dest-card"].hidden = !pull;
  els["pull-actions"].hidden = !pull;
  if (pull) renderPull(busy);
  renderCollision(collisionShown);

  // Trust: shown while the selected drive is not trusted
  renderTrust(drive, busy);

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
    const isPull = job.op === "pull";
    els.summary.textContent = isPull ? pullSummaryText(job.summary) : summaryText(job.summary);
    const warning = isPull && job.summary.warning ? job.summary.warning : "";
    els["done-warning"].hidden = !warning;
    els["done-warning"].textContent = warning;
    const mount = job.summary.mountPoint ? ` Drive is mounted at ${job.summary.mountPoint}.` : "";
    els.wall.textContent = `Whole job including unmount and remount: ${formatDuration(job.summary.wallSecs)}.${mount}`;
  }

  // Error (a collision is shown in its own panel instead)
  const failed = job && job.stage === "error";
  els.error.hidden = !failed || collisionShown;
  if (failed) els["error-message"].textContent = stageText(job);

  // Notice
  els.notice.hidden = !state.notice;
  els.notice.textContent = state.notice || "";

  // Advanced: erase
  els["erase-hint"].textContent = eraseSourceHint(state.sources.length);
  els["erase-label"].textContent = confirmHint(drive);
  if (els["erase-confirm"].value !== state.eraseTyped) els["erase-confirm"].value = state.eraseTyped;
  els["erase-confirm"].disabled = !drive || busy || !isTrusted(drive);
  els.erase.textContent = drive ? `Erase ${driveTitle(drive)} and copy` : "Erase and copy";
  els.erase.disabled = !canErase(state, state.eraseTyped);

  // Settings
  els["set-autostart"].checked = state.settings.autostart;
  els["set-notify"].checked = state.settings.notifyOnPlug;

  // Preview banner
  els.banner.hidden = hasTauri;

  maybeLoadFolders();
}

function renderPull(busy) {
  const key = browseKey(state);
  const entry = key !== null && state.pullFolders && state.pullFolders.key === key ? state.pullFolders : null;
  // The browser needs a mounted drive and a listing that did not fail; otherwise type the folder.
  const browsing = key !== null && !(entry && entry.error);
  const from = normalizeFrom(state.pullFrom);

  els["pull-browser"].hidden = !browsing;
  els["pull-typed"].hidden = browsing;
  if (browsing) renderFolderBrowser(entry, from || "", busy);

  if (els["pull-from"].value !== state.pullFrom) els["pull-from"].value = state.pullFrom;
  els["pull-from"].disabled = busy;
  els["pull-from"].setAttribute("aria-invalid", from === null ? "true" : "false");
  els["pull-typed-hint"].textContent = entry && entry.error
    ? `Folders can't be listed (${entry.error}). Type a folder name, or leave it empty for the whole drive.`
    : "The drive is not mounted, so folders can't be listed. Type a folder name, or leave it empty for the whole drive.";

  els["pull-current"].textContent =
    from === null
      ? "That folder name is not allowed. Use a path inside the drive, without “..”."
      : from === ""
        ? "Copying the whole drive."
        : `Copying only the folder ${from}.`;
  els["pull-whole"].hidden = from === "";
  els["pull-whole"].disabled = busy;

  const dest = pullDestination(state);
  els["pull-dest-path"].textContent = dest || "No default folder yet.";
  els["pull-change"].disabled = busy;
  els["pull-default"].hidden = !state.pullDestOverride;
  els["pull-default"].disabled = busy;

  els["pull-start"].disabled = !canPull(state);
}

function renderFolderBrowser(entry, from, busy) {
  const crumbs = breadcrumbs(from);
  const names = entry ? entry.names : null;
  const sig = JSON.stringify([from, names, busy]);
  els["pull-up"].disabled = busy || from === "";
  const status = names === null
    ? "Loading folders…"
    : names.length === 0
      ? "No folders here."
      : names.length === 1 ? "1 folder here." : `${names.length} folders here.`;
  els["pull-folder-status"].textContent = status;
  if (sig === folderSig) return;
  folderSig = sig;

  const crumbList = els.crumbs;
  crumbList.replaceChildren(
    ...crumbs.map((c, i) => {
      const li = document.createElement("li");
      if (i === crumbs.length - 1) {
        const here = document.createElement("span");
        here.textContent = c.label;
        here.setAttribute("aria-current", "location");
        li.append(here);
      } else {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "crumb";
        button.textContent = c.label;
        button.disabled = busy;
        button.addEventListener("click", () => navigatePull(c.from));
        li.append(button);
      }
      return li;
    }),
  );

  const list = els["pull-folders"];
  list.replaceChildren(
    ...folderRows(names || [], from).map((row) => {
      const li = document.createElement("li");
      const button = document.createElement("button");
      button.type = "button";
      button.className = "folder";
      const label = document.createElement("span");
      label.textContent = row.name;
      const arrow = document.createElement("span");
      arrow.className = "arrow";
      arrow.setAttribute("aria-hidden", "true");
      arrow.textContent = "›";
      button.append(label, arrow);
      button.setAttribute("aria-label", `Open ${row.name}`);
      if (row.from === null) {
        button.disabled = true;
        button.title = "flashcp can't open this folder name";
      } else {
        button.disabled = busy;
        button.addEventListener("click", () => navigatePull(row.from));
      }
      li.append(button);
      return li;
    }),
  );
}

function renderCollision(open) {
  els.collision.hidden = !open;
  if (!open) {
    collisionOpen = false;
    return;
  }
  els["collision-text"].textContent = collisionText(state.pullCollision.dest);
  els["collision-skip"].disabled = jobActive(state);
  els["collision-overwrite"].disabled = jobActive(state);
  if (!collisionOpen) {
    collisionOpen = true;
    els.collision.focus();
  }
}

function renderTrust(drive, busy) {
  const needsTrust = !!drive && trustState(drive) !== "trusted";
  els.trust.hidden = !needsTrust;
  if (!needsTrust) return;

  els["trust-text"].textContent = trustNotice(drive);
  const offered = canTrust(drive);
  const confirming = offered && state.trustOpen;
  els["trust-open-row"].hidden = !offered || confirming;
  els["trust-open"].disabled = busy || state.trustBusy;

  els["trust-confirm"].hidden = !confirming;
  if (confirming) {
    const rows = trustDetails(drive).flatMap((row) => {
      const dt = document.createElement("dt");
      dt.textContent = row.label;
      const dd = document.createElement("dd");
      dd.textContent = row.value;
      return [dt, dd];
    });
    els["trust-facts"].replaceChildren(...rows);
  }
  els["trust-yes"].disabled = busy || state.trustBusy;
  els["trust-yes"].textContent = state.trustBusy ? "Trusting…" : "Trust this drive";
  els["trust-cancel"].disabled = state.trustBusy;

  els["trust-error"].hidden = !state.trustError;
  els["trust-error"].textContent = state.trustError || "";
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
      if (radio.checked) setState(selectDrive(state, d.id));
    });

    const body = document.createElement("span");
    body.className = "drive-body";

    const title = document.createElement("span");
    title.className = "drive-title";
    title.textContent = driveTitle(d);

    const device = document.createElement("span");
    device.className = "drive-device";
    device.textContent = d.device;

    const serial = document.createElement("span");
    serial.className = "drive-serial";
    serial.textContent = d.serial ? `Serial ${d.serial}` : "No serial number";

    const meta = document.createElement("span");
    meta.className = "drive-meta";
    const size = document.createElement("span");
    size.textContent = formatBytes(d.size);
    const badge = document.createElement("span");
    badge.className = d.isExfat ? "badge exfat" : "badge";
    badge.textContent = d.fsType ? d.fsType.toUpperCase() : "NO FS";
    const trust = document.createElement("span");
    const trusted = trustState(d) === "trusted";
    trust.className = trusted ? "badge trusted" : "badge untrusted";
    trust.textContent = trusted ? "Trusted" : "Not trusted";
    meta.append(size, badge, trust);

    body.append(title, device, serial, meta);
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
  els["mode-copy"].addEventListener("click", () => setState(setMode(state, "copy")));
  els["mode-pull"].addEventListener("click", () => setState(setMode(state, "pull")));

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
  els["copy-more"].addEventListener("click", () =>
    setState(state.mode === "pull" ? dismissJob(state) : resetForm(state)),
  );
  els.dismiss.addEventListener("click", () => setState(dismissJob(state)));

  els["erase-confirm"].addEventListener("input", () => {
    state = { ...state, eraseTyped: els["erase-confirm"].value };
    render();
  });
  els.erase.addEventListener("click", startErase);

  els["pull-up"].addEventListener("click", () => navigatePull(parentFrom(state.pullFrom)));
  els["pull-whole"].addEventListener("click", () => navigatePull(""));
  els["pull-from"].addEventListener("input", () => {
    state = { ...state, pullFrom: els["pull-from"].value };
    render();
  });
  els["pull-change"].addEventListener("click", changePullDest);
  els["pull-default"].addEventListener("click", () => setState(resetPullDest(state)));
  els["pull-start"].addEventListener("click", () => startPull(null));
  els["collision-skip"].addEventListener("click", () => chooseCollision("skip"));
  els["collision-overwrite"].addEventListener("click", () => chooseCollision("overwrite"));
  els["collision-cancel"].addEventListener("click", cancelCollision);

  els["trust-open"].addEventListener("click", showTrustPanel);
  els["trust-cancel"].addEventListener("click", cancelTrust);
  els["trust-yes"].addEventListener("click", startTrust);

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
  await tauri.event.listen("pull-requested", (e) => onPullRequest(e.payload));
}

async function init() {
  // Preview only: ?mode=pull opens the Copy from USB view.
  if (!hasTauri && demoParams.get("mode") === "pull") state = { ...state, mode: "pull" };

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

  try {
    const defaults = await call("pull_defaults");
    if (defaults && typeof defaults.downloads === "string") {
      setState({ ...state, downloads: defaults.downloads });
    }
  } catch (err) {
    setNotice(`Could not find your Downloads folder: ${messageOf(err)}`);
  }

  try {
    const req = await call("take_pending_pull");
    if (req) setState(applyPullRequest(state, req));
  } catch (err) {
    setNotice(`Could not read the folder from the file manager: ${messageOf(err)}`);
  }
}

init();
