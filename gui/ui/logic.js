// Pure logic for the flashcp GUI. No DOM, no Tauri, so it can be imported
// from Node (gui/ui/tests/logic.test.mjs). Every UI decision goes through here.

/**
 * @typedef {Object} Drive
 * @property {string} id
 * @property {string} device        e.g. "/dev/sdb1"
 * @property {string} label         "" when none
 * @property {string} fsType        e.g. "exfat", "vfat", ""
 * @property {boolean} isExfat
 * @property {number} size          bytes
 * @property {string|null} mountPoint
 * @property {string} model
 * @property {boolean} wholeDisk
 * @property {string} serial        disk serial from udisks; "" when the drive reports none
 * @property {boolean} trusted      the helper's trust file has this drive (UI hint only)
 */

/**
 * @typedef {Object} JobSummary
 * @property {number} files
 * @property {number} bytes
 * @property {number} secs
 * @property {number} wallSecs
 * @property {string|null} mountPoint
 * @property {string|null} [dest]     pull: the local destination folder
 * @property {string|null} [warning]  pull: set when the remount failed after a good copy
 * @property {number|null} [clusterBytes]  add: the stick's cluster size after the copy
 */

/**
 * @typedef {Object} JobUpdate
 * @property {"starting"|"unmounting"|"authorizing"|"running"|"remounting"|"done"|"error"} stage
 *   "starting" is client-only: set locally before start_copy/start_erase/start_optimize is invoked.
 * @property {"add"|"build"|"pull"|"optimize"} op
 * @property {boolean} [conflict]  pull: the copy stopped because the destination already has files
 * @property {number} index
 * @property {number} total
 * @property {number} filesDone     optimize: within the current phase
 * @property {number} bytesDone     optimize: within the current phase
 * @property {number} planFiles
 * @property {number} planBytes
 * @property {string|null} message
 * @property {JobSummary|null} summary
 * @property {"pull"|"verify"|"build"|"check"|"cleanup"|null} [phase]  optimize only
 * @property {number} [step]     optimize only: 1-based index of the phase
 * @property {number} [steps]    optimize only: number of phases in this run
 * @property {string|null} [recovery]  optimize only: what to do when the stick was left half-built
 */

/**
 * @typedef {Object} OptimizePlan
 * @property {number} files
 * @property {number} dirs
 * @property {number} bytes
 * @property {number} writeBytes
 * @property {string|null} temp
 * @property {number|null} oldCluster
 * @property {number|null} newCluster
 * @property {string} label
 * @property {boolean} resume
 * @property {boolean} alreadyOptimized
 * @property {number} needBytes
 * @property {number|null} tempFree
 * @property {boolean} enoughSpace
 */

/**
 * @typedef {Object} AppState
 * @property {Drive[]} drives
 * @property {string|null} selectedId
 * @property {string[]} sources
 * @property {string} destDir
 * @property {boolean} skipExisting
 * @property {JobUpdate|null} job
 * @property {{autostart: boolean, notifyOnPlug: boolean}} settings
 * @property {string} eraseTyped
 * @property {string|null} notice
 * @property {boolean} trustOpen    the "Trust this drive" confirm panel is showing
 * @property {boolean} trustBusy    a trust_drive call is in flight
 * @property {string|null} trustError
 * @property {"copy"|"pull"} mode   which direction the window is showing
 * @property {string|null} downloads        the user's Downloads folder (pull_defaults)
 * @property {string} pullFrom              folder on the drive, "" = whole drive
 * @property {{key: string, names: string[], error: string|null}|null} pullFolders
 *   subfolders of the folder named by `key` (see folderKey); null until loaded
 * @property {string|null} pullDestOverride a destination picked with Change…; null = default
 * @property {{dest: string, source: "preflight"|"conflict"}|null} pullCollision
 * @property {{driveId: string, status: "checking"|"ready"|"error", plan: OptimizePlan|null, error: string|null}|null} optimize
 *   the "Optimize this stick" plan panel; null when closed
 * @property {string|null} optimizeDriveId  the drive the running or last optimize job works on
 * @property {boolean} optimizePending      optimize_pending(): an interrupted optimize copy exists
 * @property {string[]} tipDismissed        drive keys (serial) whose optimize tip was dismissed
 */

const UNITS = ["kB", "MB", "GB", "TB"];

/**
 * Human-readable size, 1000-based (kB = 1000 B, matching drive marketing).
 * Under 1000 B: integer bytes. Otherwise one decimal below 100 of a unit
 * ("1.5 kB", "50.0 MB") and no decimals from 100 up ("512 MB").
 * A value that would round up to 1000 moves to the next unit ("1.0 MB", not "1000 kB").
 * @param {number} n
 * @returns {string}
 */
export function formatBytes(n) {
  if (!Number.isFinite(n) || n <= 0) return "0 B";
  if (n < 1000) return `${Math.floor(n)} B`;
  let value = n / 1000;
  let unit = 0;
  while (unit < UNITS.length - 1 && value >= 1000) {
    value /= 1000;
    unit++;
  }
  let text = formatScaled(value);
  // Rounding can push e.g. 999.96 kB to "1000.0 kB"; promote to the next unit.
  if (parseFloat(text) >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit++;
    text = formatScaled(value);
  }
  return `${text} ${UNITS[unit]}`;
}

function formatScaled(value) {
  return value < 100 ? value.toFixed(1) : value.toFixed(0);
}

/**
 * Transfer speed in MB/s (1 MB = 1e6 bytes). "—" when the duration is unusable.
 * @param {number} bytes
 * @param {number} secs
 * @returns {string}
 */
export function formatSpeed(bytes, secs) {
  if (!Number.isFinite(secs) || secs <= 0 || !Number.isFinite(bytes)) return "—";
  return `${(bytes / secs / 1e6).toFixed(1)} MB/s`;
}

/**
 * "0.42 s" below 10 s, "12.3 s" below 60 s, "2 min 05 s" from 60 s up.
 * @param {number} secs
 * @returns {string}
 */
