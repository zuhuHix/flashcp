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
 */

/**
 * @typedef {Object} JobUpdate
 * @property {"starting"|"unmounting"|"authorizing"|"running"|"remounting"|"done"|"error"} stage
 *   "starting" is client-only: set locally before start_copy/start_erase is invoked.
 * @property {"add"|"build"} op
 * @property {number} index
 * @property {number} total
 * @property {number} filesDone
 * @property {number} bytesDone
 * @property {number} planFiles
 * @property {number} planBytes
 * @property {string|null} message
 * @property {JobSummary|null} summary
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
export function defaultSelection(drives, currentId) {
  if (currentId != null && drives.some((d) => d.id === currentId)) return currentId;
  const exfat = drives.filter((d) => d.isExfat === true);
  return exfat.length === 1 ? exfat[0].id : null;
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
  return { ...state, selectedId: id, eraseTyped: "", trustOpen: false, trustError: null };
}

/** Show the trust confirm panel. */
export function openTrust(state) {
  return { ...state, trustOpen: true, trustError: null };
}

/** Hide the trust confirm panel and clear its busy flag and error. */
export function closeTrust(state) {
  return { ...state, trustOpen: false, trustBusy: false, trustError: null };
}