export function formatDuration(secs) {
  if (!Number.isFinite(secs) || secs < 0) return "—";
  if (secs < 10) return `${secs.toFixed(2)} s`;
  if (secs < 60) return `${secs.toFixed(1)} s`;
  let minutes = Math.floor(secs / 60);
  let seconds = Math.round(secs - minutes * 60);
  if (seconds === 60) {
    minutes++;
    seconds = 0;
  }
  return `${minutes} min ${String(seconds).padStart(2, "0")} s`;
}

/**
 * Progress of the current job as a fraction in [0, 1]. 0 when nothing is planned.
 * @param {Pick<JobUpdate, "bytesDone"|"planBytes">} update
 * @returns {number}
 */
export function progressFraction(update) {
  if (!update || !(update.planBytes > 0)) return 0;
  const f = update.bytesDone / update.planBytes;
  if (!Number.isFinite(f)) return 0;
  return Math.min(1, Math.max(0, f));
}

/**
 * Status line for the progress panel.
 * @param {JobUpdate} update
 * @returns {string}
 */
export function stageText(update) {
  switch (update.stage) {
    case "starting":
      return "Starting…";
    case "unmounting":
      return "Unmounting drive…";
    case "authorizing":
      return "Waiting for authorization…";
    case "running":
      if (update.op === "pull") return "Copying from the drive…";
      if (update.op === "optimize") return optimizeStepText(update);
      return `Copying item ${update.index + 1} of ${update.total}…`;
    case "remounting":
      return "Remounting…";
    case "done":
      return "Done.";
    case "error":
      return update.message || "Failed.";
    default:
      return "";
  }
}

/**
 * "3 of 10 files · 1.5 kB of 4.0 MB"
 * @param {JobUpdate} update
 * @returns {string}
 */
export function progressText(update) {
  // The cleanup phase has no meaningful file or byte count to show.
  if (update.op === "optimize" && update.phase === "cleanup") return "";
  return (
    `${update.filesDone} of ${update.planFiles} files · ` +
    `${formatBytes(update.bytesDone)} of ${formatBytes(update.planBytes)}`
  );
}

/**
 * Last path segment: "/dev/sdb1" -> "sdb1".
 * @param {string} path
 * @returns {string}
 */
export function baseName(path) {
  if (typeof path !== "string") return "";
  const trimmed = path.replace(/\/+$/, "");
  const i = trimmed.lastIndexOf("/");
  return i >= 0 ? trimmed.slice(i + 1) : trimmed;
}

/**
 * The label is used only for an exFAT volume that has one; everything else
 * (vfat with a label, unlabelled exFAT, no filesystem) uses the serial number.
 * "" when there is nothing to type. Must match the helper's rule exactly.
 * @param {Drive} drive
 * @returns {string}
 */
export function expectedConfirm(drive) {
  if (!drive) return "";
  if (usesLabel(drive)) return drive.label;
  return typeof drive.serial === "string" ? drive.serial : "";
}

/** True when the confirm text is the volume label rather than the serial. */
function usesLabel(drive) {
  return drive.isExfat === true && typeof drive.label === "string" && drive.label !== "";
}

/**
 * The line above the erase confirm box, naming what to type.
 * @param {Drive|null} drive
 * @returns {string}
 */
export function confirmHint(drive) {
  if (!drive) return "Select a drive first.";
  const expected = expectedConfirm(drive);
  if (expected === "") return "This drive has no name or serial number to type.";
  if (usesLabel(drive)) return `Type the drive name ${expected} to confirm`;
  return `Type the drive's serial number ${expected} to confirm`;
}

/**
 * "trusted": the helper's trust file has this drive.
 * "untrustable": the drive reports no serial, so it can never be trusted.
 * "untrusted": it has a serial but has not been trusted yet.
 * @param {Drive|null} drive
 * @returns {"trusted"|"untrusted"|"untrustable"}
 */
export function trustState(drive) {
  if (!drive) return "untrusted";
  if (typeof drive.serial !== "string" || drive.serial === "") return "untrustable";
  return drive.trusted === true ? "trusted" : "untrusted";
}

/** True only for a drive the helper trusts (the UI gate for copy and erase). */
export function isTrusted(drive) {
  return trustState(drive) === "trusted";
}

/** True when the "Trust this drive…" button applies. */
export function canTrust(drive) {
  return !!drive && trustState(drive) === "untrusted";
}

/**
 * The sentence shown while the selected drive is not trusted. "" when trusted.
 * @param {Drive|null} drive
 * @returns {string}
 */
export function trustNotice(drive) {
  switch (trustState(drive)) {
    case "trusted":
      return "";
    case "untrustable":
      return "This drive doesn't report a usable serial number or WWN, so flashcp can't remember it safely. You can still copy to it from the command line: sudo flashcp add <folder> /dev/sdXN";
    default:
      return "flashcp hasn't been allowed to write to this drive yet.";
  }
}

/**
 * Rows for the trust confirm panel.
 * @param {Drive|null} drive
 * @returns {{label: string, value: string}[]}
 */
/** Exact byte count with thousands separators, e.g. "1,000,204,886,016". */
function groupDigits(n) {
  return String(Math.trunc(Number(n) || 0)).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/**
 * The rows of the trust confirmation: exactly the identity tuple flashcp-trust checks
 * (drive.identity, the same object the backend builds its --expect/--wwn/--model/--size from),
 * one row per checked field. The friendly vendor+model and the partition size are shown on
 * the drive card, never here.
 */
export function trustDetails(drive) {
  const id = drive && drive.identity;
  if (!id || !id.key) return [];
  const rows = [
    { label: id.keyKind === "wwn" ? "WWN" : "Serial", value: id.key },
    { label: "Model", value: id.model },
    {
      label: "Disk size (whole disk)",
      value: `${formatBytes(id.diskSize)} (${groupDigits(id.diskSize)} bytes)`,
    },
  ];
  if (id.keyKind === "serial" && id.wwn && id.wwn !== "-") {
    rows.push({ label: "WWN", value: id.wwn });
  }
  return rows;
}

/** The values flashcp-trust receives for this drive (mirrors runner::trust_args). */
export function trustArgs(drive) {
  const id = drive.identity;
  return { expect: id.key, wwn: id.wwn, model: id.model, size: String(id.diskSize) };
}

/**
 * @param {Drive} drive
 * @param {string} typed
 * @returns {boolean} exact, case-sensitive match
 */
export function confirmMatches(drive, typed) {
  const expected = expectedConfirm(drive);
  return expected !== "" && typed === expected;
}

/** Name shown for a drive; "Untitled" when it has no label. */
export function driveTitle(drive) {
  return drive && drive.label ? drive.label : "Untitled";
}

/**
 * Explanation shown when the selected drive is not exFAT.
 * @param {Drive} drive
 * @returns {string}
 */
export function nonExfatText(drive) {
  const fs = drive && drive.fsType ? drive.fsType.toUpperCase() : "";
  const what = fs ? `This drive is ${fs}.` : "This drive has no filesystem.";
  return (
    `flashcp only works on exFAT drives. ${what} ` +
    "You can erase it and rebuild it as exFAT with your files (Advanced below) — " +
    "everything on it will be lost."
  );
}

/**
 * The drive currently selected in state, or null.
 * @param {AppState} state
 * @returns {Drive|null}
 */
export function selectedDrive(state) {
  if (!state || state.selectedId == null) return null;
  return state.drives.find((d) => d.id === state.selectedId) || null;
}

/**
 * True while a job is in flight (not done, not error, not absent).
 * @param {AppState} state
 */
export function jobActive(state) {
  const job = state.job;
  return !!job && job.stage !== "done" && job.stage !== "error";
}

/**
 * Copy is allowed only for an exFAT drive, at least one source, a valid
 * destination and no job in flight.
 * @param {AppState} state
 */
export function canCopy(state) {
  const drive = selectedDrive(state);
  return (
    !!drive &&
    drive.isExfat === true &&
    isTrusted(drive) &&
    state.sources.length > 0 &&
    normalizeDestDir(state.destDir) !== null &&
    !jobActive(state)
  );
}

/**
 * Erase needs a trusted drive, exactly one source (the folder that becomes the
 * new volume), the exact confirm text typed, and no job in flight.
 * @param {AppState} state
 * @param {string} typed
 */
export function canErase(state, typed) {
  const drive = selectedDrive(state);
  return (
    !!drive &&
    isTrusted(drive) &&
    state.sources.length === 1 &&
    confirmMatches(drive, typed) &&
    !jobActive(state)
  );
}

/**
 * Append incoming absolute paths to existing ones: dedupe, keep order,
 * ignore anything that is not an absolute path string.
 * @param {string[]} existing
 * @param {string[]} incoming
 * @returns {string[]} a new array
 */
export function mergeSources(existing, incoming) {
  const out = [];
  const seen = new Set();
  for (const p of [...(existing || []), ...(incoming || [])]) {
    if (typeof p !== "string" || !p.startsWith("/")) continue;
    if (seen.has(p)) continue;
    seen.add(p);
    out.push(p);
  }
  return out;
}

/**
 * Remove one source by index. Out-of-range indexes return the list unchanged.
 * @param {string[]} sources
 * @param {number} index
 * @returns {string[]}
 */
export function removeSource(sources, index) {
  if (!Number.isInteger(index) || index < 0 || index >= sources.length) return sources.slice();
  return sources.filter((_, i) => i !== index);
}

/**
 * Keep the current selection if that drive is still present; otherwise select
 * the only exFAT drive; otherwise nothing.
 * @param {Drive[]} drives
 * @param {string|null} currentId
 * @returns {string|null}
 */
export function defaultSelection(drives, currentId, optimizePending = false) {
  if (currentId != null && drives.some((d) => d.id === currentId)) return currentId;
  const exfat = drives.filter((d) => d.isExfat === true);
  if (exfat.length === 1) return exfat[0].id;
  // A stick left without a filesystem by an interrupted optimize: select it so "Resume optimize…" shows.
  if (exfat.length === 0 && optimizePending === true) {
    const blank = drives.filter((d) => d.fsType === "");
    if (blank.length === 1) return blank[0].id;
  }
  return null;
}

/**
 * Canonical in-volume destination: "/" is the root, otherwise an absolute path
 * with no empty or "." segments and no trailing slash. Returns null when the
 * path contains ".." (we never let the copy escape the volume).
 * @param {string} s
 * @returns {string|null}
 */
export function normalizeDestDir(s) {
  if (typeof s !== "string") return null;
  const segments = s.trim().split("/").filter((seg) => seg !== "" && seg !== ".");
  if (segments.some((seg) => seg === "..")) return null;
  return "/" + segments.join("/");
}

/**
 * One-line result for a finished job. Empty string when there is no summary.
 * @param {JobSummary|null} summary
 * @returns {string}
 */
export function summaryText(summary) {
  if (!summary) return "";
  const files = summary.files === 1 ? "1 file" : `${summary.files} files`;
  return (
    `Copied ${files} (${formatBytes(summary.bytes)}) in ${formatDuration(summary.secs)} — ` +
    formatSpeed(summary.bytes, summary.secs)
  );
}

/** Start a job locally before start_copy/start_erase is invoked. */
export function beginJob(state, op, total) {
  return {
    ...state,
    job: {
      stage: "starting",
      op,
      index: 0,
      total,
      filesDone: 0,
      bytesDone: 0,
      planFiles: 0,
      planBytes: 0,
      message: null,
      summary: null,
    },
    notice: null,
  };
}

/**
 * Apply one "job" event from the backend. Returns a new state.
 * - stage "done" keeps the summary; "error" keeps the message.
 * - the job record always mirrors the latest update.
 * @param {AppState} state
 * @param {JobUpdate} update
 * @returns {AppState}
 */
export function applyJobUpdate(state, update) {
  if (!update || typeof update.stage !== "string") return state;
  return {
    ...state,
    job: { ...update, summary: update.stage === "done" ? update.summary || null : null },
  };
}

/** Clear a finished or failed job so the error/summary box goes away. */
export function dismissJob(state) {
  return { ...state, job: null };
}

/**
 * "Copy more": forget the sources and any typed erase confirmation; keep the
 * drive and destination so the next copy goes to the same place.
 * @param {AppState} state
 */
export function resetForm(state) {
  return { ...state, sources: [], job: null, eraseTyped: "" };
}

/**
 * The erase explanation text for the number of sources selected.
 * @param {number} count
 * @returns {string}
 */
export function eraseSourceHint(count) {
  if (count === 1) return "";
  if (count === 0) return "Add exactly one folder to copy onto the rebuilt drive.";
  return `Erase needs exactly one folder, but ${count} items are selected. Remove all but one folder.`;
}

/**
 * Select a drive. Choosing a different drive closes the trust panel and clears
 * the typed erase confirmation; re-selecting the same drive changes nothing.
 * @param {AppState} state
 * @param {string|null} id
 * @returns {AppState}
 */
export function selectDrive(state, id) {
  if (id === state.selectedId) return state;
  return {
    ...state,
    selectedId: id,
    eraseTyped: "",
    trustOpen: false,
    trustError: null,
    pullFrom: "",
    pullCollision: null,
    optimize: null,
  };
}

/** Show the trust confirm panel. */
export function openTrust(state) {
  return { ...state, trustOpen: true, trustError: null };
}

/** Hide the trust confirm panel and clear its busy flag and error. */
export function closeTrust(state) {
  return { ...state, trustOpen: false, trustBusy: false, trustError: null };
}

// ---- Copy from USB (pull) ----------------------------------------------------

/**
 * Local calendar date as YYYY-MM-DD (the user's day, not UTC).
 * @param {Date} date
 * @returns {string}
 */
export function isoLocalDate(date) {
  if (!(date instanceof Date) || Number.isNaN(date.getTime())) {
    throw new TypeError("isoLocalDate needs a valid Date");
  }
  const y = String(date.getFullYear()).padStart(4, "0");
  const m = String(date.getMonth() + 1).padStart(2, "0");
  const d = String(date.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/**
 * A drive label as a single file name: "/" and control characters become "_",
 * surrounding spaces are trimmed, and an empty result becomes "USB".
 * @param {string} label
 * @returns {string}
 */
export function sanitizeLabel(label) {
  const s = typeof label === "string" ? label.replace(/[\/\u0000-\u001f\u007f]/g, "_").trim() : "";
  return s === "" ? "USB" : s;
}

/**
 * Default copy destination: "<downloads>/<label or USB>-<YYYY-MM-DD>".
 * @param {string} downloads absolute Downloads folder
 * @param {string} label     drive label ("" when none)
 * @param {Date} date
 * @returns {string}
 */
export function defaultPullDest(downloads, label, date) {
  const base = String(downloads || "").replace(/\/+$/, "");
  return `${base}/${sanitizeLabel(label)}-${isoLocalDate(date)}`;
}

/**
 * Canonical folder path on the drive: no leading or trailing "/" (trimmed),
 * "" for the drive root. null when it is not allowed: an empty component
 * (a "//" inside), a "." or ".." component, a leading "-",
 * a control character, or more than 1024 bytes. Same rule as the helper.
 * @param {string} s
 * @returns {string|null}
 */
export function normalizeFrom(s) {
  if (typeof s !== "string") return null;
  const rel = s.replace(/^\/+|\/+$/g, "");
  if (rel === "") return "";
  // Same rule as the helper's check_from: Rust's char::is_control is C0, DEL and C1.
  if (/[\u0000-\u001f\u007f-\u009f]/.test(rel)) return null;
  if (new TextEncoder().encode(rel).length > 1024) return null;
  // Only a leading "-" on the whole value could be read as a flag; "a/-b" is fine.
  if (rel.startsWith("-")) return null;
  for (const seg of rel.split("/")) {
    if (seg === "" || seg === "." || seg === "..") return null;
  }
  return rel;
}

/**
 * The folder `name` inside `rel`, canonical, or null. `name` is one folder
 * name as list_drive_folders returns it (no "/").
 * @param {string} rel
 * @param {string} name
 * @returns {string|null}
 */
export function joinFrom(rel, name) {
  if (typeof name !== "string" || name === "" || name.includes("/")) return null;
  const base = normalizeFrom(rel);
  if (base === null) return null;
  return normalizeFrom(base === "" ? name : `${base}/${name}`);
}

/**
 * The folder above `rel`. "" at the root or when `rel` is not valid.
 * @param {string} rel
 * @returns {string}
 */
export function parentFrom(rel) {
  const r = normalizeFrom(rel);
  if (!r) return "";
  const i = r.lastIndexOf("/");
  return i >= 0 ? r.slice(0, i) : "";
}

/**
 * Breadcrumb trail for the folder browser: the drive root, then each folder.
 * @param {string} rel
 * @returns {{label: string, from: string}[]}
 */
export function breadcrumbs(rel) {
  const r = normalizeFrom(rel) || "";
  const out = [{ label: "Drive", from: "" }];
  if (r === "") return out;
  const parts = r.split("/");
  parts.forEach((part, i) => out.push({ label: part, from: parts.slice(0, i + 1).join("/") }));
  return out;
}

/**
 * Rows for the folder list: each name with the canonical path it opens to.
 * `from` is null for a name flashcp will not open (for example "-x").
 * @param {string[]} names
 * @param {string} rel
 * @returns {{name: string, from: string|null}[]}
 */
export function folderRows(names, rel) {
  return (Array.isArray(names) ? names : []).map((name) => ({ name, from: joinFrom(rel, name) }));
}

/** A usable absolute local folder path that is not the filesystem root. */
export function isAbsolutePath(p) {
  return (
    typeof p === "string" &&
    p.startsWith("/") &&
    p.replace(/\/+$/, "") !== "" &&
    !p.includes("\u0000")
  );
}

/**
 * The destination the pull will use: the picked folder if one was chosen,
 * otherwise the default for the selected drive. null when there is no usable path.
 * Trailing slashes are removed.
 * @param {AppState} state
 * @param {Date} [date]
 * @returns {string|null}
 */
export function pullDestination(state, date = new Date()) {
  let dest;
  if (state.pullDestOverride) {
    dest = state.pullDestOverride;
  } else if (state.downloads) {
    const drive = selectedDrive(state);
    dest = defaultPullDest(state.downloads, drive ? drive.label : "", date);
  } else {
    return null;
  }
  dest = dest.replace(/\/+$/, "");
  return isAbsolutePath(dest) ? dest : null;
}

/**
 * Copy from USB is allowed for a trusted exFAT drive, a valid folder on it,
 * a usable destination, and no job in flight.
 * @param {AppState} state
 */
export function canPull(state) {
  const drive = selectedDrive(state);
  return (
    !!drive &&
    drive.isExfat === true &&
    isTrusted(drive) &&
    !jobActive(state) &&
    normalizeFrom(state.pullFrom) !== null &&
    pullDestination(state) !== null
  );
}

/**
 * Arguments for start_pull, or null when the pull cannot start.
 * @param {AppState} state
 * @param {"fail"|"skip"|"overwrite"} existing
 * @returns {{driveId: string, from: string, dest: string, existing: string}|null}
 */
export function pullArgs(state, existing) {
  if (!canPull(state)) return null;
  if (existing !== "fail" && existing !== "skip" && existing !== "overwrite") return null;
  return {
    driveId: selectedDrive(state).id,
    from: normalizeFrom(state.pullFrom),
    dest: pullDestination(state),
    existing,
  };
}

/**
 * Switch the window between "Copy to USB" and "Copy from USB".
 * @param {AppState} state
 * @param {"copy"|"pull"} mode
 */
export function setMode(state, mode) {
  if (mode !== "copy" && mode !== "pull") return state;
  return state.mode === mode ? state : { ...state, mode };
}

/** Set the folder on the drive to copy from. Stored as given; canPull validates it. */
export function setPullFrom(state, rel) {
  return { ...state, pullFrom: typeof rel === "string" ? rel : "" };
}

/**
 * Choose a destination with Change…. A path that is not absolute is refused
 * with a notice; a choice clears any collision panel for the old destination.
 * @param {AppState} state
 * @param {string} path
 */
export function setPullDest(state, path) {
  const p = typeof path === "string" ? path.replace(/\/+$/, "") : "";
  if (!isAbsolutePath(p)) {
    return { ...state, notice: "Choose a folder with a full path, such as /home/you/Documents/USB." };
  }
  return { ...state, pullDestOverride: p, pullCollision: null, notice: null };
}

/** Go back to the default destination for the selected drive. */
export function resetPullDest(state) {
  return { ...state, pullDestOverride: null, pullCollision: null };
}

/** Key identifying one folder listing: the drive and the folder on it. */
export function folderKey(driveId, from) {
  return `${driveId}\n${from}`;
}

/**
 * The listing the browser needs now: null when the drive is not mounted (or
 * the folder text is not valid), otherwise folderKey for the selected drive.
 * @param {AppState} state
 * @returns {string|null}
 */
export function browseKey(state) {
  const drive = selectedDrive(state);
  if (!drive || !drive.mountPoint) return null;
  const from = normalizeFrom(state.pullFrom);
  return from === null ? null : folderKey(drive.id, from);
}

/**
 * Store a listing result for `key`. Results for other folders are ignored by
 * the caller, which checks browseKey first.
 * @param {AppState} state
 * @param {string} key
 * @param {string[]} names
 * @param {string|null} error
 */
export function setFolders(state, key, names, error) {
  return {
    ...state,
    pullFolders: { key, names: Array.isArray(names) ? names.slice() : [], error: error || null },
  };
}

/** Open the collision panel for `dest`. source: "preflight" (checked before start) or "conflict" (the job reported it). */
export function showCollision(state, dest, source) {
  return { ...state, pullCollision: { dest, source } };
}

/** Close the collision panel. */
export function clearCollision(state) {
  return { ...state, pullCollision: null };
}

/** The explanation at the top of the collision panel. */
export function collisionText(dest) {
  return `${dest} already has files in it.`;
}

/**
 * The line for a finished pull: "Copied 3 files (1.5 MB) to /x/USB-2026-11-09 in 1.8 s".
 * The warning (remount failed after a good copy) is shown separately.
 * @param {JobSummary|null} summary
 * @returns {string}
 */
export function pullSummaryText(summary) {
  if (!summary) return "";
  const files = summary.files === 1 ? "1 file" : `${summary.files} files`;
  const to = summary.dest ? ` to ${summary.dest}` : "";
  return `Copied ${files} (${formatBytes(summary.bytes)})${to} in ${formatDuration(summary.secs)}`;
}

/**
 * Apply a pull request (take_pending_pull at start, or the "pull-requested"
 * event): switch to Copy from USB, select the drive if it is listed, set the
 * folder, and show the backend's error if there is one.
 * @param {AppState} state
 * @param {{driveId: string|null, from: string, error: string|null}|null} req
 * @returns {AppState}
 */
export function applyPullRequest(state, req) {
  if (!req || typeof req !== "object") return state;
  let next = { ...state, mode: "pull", pullCollision: null };
  if (typeof req.driveId === "string" && state.drives.some((d) => d.id === req.driveId)) {
    next = selectDrive(next, req.driveId);
  }
  const from = normalizeFrom(typeof req.from === "string" ? req.from : "");
  const error = typeof req.error === "string" && req.error !== "" ? req.error : null;
  return {
    ...next,
    pullFrom: from === null ? "" : from,
    notice: error || next.notice,
  };
}

// ---- Optimize this stick -----------------------------------------------------

// Speeds behind the time estimate. "fast" gives the low end, "slow USB 2" the high end.
const OPTIMIZE_FAST = { readBps: 100e6, writeBps: 60e6, perFileSecs: 0.0002 };
const OPTIMIZE_SLOW = { readBps: 5e6, writeBps: 4e6, perFileSecs: 0.002 };

const OPTIMIZE_PHASES = {
  pull: "Copying the stick to this computer",
  verify: "Checking the copy",
  build: "Rebuilding the stick",
  check: "Checking the stick",
  cleanup: "Removing the temporary copy",
};

export const OPTIMIZE_RISK =
  "This takes a while. Don't unplug the stick or put the computer to sleep until it says it's done.";

export const OPTIMIZE_RECOVERY_WARNING =
  "If Windows (or another computer) offers to format this stick, say No. " +
  "Plug it back into this computer and choose Resume optimize here.";

const OPTIMIZE_WHAT =
  "Your files are copied to this computer and checked, then the stick is erased and rebuilt " +
  "from that verified copy, then checked again. Your files are kept.";

const OPTIMIZE_LOST = "Lost: file attributes, and creation and access times.";

function nonNegative(n) {
  return Number.isFinite(n) && n > 0 ? n : 0;
}

/**
 * Rough time range for an optimize plan. A fresh run reads the data twice (copy,
 * then check) and writes it once; a resume reads it once (check) and writes once.
 * Every file adds a fixed cost. lowSecs is a fast drive, highSecs a slow USB 2 stick.
 * @param {Pick<OptimizePlan, "bytes"|"writeBytes"|"files"|"resume">} plan
 * @returns {{lowSecs: number, highSecs: number}}
 */
export function optimizeEstimate(plan) {
  const bytes = nonNegative(plan && plan.bytes);
  const writeBytes = nonNegative(plan && plan.writeBytes);
  const files = nonNegative(plan && plan.files);
  const reads = plan && plan.resume === true ? 1 : 2;
  const secs = (s) => (reads * bytes) / s.readBps + writeBytes / s.writeBps + files * s.perFileSecs;
  return { lowSecs: secs(OPTIMIZE_FAST), highSecs: secs(OPTIMIZE_SLOW) };
}

/**
 * The phase name for the progress line. Unknown or missing phases read "Working".
 * @param {string|null|undefined} phase
 * @returns {string}
 */
export function optimizePhaseText(phase) {
  return typeof phase === "string" && Object.prototype.hasOwnProperty.call(OPTIMIZE_PHASES, phase)
    ? OPTIMIZE_PHASES[phase]
    : "Working";
}

/**
 * "Step 2 of 5: Rebuilding the stick…" (the step prefix is left out when the backend sent none).
 * @param {JobUpdate} update
 * @returns {string}
 */
export function optimizeStepText(update) {
  const { step, steps } = update;
  const prefix =
    Number.isInteger(step) && step > 0 && Number.isInteger(steps) && steps > 0
      ? `Step ${step} of ${steps}: `
      : "";
  return `${prefix}${optimizePhaseText(update.phase)}…`;
}

/**
 * True when the progress bar should not show a fraction: the cleanup phase, and any
 * optimize phase that has no bytes to count. The bar then runs as indeterminate.
 * @param {JobUpdate} update
 * @returns {boolean}
 */
export function progressIndeterminate(update) {
  if (!update || update.op !== "optimize") return false;
  return update.phase === "cleanup" || !(update.planBytes > 0);
}

/**
 * The finished line: "Optimized: 20,000 files (169 MB) in 2 min 35 s." Empty without a summary.
 * @param {JobSummary|null} summary
 * @returns {string}
 */
export function optimizeSummaryText(summary) {
  if (!summary) return "";
  const files = summary.files === 1 ? "1 file" : `${groupDigits(summary.files)} files`;
  return `Optimized: ${files} (${formatBytes(summary.bytes)}) in ${formatDuration(summary.secs)}.`;
}

/**
 * The error view for a failed optimize. `recovery` is set only when the stick may have been
 * left half-built; then the warning and the Resume button apply. Without it the message is
 * shown as a normal error.
 * @param {JobUpdate} update
 * @returns {{message: string, recovery: string|null, warning: string|null, canResume: boolean}}
 */
export function optimizeErrorView(update) {
  const message = (update && update.message) || "Failed.";
  const recovery =
    update && typeof update.recovery === "string" && update.recovery !== "" ? update.recovery : null;
  return {
    message,
    recovery,
    warning: recovery ? OPTIMIZE_RECOVERY_WARNING : null,
    canResume: recovery !== null && update.op === "optimize",
  };
}

/** Coarse clusters (over 4 KiB) on a stick with many small files: worth optimizing. */
export function shouldSuggestOptimize(op, summary) {
  if (op !== "add" || !summary) return false;
  if (!(summary.clusterBytes > 4096)) return false;
  if (!(summary.files >= 1000)) return false;
  return summary.bytes / summary.files <= 256 * 1024;
}

/** The key a dismissed tip is remembered by: the drive serial, or its id when it has none. */
export function tipKey(drive) {
  if (!drive) return "";
  return typeof drive.serial === "string" && drive.serial !== "" ? drive.serial : String(drive.id || "");
}

/**
 * Whether the optimize tip shows in the done view of a finished add job.
 * @param {JobUpdate|null} job
 * @param {Drive|null} drive
 * @param {string[]} dismissed  tipKey values the user dismissed
 */
export function shouldShowOptimizeTip(job, drive, dismissed) {
  if (!job || job.stage !== "done" || !job.summary || !drive) return false;
  if (drive.isExfat !== true) return false;
  const key = tipKey(drive);
  if (key === "" || (Array.isArray(dismissed) && dismissed.includes(key))) return false;
  return shouldSuggestOptimize(job.op, job.summary);
}

/**
 * The stored list of dismissed tip keys. Anything malformed reads as an empty list.
 * @param {string|null} raw  the stored JSON text
 * @returns {string[]}
 */
export function parseDismissed(raw) {
  if (typeof raw !== "string") return [];
  try {
    const value = JSON.parse(raw);
    return Array.isArray(value) ? value.filter((k) => typeof k === "string") : [];
  } catch {
    return [];
  }
}

/** The list with `key` added (no duplicates). */
export function addDismissed(list, key) {
  const base = Array.isArray(list) ? list : [];
  return base.includes(key) ? base.slice() : [...base, key];
}

/**
 * Which optimize entry the Drive card offers: "optimize" for a trusted exFAT drive,
 * "resume" for a trusted drive with no filesystem while an interrupted optimize is pending,
 * null otherwise (including while a job runs).
 * @param {AppState} state
 * @param {boolean} pending  optimize_pending()
 * @returns {"optimize"|"resume"|null}
 */
export function optimizeAction(state, pending) {
  const drive = selectedDrive(state);
  if (!drive || jobActive(state) || !isTrusted(drive)) return null;
  if (drive.isExfat === true) return "optimize";
  if (drive.fsType === "" && pending === true) return "resume";
  return null;
}

/** True when an optimize entry applies (see optimizeAction). */
export function canOptimize(state, pending) {
  return optimizeAction(state, pending) !== null;
}

/** The entry button's text for an action from optimizeAction. */
export function optimizeEntryLabel(action) {
  if (action === "resume") return "Resume optimize…";
  if (action === "optimize") return "Optimize this stick…";
  return "";
}

/** "32 KB", "4 KB"; other sizes use formatBytes. */
export function clusterText(bytes) {
  return Number.isInteger(bytes) && bytes >= 1024 && bytes % 1024 === 0
    ? `${bytes / 1024} KB`
    : formatBytes(bytes);
}

/** "Usually between 10 s and 2 min 30 s, depending on the stick; slow USB 2 sticks take longest." */
export function optimizeTimeText(plan) {
  const { lowSecs, highSecs } = optimizeEstimate(plan);
  return (
    `Usually between ${formatDuration(lowSecs)} and ${formatDuration(highSecs)}, ` +
    "depending on the stick; slow USB 2 sticks take longest."
  );
}

/** "Needs 1.2 GB free on this computer (temporary copy in /x/y); 48.0 GB free." */
export function optimizeSpaceText(plan) {
  const temp = typeof plan.temp === "string" && plan.temp !== "" ? ` (temporary copy in ${plan.temp})` : "";
  const free = Number.isFinite(plan.tempFree) ? `${formatBytes(plan.tempFree)} free` : "free space unknown";
  return `Needs ${formatBytes(nonNegative(plan.needBytes))} free on this computer${temp}; ${free}.`;
}

/** "Kept: ... and the label “X”." */
export function optimizeKeptText(plan) {
  const label = typeof plan.label === "string" && plan.label !== "" ? `, and the label “${plan.label}”` : "";
  return `Kept: file contents, file and folder names, folders, modification times${label}.`;
}

/** "Cluster size: 32 KB → 4 KB", or null when either size is unknown. */
export function optimizeClusterText(plan) {
  if (!(plan.oldCluster > 0) || !(plan.newCluster > 0)) return null;
  return `Cluster size: ${clusterText(plan.oldCluster)} → ${clusterText(plan.newCluster)}`;
}

/** The explanation for a resume plan. */
export function optimizeResumeText(plan) {
  const where =
    typeof plan.temp === "string" && plan.temp !== ""
      ? `the verified copy in ${plan.temp}`
      : "the verified copy on this computer";
  return `An interrupted optimize of this stick was found. It will be finished from ${where}.`;
}

/**
 * "already" (nothing to do), "refused" (not enough space: an error, no confirm),
 * "resume" or "ready" (both can be confirmed).
 * @param {OptimizePlan} plan
 * @returns {"already"|"refused"|"resume"|"ready"}
 */
export function optimizePlanMode(plan) {
  if (plan.alreadyOptimized === true) return "already";
  if (plan.enoughSpace !== true) return "refused";
  if (plan.tempVolatile === true) return "volatile";
  // Without a confirmation token the backend would refuse to start it anyway.
  if (typeof plan.token !== "string" || plan.token === "") return "refused";
  return plan.resume === true ? "resume" : "ready";
}

/**
 * Everything the plan panel shows for a plan, in plain words.
 * - lines: explanatory paragraphs, in order
 * - note: the "nothing to do" sentence
 * - error: why it cannot run (shown with role=alert)
 * - canConfirm / confirmLabel / risk: the single confirmation and the line above it
 * @param {OptimizePlan|null} plan
 */
export function optimizePlanView(plan) {
  if (!plan || typeof plan !== "object") return null;
  const mode = optimizePlanMode(plan);
  const canConfirm = mode === "ready" || mode === "resume";
  const view = {
    mode,
    canConfirm,
    confirmLabel: mode === "resume" ? "Resume optimize" : "Optimize this stick",
    risk: canConfirm ? OPTIMIZE_RISK : "",
    lines: [],
    note: null,
    error: null,
  };
  if (mode === "already") {
    view.note = "This stick is already optimized. Nothing to do.";
  } else if (mode === "volatile") {
    view.error =
      `The temporary copy would be kept in memory (${plan.temp || "your cache folder"} is on tmpfs), ` +
      "so a crash while the stick is rebuilt could lose your files. Optimize needs a folder on a disk.";
  } else if (mode === "refused") {
    view.error = plan.enoughSpace === true
      ? "This plan cannot be started. Close it and check the stick again."
      : `${optimizeSpaceText(plan)} Free up space on this computer, then check again.`;
  } else {
    if (mode === "resume") view.lines.push(optimizeResumeText(plan));
    view.lines.push(OPTIMIZE_WHAT, optimizeKeptText(plan), OPTIMIZE_LOST, optimizeTimeText(plan));
    view.lines.push(optimizeSpaceText(plan));
    const cluster = optimizeClusterText(plan);
    if (cluster) view.lines.push(cluster);
  }
  return view;
}

/**
 * Open the plan panel for a drive while optimize_plan runs.
 * @param {AppState} state
 * @param {string} driveId
 */
export function openOptimize(state, driveId) {
  return { ...state, optimize: { driveId, status: "checking", plan: null, error: null } };
}

/** The plan came back. */
export function optimizePlanned(state, plan) {
  if (!state.optimize) return state;
  return { ...state, optimize: { ...state.optimize, status: "ready", plan, error: null } };
}

/** optimize_plan failed; the message is shown in the panel. */
export function optimizeFailed(state, message) {
  if (!state.optimize) return state;
  return { ...state, optimize: { ...state.optimize, status: "error", plan: null, error: message } };
}

/** Close the plan panel. */
export function closeOptimize(state) {
  return { ...state, optimize: null };
}

/**
 * Start an optimize job on a drive: the plan panel closes and the job starts in the
 * progress view. Same as beginJob for the progress state.
 * @param {AppState} state
 * @param {string} driveId
 */
export function beginOptimize(state, driveId) {
  return { ...beginJob(state, "optimize", 1), optimize: null, optimizeDriveId: driveId };
}

// ---- USB drive mode (the flashcp drived daemon) -------------------------

/**
 * @typedef {Object} DriveModeDrive
 * @property {string} device
 * @property {string} label
 * @property {"flashcp"|"kernel"|"none"|string} mode
 * @property {string} state
 * @property {string|null} mountpoint
 * @property {number} dirtyBytes
 * @property {number} dirtyFiles
 * @property {boolean} safeToRemove
 * @property {boolean} [canConvert] "Make faster" can run (the drive would gain from it)
 * @property {number} [convertSecs] about how many seconds it takes
 * @property {number} [convertDone] bytes written so far while converting
 * @property {number} [convertTotal]
 * @property {string|null} [convertResult] "done", "refused" or "failed"
 * @property {string|null} [convertMessage]
 */

const DRIVE_STATE_BADGES = {
  checking: "Checking…",
  busy: "In use",
  starting: "Starting…",
  ejecting: "Ejecting…",
  stopping: "Stopping…",
  mounting: "Mounting…",
  read_only: "Read-only",
  converting: "Making faster…",
  unmounted: "Not mounted",
  stopped: "Stopped",
  failed: "Failed",
};

/** Badge text and tone ("good", "neutral", "warn", "bad") for one drive row. */
export function driveModeBadge(drive) {
  const special = DRIVE_STATE_BADGES[drive.state];
  if (special) {
    const tone = drive.state === "failed" ? "bad" : drive.state === "read_only" ? "warn" : "neutral";
    return { text: special, tone };
  }
  if (drive.mode === "flashcp") return { text: "Fast (flashcp)", tone: "good" };
  if (drive.mode === "kernel" || drive.state === "kernel") return { text: "Normal driver", tone: "neutral" };
  return { text: "Not in use", tone: "neutral" };
}

/** "Saving 12 MB…" while data is still waiting to be written; "" otherwise. */
export function drivePendingText(drive) {
  return drive.dirtyBytes > 0 ? `Saving ${formatBytes(drive.dirtyBytes)}…` : "";
}

/** Which controls a drive row shows. Eject only for drives flashcp or the kernel serves. */
export function driveModeButtons(drive) {
  const served = drive.mode === "flashcp" || drive.mode === "kernel";
  const leaving = drive.state === "ejecting" || drive.state === "stopping" || drive.state === "converting";
  return { eject: served, ejectDisabled: leaving, safe: drive.safeToRemove === true && !(drive.dirtyBytes > 0) };
}

/** "7 seconds", "a minute and a half", "4 minutes" (as the drive service says it). */
export function secondsText(secs) {
  const s = Math.max(0, Math.ceil(Number(secs) || 0));
  if (s <= 1) return "a second";
  if (s < 90) return `${s} seconds`;
  if (s < 120) return "a minute and a half";
  return `${Math.ceil(s / 60)} minutes`;
}

/** Whether a drive row offers "Make faster": flashcp serves it, it is idle, and it would gain. */
export function canMakeFaster(drive) {
  return drive.canConvert === true && drive.mode === "flashcp" && (drive.state === "ready" || drive.state === "saving");
}

/** The windows note of a conversion, in one sentence. */
export const FASTER_WINDOWS_NOTE =
  "If it loses power in the second it switches over, Windows may show it as unformatted until you plug it in here again; your files stay safe.";

/** The confirm dialog: the same words as the notification. No jargon. */
export function makeFasterTexts(drive) {
  const name = drive.label || drive.device;
  return {
    title: `Make ${name} faster for small files?`,
    body:
      `flashcp can change how ${name} stores files so that small files copy much faster. Nothing is ` +
      `erased. It takes about ${secondsText(drive.convertSecs)} and you must not unplug it meanwhile.`,
    windows: FASTER_WINDOWS_NOTE,
    confirm: "Make it faster",
  };
}

/** "Making KINGSTON faster… don't unplug it (42 %)" while it runs; "" otherwise. */
export function makeFasterProgress(drive) {
  if (drive.state !== "converting") return "";
  const name = drive.label || drive.device;
  const total = Number(drive.convertTotal) || 0;
  const pct = total > 0 ? ` (${Math.min(100, Math.floor(((Number(drive.convertDone) || 0) * 100) / total))} %)` : "";
  return `Making ${name} faster… don't unplug it${pct}`;
}

/** The line under the switch: the service note, what the switch is doing, or "". */
export function driveModeNote(status, pendingEnabled) {
  if (pendingEnabled === true) return "Turning on…";
  if (pendingEnabled === false) return "Turning off…";
  if (status && status.running === false) return "The drive service isn't running";
  return "";
}

/** The command that starts the drive service, shown when it isn't running. */
export const DRIVE_SERVICE_HINT = "systemctl --user start flashcp-drived";
