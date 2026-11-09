// Run with: node --test gui/ui/tests/
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  formatBytes,
  formatSpeed,
  formatDuration,
  progressFraction,
  progressText,
  stageText,
  baseName,
  expectedConfirm,
  confirmMatches,
  confirmHint,
  trustState,
  isTrusted,
  canTrust,
  trustNotice,
  trustDetails,
  trustArgs,
  selectDrive,
  openTrust,
  closeTrust,
  driveTitle,
  nonExfatText,
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
  jobActive,
  isoLocalDate,
  sanitizeLabel,
  defaultPullDest,
  normalizeFrom,
  joinFrom,
  parentFrom,
  breadcrumbs,
  folderRows,
  isAbsolutePath,
  pullDestination,
  canPull,
  pullArgs,
  setMode,
  setPullFrom,
  setPullDest,
  resetPullDest,
  folderKey,
  browseKey,
  setFolders,
  showCollision,
  clearCollision,
  collisionText,
  pullSummaryText,
  applyPullRequest,
} from "../logic.js";

const exfat = (over = {}) => ({
  id: "/org/block/sdz1",
  device: "/dev/sdz1",
  label: "FAKEUSB",
  fsType: "exfat",
  isExfat: true,
  size: 32_000_000_000,
  mountPoint: "/run/media/zuhu/FAKEUSB",
  model: "SanDisk Ultra",
  wholeDisk: false,
  serial: "FAKE0001",
  identity: { keyKind: "serial", key: "FAKE0001", wwn: "-", model: "Ultra", diskSize: 32_015_122_432 },
  trusted: true,
  ...over,
});
const vfat = (over = {}) => ({
  id: "/org/block/sdy1",
  device: "/dev/sdy1",
  label: "",
  fsType: "vfat",
  isExfat: false,
  size: 8_000_000_000,
  mountPoint: null,
  model: "Kingston",
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
  ...over,
});

const baseState = (over = {}) => ({
  drives: [exfat()],
  selectedId: "/org/block/sdz1",
  sources: ["/home/zuhu/photos"],
  destDir: "/",
  skipExisting: false,
  job: null,
  settings: { autostart: false, notifyOnPlug: true },
  eraseTyped: "",
  notice: null,
  ...over,
});

const upd = (over = {}) => ({
  stage: "running",
  op: "add",
  index: 0,
  total: 2,
  filesDone: 0,
  bytesDone: 0,
  planFiles: 0,
  planBytes: 0,
  message: null,
  summary: null,
  ...over,
});

describe("formatBytes", () => {
  test("bytes below 1000 are integers", () => {
    assert.equal(formatBytes(0), "0 B");
    assert.equal(formatBytes(999), "999 B");
  });
  test("1000-based units with one decimal under 100", () => {
    assert.equal(formatBytes(1000), "1.0 kB");
    assert.equal(formatBytes(1500), "1.5 kB");
    assert.equal(formatBytes(50_000_000), "50.0 MB");
    assert.equal(formatBytes(1_000_000_000), "1.0 GB");
  });
  test("no decimals from 100 of a unit up", () => {
    assert.equal(formatBytes(512_000_000), "512 MB");
    assert.equal(formatBytes(999_000), "999 kB");
  });
  test("rounding up to 1000 promotes to the next unit", () => {
    assert.equal(formatBytes(999_960), "1.0 MB");
  });
  test("terabytes are the last unit", () => {
    assert.equal(formatBytes(2_000_000_000_000), "2.0 TB");
    assert.equal(formatBytes(5e15), "5000 TB");
  });
  test("invalid input is 0 B", () => {
    assert.equal(formatBytes(NaN), "0 B");
    assert.equal(formatBytes(-5), "0 B");
  });
});

describe("formatSpeed", () => {
  test("MB/s from bytes and seconds", () => {
    assert.equal(formatSpeed(100_000_000, 2), "50.0 MB/s");
  });
  test("dash when seconds is not positive", () => {
    assert.equal(formatSpeed(100, 0), "—");
    assert.equal(formatSpeed(100, -1), "—");
    assert.equal(formatSpeed(100, NaN), "—");
  });
});

describe("formatDuration", () => {
  test("two decimals under 10 s", () => {
    assert.equal(formatDuration(0.42), "0.42 s");
    assert.equal(formatDuration(0), "0.00 s");
  });
  test("one decimal from 10 s to 60 s", () => {
    assert.equal(formatDuration(12.34), "12.3 s");
    assert.equal(formatDuration(59.2), "59.2 s");
  });
  test("minutes and zero-padded seconds from 60 s", () => {
    assert.equal(formatDuration(125), "2 min 05 s");
    assert.equal(formatDuration(60), "1 min 00 s");
  });
  test("seconds rounding to 60 carries into minutes", () => {
    assert.equal(formatDuration(119.6), "2 min 00 s");
  });
  test("unusable values give a dash", () => {
    assert.equal(formatDuration(-1), "—");
    assert.equal(formatDuration(NaN), "—");
  });
});

describe("progressFraction", () => {
  test("bytesDone over planBytes", () => {
    assert.equal(progressFraction({ bytesDone: 25, planBytes: 100 }), 0.25);
  });
  test("clamped to 0..1", () => {
    assert.equal(progressFraction({ bytesDone: 500, planBytes: 100 }), 1);
    assert.equal(progressFraction({ bytesDone: -5, planBytes: 100 }), 0);
  });
  test("0 when nothing is planned", () => {
    assert.equal(progressFraction({ bytesDone: 0, planBytes: 0 }), 0);
    assert.equal(progressFraction({ bytesDone: 10, planBytes: 0 }), 0);
  });
});

describe("stageText and progressText", () => {
  test("each stage has its wording", () => {
    assert.equal(stageText(upd({ stage: "unmounting" })), "Unmounting drive…");
    assert.equal(stageText(upd({ stage: "authorizing" })), "Waiting for authorization…");
    assert.equal(stageText(upd({ stage: "remounting" })), "Remounting…");
    assert.equal(stageText(upd({ stage: "starting" })), "Starting…");
  });
  test("running counts items from one", () => {
    assert.equal(stageText(upd({ stage: "running", index: 0, total: 3 })), "Copying item 1 of 3…");
    assert.equal(stageText(upd({ stage: "running", index: 2, total: 3 })), "Copying item 3 of 3…");
  });
  test("error shows its message, done has a fallback", () => {
    assert.equal(stageText(upd({ stage: "error", message: "disk full" })), "disk full");
    assert.equal(stageText(upd({ stage: "error", message: null })), "Failed.");
    assert.equal(stageText(upd({ stage: "done" })), "Done.");
  });
  test("progress line counts files and bytes", () => {
    assert.equal(
      progressText(upd({ filesDone: 3, planFiles: 10, bytesDone: 1500, planBytes: 50_000_000 })),
      "3 of 10 files · 1.5 kB of 50.0 MB",
    );
  });
});

describe("drive labels and confirmation", () => {
  test("baseName takes the last segment", () => {
    assert.equal(baseName("/dev/sdb1"), "sdb1");
    assert.equal(baseName("sdb1"), "sdb1");
    assert.equal(baseName(""), "");
  });
  test("expectedConfirm is the label for a labelled exFAT volume", () => {
    assert.equal(expectedConfirm(exfat()), "FAKEUSB");
  });
  test("expectedConfirm is the serial for an unlabelled exFAT volume", () => {
    assert.equal(expectedConfirm(exfat({ label: "" })), "FAKE0001");
  });
  test("expectedConfirm is the serial for vfat, even when it has a label", () => {
    assert.equal(expectedConfirm(vfat()), "OLD0002");
    assert.equal(expectedConfirm(vfat({ label: "OLDSTICK" })), "OLD0002");
  });
  test("expectedConfirm is empty without a drive or a serial to fall back on", () => {
    assert.equal(expectedConfirm(null), "");
    assert.equal(expectedConfirm(vfat({ serial: "" })), "");
    assert.equal(expectedConfirm(exfat({ serial: "", label: "" })), "");
  });
  test("expectedConfirm uses the label of an untrustable exFAT volume", () => {
    assert.equal(expectedConfirm(exfat({ serial: "" })), "FAKEUSB");
  });
  test("confirmMatches is exact and case-sensitive", () => {
    assert.equal(confirmMatches(exfat(), "FAKEUSB"), true);
    assert.equal(confirmMatches(exfat(), "fakeusb"), false);
    assert.equal(confirmMatches(exfat(), "FAKEUSB "), false);
    assert.equal(confirmMatches(exfat(), ""), false);
    assert.equal(confirmMatches(vfat(), "OLD0002"), true);
    assert.equal(confirmMatches(vfat(), "OLDSTICK"), false);
    assert.equal(confirmMatches(vfat({ serial: "" }), ""), false);
    assert.equal(confirmMatches(null, ""), false);
  });
  test("confirmHint names the label or the serial", () => {
    assert.equal(confirmHint(exfat()), "Type the drive name FAKEUSB to confirm");
    assert.equal(confirmHint(exfat({ label: "" })), "Type the drive's serial number FAKE0001 to confirm");
    assert.equal(confirmHint(vfat({ label: "OLDSTICK" })), "Type the drive's serial number OLD0002 to confirm");
    assert.equal(confirmHint(null), "Select a drive first.");
    assert.equal(confirmHint(vfat({ serial: "" })), "This drive has no name or serial number to type.");
  });
  test("trustState is trusted, untrusted or untrustable", () => {
    assert.equal(trustState(exfat()), "trusted");
    assert.equal(trustState(vfat()), "untrusted");
    assert.equal(trustState(vfat({ serial: "" })), "untrustable");
    assert.equal(trustState(exfat({ serial: "", trusted: true })), "untrustable");
  });
  test("isTrusted and canTrust follow trustState", () => {
    assert.equal(isTrusted(exfat()), true);
    assert.equal(isTrusted(vfat()), false);
    assert.equal(isTrusted(vfat({ serial: "" })), false);
    assert.equal(canTrust(vfat()), true);
    assert.equal(canTrust(exfat()), false);
    assert.equal(canTrust(vfat({ serial: "" })), false);
    assert.equal(canTrust(null), false);
  });
  test("trustNotice explains an untrusted or untrustable drive", () => {
    assert.equal(trustNotice(exfat()), "");
    assert.equal(trustNotice(vfat()), "flashcp hasn't been allowed to write to this drive yet.");
    assert.equal(
      trustNotice(vfat({ serial: "" })),
      "This drive doesn't report a usable serial number or WWN, so flashcp can't remember it safely. You can still copy to it from the command line: sudo flashcp add <folder> /dev/sdXN",
    );
  });
  test("trustDetails shows exactly the checked identity tuple", () => {
    assert.deepEqual(trustDetails(vfat()), [
      { label: "Serial", value: "OLD0002" },
      { label: "Model", value: "DataTraveler" },
      { label: "Disk size (whole disk)", value: "8.0 GB (8,004,829,184 bytes)" },
      { label: "WWN", value: "0x50014ee2b5a1c3d7" },
    ]);
    // Not the friendly vendor+model, not the partition size.
    const shown = trustDetails(vfat()).map((r) => r.value).join("|");
    assert.ok(!shown.includes("Kingston"));
    assert.ok(!shown.includes("8,000,000,000"));
    // A WWN key is labelled as such, and isn't repeated.
    const wwnKeyed = vfat({
      identity: { keyKind: "wwn", key: "0x5001", wwn: "0x5001", model: "-", diskSize: 1000 },
    });
    assert.deepEqual(
      trustDetails(wwnKeyed).map((r) => r.label),
      ["WWN", "Model", "Disk size (whole disk)"],
    );
    assert.deepEqual(trustDetails(null), []);
    assert.deepEqual(trustDetails(vfat({ identity: { keyKind: "", key: "", wwn: "-", model: "-", diskSize: 1 } })), []);
  });
  test("every value flashcp-trust receives is shown in the dialog, one to one", () => {
    for (const d of [exfat(), vfat()]) {
      const rows = trustDetails(d);
      const byLabel = Object.fromEntries(rows.map((r) => [r.label, r.value]));
      const args = trustArgs(d);
      assert.equal(byLabel[d.identity.keyKind === "wwn" ? "WWN" : "Serial"], args.expect);
      assert.equal(byLabel.Model, args.model);
      assert.ok(byLabel["Disk size (whole disk)"].includes(`(${Number(args.size).toLocaleString("en-US")} bytes)`));
      if (args.wwn !== "-") assert.equal(byLabel.WWN, args.wwn);
      else assert.equal(byLabel.WWN, undefined);
      // Nothing else: one row per checked field.
      assert.equal(rows.length, args.wwn !== "-" ? 4 : 3);
    }
  });
  test("driveTitle falls back to Untitled", () => {
    assert.equal(driveTitle(exfat()), "FAKEUSB");
    assert.equal(driveTitle(vfat()), "Untitled");
  });
  test("nonExfatText names the filesystem", () => {
    assert.match(nonExfatText(vfat()), /^flashcp only works on exFAT drives\. This drive is VFAT\./);
    assert.match(nonExfatText(vfat()), /everything on it will be lost\.$/);
    assert.match(nonExfatText(vfat({ fsType: "" })), /This drive has no filesystem\./);
  });
});

describe("canCopy", () => {
  test("allowed for exFAT, sources, valid destination, no job", () => {
    assert.equal(canCopy(baseState()), true);
  });
  test("blocked without a drive, on non-exFAT, without sources", () => {
    assert.equal(canCopy(baseState({ selectedId: null })), false);
    assert.equal(canCopy(baseState({ drives: [vfat()], selectedId: vfat().id })), false);
    assert.equal(canCopy(baseState({ sources: [] })), false);
  });
  test("blocked by an invalid destination", () => {
    assert.equal(canCopy(baseState({ destDir: "/a/../b" })), false);
  });
  test("blocked on an untrusted or untrustable exFAT drive", () => {
    assert.equal(canCopy(baseState({ drives: [exfat({ trusted: false })] })), false);
    assert.equal(canCopy(baseState({ drives: [exfat({ serial: "", trusted: true })] })), false);
  });
  test("blocked while a job is running, allowed after done or error", () => {
    const running = applyJobUpdate(baseState(), upd({ stage: "running" }));
    assert.equal(canCopy(running), false);
    assert.equal(canCopy(applyJobUpdate(baseState(), upd({ stage: "done", summary: null }))), true);
    assert.equal(canCopy(applyJobUpdate(baseState(), upd({ stage: "error", message: "x" }))), true);
  });
});

describe("canErase", () => {
  test("needs exactly one source and the exact confirmation", () => {
    const s = baseState({
      drives: [vfat({ trusted: true })],
      selectedId: vfat().id,
      sources: ["/home/zuhu/a"],
    });
    assert.equal(canErase(s, "OLD0002"), true);
    assert.equal(canErase(s, "old0002"), false);
    assert.equal(canErase(s, "OLDSTICK"), false);
    assert.equal(canErase(s, ""), false);
  });
  test("requires the drive to be trusted", () => {
    assert.equal(canErase(baseState(), "FAKEUSB"), true);
    assert.equal(canErase(baseState({ drives: [exfat({ trusted: false })] }), "FAKEUSB"), false);
    assert.equal(canErase(baseState({ drives: [exfat({ serial: "", trusted: true })] }), "FAKEUSB"), false);
  });
  test("blocked with zero or several sources", () => {
    const none = baseState({ sources: [] });
    const two = baseState({ sources: ["/a", "/b"] });
    assert.equal(canErase(none, "FAKEUSB"), false);
    assert.equal(canErase(two, "FAKEUSB"), false);
  });
  test("blocked while a job is in flight or without a drive", () => {
    assert.equal(canErase(baseState({ selectedId: null }), "FAKEUSB"), false);
    const busy = applyJobUpdate(baseState(), upd({ stage: "unmounting" }));
    assert.equal(canErase(busy, "FAKEUSB"), false);
  });
});

describe("mergeSources", () => {
  test("keeps order and dedupes across and within lists", () => {
    assert.deepEqual(mergeSources(["/a", "/b"], ["/c", "/a", "/c"]), ["/a", "/b", "/c"]);
  });
  test("ignores non-absolute and non-string entries", () => {
    assert.deepEqual(mergeSources([], ["rel/path", "", "/ok", 7, null]), ["/ok"]);
  });
  test("does not mutate its inputs", () => {
    const existing = ["/a"];
    mergeSources(existing, ["/b"]);
    assert.deepEqual(existing, ["/a"]);
  });
});

describe("removeSource", () => {
  test("removes by index without mutating", () => {
    const src = ["/a", "/b", "/c"];
    assert.deepEqual(removeSource(src, 1), ["/a", "/c"]);
    assert.deepEqual(src, ["/a", "/b", "/c"]);
  });
  test("out of range returns a copy", () => {
    assert.deepEqual(removeSource(["/a"], 5), ["/a"]);
    assert.deepEqual(removeSource(["/a"], -1), ["/a"]);
  });
});

describe("defaultSelection", () => {
  test("keeps the current drive while it is present", () => {
    assert.equal(defaultSelection([exfat(), vfat()], vfat().id), vfat().id);
  });
  test("selects the only exFAT drive otherwise", () => {
    assert.equal(defaultSelection([vfat(), exfat()], null), exfat().id);
    assert.equal(defaultSelection([vfat(), exfat()], "/gone"), exfat().id);
  });
  test("null with several exFAT drives or none", () => {
    assert.equal(defaultSelection([exfat(), exfat({ id: "/x" })], null), null);
    assert.equal(defaultSelection([vfat()], null), null);
    assert.equal(defaultSelection([], null), null);
  });
});

describe("normalizeDestDir", () => {
  test("root and empty map to /", () => {
    assert.equal(normalizeDestDir("/"), "/");
    assert.equal(normalizeDestDir(""), "/");
    assert.equal(normalizeDestDir("   "), "/");
    assert.equal(normalizeDestDir("//"), "/");
  });
  test("adds a leading slash and trims", () => {
    assert.equal(normalizeDestDir("Photos"), "/Photos");
    assert.equal(normalizeDestDir("  /Photos/2026  "), "/Photos/2026");
  });
  test("collapses doubled slashes, drops . and trailing slash", () => {
    assert.equal(normalizeDestDir("//a///b/"), "/a/b");
    assert.equal(normalizeDestDir("/./a/."), "/a");
  });
  test("rejects parent segments", () => {
    assert.equal(normalizeDestDir("/a/../b"), null);
    assert.equal(normalizeDestDir(".."), null);
  });
  test("does not treat ..foo as a parent segment", () => {
    assert.equal(normalizeDestDir("/..foo"), "/..foo");
  });
  test("non-strings are rejected", () => {
    assert.equal(normalizeDestDir(undefined), null);
    assert.equal(normalizeDestDir(3), null);
  });
});

describe("summaryText", () => {
  test("reads like the done line", () => {
    const summary = { files: 1200, bytes: 500_000_000, secs: 4, wallSecs: 6, mountPoint: null };
    assert.equal(summaryText(summary), "Copied 1200 files (500 MB) in 4.00 s — 125.0 MB/s");
  });
  test("singular file and minutes", () => {
    const summary = { files: 1, bytes: 1000, secs: 125, wallSecs: 130, mountPoint: null };
    assert.equal(summaryText(summary), "Copied 1 file (1.0 kB) in 2 min 05 s — 0.0 MB/s");
  });
  test("empty without a summary", () => {
    assert.equal(summaryText(null), "");
  });
});

describe("job reducer: successful copy", () => {
  test("unmounting -> authorizing -> running -> remounting -> done", () => {
    let s = baseState();
    s = beginJob(s, "add", 2);
    assert.equal(s.job.stage, "starting");
    assert.equal(jobActive(s), true);
    assert.equal(canCopy(s), false);

    s = applyJobUpdate(s, upd({ stage: "unmounting", total: 2 }));
    assert.equal(stageText(s.job), "Unmounting drive…");
    assert.equal(jobActive(s), true);

    s = applyJobUpdate(s, upd({ stage: "authorizing", total: 2 }));
    assert.equal(stageText(s.job), "Waiting for authorization…");

    s = applyJobUpdate(s, upd({ stage: "running", index: 0, total: 2, filesDone: 0, bytesDone: 0, planFiles: 4, planBytes: 400 }));
    assert.equal(stageText(s.job), "Copying item 1 of 2…");
    assert.equal(progressFraction(s.job), 0);

    s = applyJobUpdate(s, upd({ stage: "running", index: 0, total: 2, filesDone: 2, bytesDone: 100, planFiles: 4, planBytes: 400 }));
    assert.equal(progressFraction(s.job), 0.25);
    assert.equal(progressText(s.job), "2 of 4 files · 100 B of 400 B");

    s = applyJobUpdate(s, upd({ stage: "running", index: 1, total: 2, filesDone: 4, bytesDone: 400, planFiles: 4, planBytes: 400 }));
    assert.equal(stageText(s.job), "Copying item 2 of 2…");
    assert.equal(progressFraction(s.job), 1);

    s = applyJobUpdate(s, upd({ stage: "remounting", index: 1, total: 2, filesDone: 4, bytesDone: 400, planFiles: 4, planBytes: 400 }));
    assert.equal(stageText(s.job), "Remounting…");
    assert.equal(jobActive(s), true);

    const summary = { files: 4, bytes: 400, secs: 0.5, wallSecs: 3.2, mountPoint: "/run/media/zuhu/FAKEUSB" };
    s = applyJobUpdate(s, upd({ stage: "done", index: 1, total: 2, filesDone: 4, bytesDone: 400, planFiles: 4, planBytes: 400, summary }));
    assert.equal(jobActive(s), false);
    assert.deepEqual(s.job.summary, summary);
    assert.equal(summaryText(s.job.summary), "Copied 4 files (400 B) in 0.50 s — 0.0 MB/s");
    assert.equal(canCopy(s), true);

    s = resetForm(s);
    assert.equal(s.job, null);
    assert.deepEqual(s.sources, []);
    assert.equal(s.selectedId, "/org/block/sdz1");
  });
});

describe("job reducer: error", () => {
  test("error sequence keeps the message and frees the form after dismiss", () => {
    let s = beginJob(baseState(), "add", 1);
    s = applyJobUpdate(s, upd({ stage: "unmounting", total: 1 }));
    s = applyJobUpdate(s, upd({ stage: "authorizing", total: 1 }));
    s = applyJobUpdate(s, upd({ stage: "error", total: 1, message: "Authorization dismissed" }));
    assert.equal(s.job.stage, "error");
    assert.equal(s.job.message, "Authorization dismissed");
    assert.equal(s.job.summary, null);
    assert.equal(jobActive(s), false);
    assert.equal(canCopy(s), true);

    s = dismissJob(s);
    assert.equal(s.job, null);
  });
  test("an error after partial progress is still an error", () => {
    let s = applyJobUpdate(baseState(), upd({ stage: "running", index: 0, total: 2, filesDone: 3, bytesDone: 300, planFiles: 9, planBytes: 900 }));
    s = applyJobUpdate(s, upd({ stage: "error", index: 0, total: 2, message: "write failed", filesDone: 3, bytesDone: 300, planFiles: 9, planBytes: 900 }));
    assert.equal(stageText(s.job), "write failed");
    assert.equal(s.job.summary, null);
  });
  test("a non-done update never carries a summary forward", () => {
    const summary = { files: 1, bytes: 1, secs: 1, wallSecs: 1, mountPoint: null };
    let s = applyJobUpdate(baseState(), upd({ stage: "done", summary }));
    s = applyJobUpdate(s, upd({ stage: "running" }));
    assert.equal(s.job.summary, null);
  });
  test("ignores malformed updates", () => {
    const s = baseState();
    assert.equal(applyJobUpdate(s, null), s);
    assert.equal(applyJobUpdate(s, {}), s);
  });
});

describe("eraseSourceHint", () => {
  test("empty for exactly one, explains otherwise", () => {
    assert.equal(eraseSourceHint(1), "");
    assert.match(eraseSourceHint(0), /Add exactly one folder/);
    assert.match(eraseSourceHint(3), /exactly one folder, but 3 items are selected/);
  });
});

describe("selectDrive, openTrust and closeTrust", () => {
  test("choosing another drive closes trust and clears the typed confirmation", () => {
    const s = baseState({ eraseTyped: "FAKE", trustOpen: true, trustError: "x" });
    const next = selectDrive(s, vfat().id);
    assert.equal(next.selectedId, vfat().id);
    assert.equal(next.eraseTyped, "");
    assert.equal(next.trustOpen, false);
    assert.equal(next.trustError, null);
  });
  test("re-selecting the same drive keeps everything", () => {
    const s = baseState({ eraseTyped: "FAKE", trustOpen: true });
    assert.equal(selectDrive(s, s.selectedId), s);
  });
  test("openTrust shows the panel with no stale error", () => {
    const s = openTrust(baseState({ trustError: "old" }));
    assert.equal(s.trustOpen, true);
    assert.equal(s.trustError, null);
  });
  test("closeTrust hides the panel and clears busy and error", () => {
    const s = closeTrust(baseState({ trustOpen: true, trustBusy: true, trustError: "denied" }));
    assert.equal(s.trustOpen, false);
    assert.equal(s.trustBusy, false);
    assert.equal(s.trustError, null);
  });
});

const pullState = (over = {}) =>
  baseState({
    mode: "pull",
    downloads: "/home/zuhu/Downloads",
    pullFrom: "",
    pullFolders: null,
    pullDestOverride: null,
    pullCollision: null,
    ...over,
  });
const NOV9 = new Date(2026, 10, 9, 23, 30);

describe("isoLocalDate and sanitizeLabel", () => {
  test("uses the local calendar date, zero-padded", () => {
    assert.equal(isoLocalDate(NOV9), "2026-11-09");
    assert.equal(isoLocalDate(new Date(2026, 0, 2, 0, 5)), "2026-01-02");
  });
  test("rejects an invalid date", () => {
    assert.throws(() => isoLocalDate(new Date("nope")), TypeError);
    assert.throws(() => isoLocalDate("2026-11-09"), TypeError);
  });
  test("slashes and control characters become underscores, empty becomes USB", () => {
    assert.equal(sanitizeLabel("My/Stick\u0007"), "My_Stick_");
    assert.equal(sanitizeLabel("  FAKEUSB  "), "FAKEUSB");
    assert.equal(sanitizeLabel(""), "USB");
    assert.equal(sanitizeLabel("   "), "USB");
    assert.equal(sanitizeLabel(undefined), "USB");
  });
});

describe("defaultPullDest", () => {
  test("<downloads>/<label>-<date>", () => {
    assert.equal(defaultPullDest("/home/zuhu/Downloads", "FAKEUSB", NOV9), "/home/zuhu/Downloads/FAKEUSB-2026-11-09");
  });
  test("no label uses USB, a slash in the label cannot add a folder", () => {
    assert.equal(defaultPullDest("/home/zuhu/Downloads", "", NOV9), "/home/zuhu/Downloads/USB-2026-11-09");
    assert.equal(defaultPullDest("/home/zuhu/Downloads", "a/b", NOV9), "/home/zuhu/Downloads/a_b-2026-11-09");
  });
  test("a trailing slash on downloads is not doubled", () => {
    assert.equal(defaultPullDest("/home/zuhu/Downloads/", "X", NOV9), "/home/zuhu/Downloads/X-2026-11-09");
  });
});

describe("normalizeFrom", () => {
  test("trims surrounding slashes, root is empty", () => {
    assert.equal(normalizeFrom(""), "");
    assert.equal(normalizeFrom("/"), "");
    assert.equal(normalizeFrom("/Photos/2026/"), "Photos/2026");
    assert.equal(normalizeFrom("DCIM/100 CANON"), "DCIM/100 CANON");
  });
  test("refuses empty, dot and dot-dot components", () => {
    assert.equal(normalizeFrom("a//b"), null);
    assert.equal(normalizeFrom("a/./b"), null);
    assert.equal(normalizeFrom("a/../b"), null);
    assert.equal(normalizeFrom(".."), null);
    assert.equal(normalizeFrom("."), null);
  });
  test("refuses a leading dash, control characters, non-strings and over 1024 bytes", () => {
    assert.equal(normalizeFrom("-x"), null);
    // Only a leading dash on the whole value is refused (it could parse as a flag).
    assert.equal(normalizeFrom("a/-x"), "a/-x");
    assert.equal(normalizeFrom("a\nb"), null);
    assert.equal(normalizeFrom("a\u0085b"), null);
    assert.equal(normalizeFrom("a\u007fb"), null);
    assert.equal(normalizeFrom(undefined), null);
    assert.equal(normalizeFrom("a".repeat(1025)), null);
    assert.equal(normalizeFrom("a".repeat(1024)), "a".repeat(1024));
  });
});

describe("joinFrom, parentFrom, breadcrumbs, folderRows", () => {
  test("joinFrom appends one folder name", () => {
    assert.equal(joinFrom("", "DCIM"), "DCIM");
    assert.equal(joinFrom("DCIM", "100CANON"), "DCIM/100CANON");
  });
  test("joinFrom refuses names that are not one plain folder", () => {
    assert.equal(joinFrom("", ""), null);
    assert.equal(joinFrom("", "a/b"), null);
    assert.equal(joinFrom("", ".."), null);
    assert.equal(joinFrom("", "."), null);
    assert.equal(joinFrom("", "-x"), null);
    assert.equal(joinFrom("a", undefined), null);
    assert.equal(joinFrom("..", "x"), null);
  });
  test("parentFrom goes up one level, stops at the root", () => {
    assert.equal(parentFrom("DCIM/100CANON"), "DCIM");
    assert.equal(parentFrom("DCIM"), "");
    assert.equal(parentFrom(""), "");
    assert.equal(parentFrom("bad//path"), "");
  });
  test("breadcrumbs lists the root and each folder with its path", () => {
    assert.deepEqual(breadcrumbs(""), [{ label: "Drive", from: "" }]);
    assert.deepEqual(breadcrumbs("DCIM/100CANON"), [
      { label: "Drive", from: "" },
      { label: "DCIM", from: "DCIM" },
      { label: "100CANON", from: "DCIM/100CANON" },
    ]);
  });
  test("folderRows maps names to the path each one opens", () => {
    assert.deepEqual(folderRows(["DCIM", "-ok"], "Photos"), [
      { name: "DCIM", from: "Photos/DCIM" },
      { name: "-ok", from: "Photos/-ok" },
    ]);
    // At the root a leading "-" would start the --from value, so it cannot be opened.
    assert.deepEqual(folderRows(["-bad"], ""), [{ name: "-bad", from: null }]);
    assert.deepEqual(folderRows(undefined, ""), []);
  });
});

describe("pull destination and canPull", () => {
  test("isAbsolutePath needs a leading slash and more than the root", () => {
    assert.equal(isAbsolutePath("/home/x"), true);
    assert.equal(isAbsolutePath("home/x"), false);
    assert.equal(isAbsolutePath("/"), false);
    assert.equal(isAbsolutePath("//"), false);
    assert.equal(isAbsolutePath(""), false);
    assert.equal(isAbsolutePath("/a\u0000b"), false);
  });
  test("default destination for the selected drive, with the picked one overriding it", () => {
    const s = pullState();
    assert.equal(pullDestination(s, NOV9), "/home/zuhu/Downloads/FAKEUSB-2026-11-09");
    assert.equal(pullDestination({ ...s, pullDestOverride: "/data/copies/" }, NOV9), "/data/copies");
  });
  test("no destination before pull_defaults answers", () => {
    assert.equal(pullDestination(pullState({ downloads: null }), NOV9), null);
  });
  test("canPull needs trusted exfat, a valid folder, a destination and no job", () => {
    assert.equal(canPull(pullState()), true);
    assert.equal(canPull(pullState({ pullFrom: "DCIM" })), true);
    assert.equal(canPull(pullState({ pullFrom: "a//b" })), false);
    assert.equal(canPull(pullState({ downloads: null })), false);
    assert.equal(canPull(pullState({ drives: [vfat()], selectedId: vfat().id })), false);
    assert.equal(canPull(pullState({ drives: [exfat({ trusted: false })] })), false);
    assert.equal(canPull(pullState({ selectedId: null })), false);
    assert.equal(canPull(pullState({ job: upd({ stage: "running" }) })), false);
    assert.equal(canPull(pullState({ job: upd({ stage: "done", op: "pull" }) })), true);
  });
  test("pullArgs builds start_pull arguments and rejects a bad existing choice", () => {
    assert.deepEqual(pullArgs(pullState({ pullFrom: "/Photos/" }), "fail"), {
      driveId: "/org/block/sdz1",
      from: "Photos",
      dest: "/home/zuhu/Downloads/FAKEUSB-" + isoLocalDate(new Date()),
      existing: "fail",
    });
    assert.equal(pullArgs(pullState(), "merge"), null);
    assert.equal(pullArgs(pullState({ pullFrom: "a/../b" }), "fail"), null);
    assert.equal(pullArgs(pullState({ downloads: null }), "fail"), null);
  });
});

describe("pull state transitions", () => {
  test("setMode switches between copy and pull only", () => {
    const s = baseState({ mode: "copy" });
    assert.equal(setMode(s, "copy"), s);
    assert.equal(setMode(s, "pull").mode, "pull");
    assert.equal(setMode(s, "erase"), s);
  });
  test("setPullFrom stores the folder as given", () => {
    assert.equal(setPullFrom(pullState(), "DCIM").pullFrom, "DCIM");
    assert.equal(setPullFrom(pullState(), undefined).pullFrom, "");
  });
  test("setPullDest refuses a relative path and clears a collision on a good one", () => {
    const s = pullState({ pullCollision: { dest: "/x", source: "conflict" } });
    const bad = setPullDest(s, "relative/path");
    assert.equal(bad.pullDestOverride, null);
    assert.match(bad.notice, /full path/);
    const good = setPullDest(s, "/data/copies/");
    assert.equal(good.pullDestOverride, "/data/copies");
    assert.equal(good.pullCollision, null);
    assert.equal(good.notice, null);
  });
  test("resetPullDest returns to the default", () => {
    const s = resetPullDest(pullState({ pullDestOverride: "/data" }));
    assert.equal(s.pullDestOverride, null);
    assert.equal(pullDestination(s, NOV9), "/home/zuhu/Downloads/FAKEUSB-2026-11-09");
  });
  test("selectDrive forgets the folder and any collision", () => {
    const s = selectDrive(pullState({ pullFrom: "DCIM", pullCollision: { dest: "/x", source: "preflight" } }), "/other");
    assert.equal(s.pullFrom, "");
    assert.equal(s.pullCollision, null);
  });
  test("browseKey is null without a mount point or a valid folder", () => {
    assert.equal(browseKey(pullState()), folderKey("/org/block/sdz1", ""));
    assert.equal(browseKey(pullState({ pullFrom: "DCIM" })), "/org/block/sdz1\nDCIM");
    assert.equal(browseKey(pullState({ pullFrom: "a//b" })), null);
    assert.equal(browseKey(pullState({ drives: [exfat({ mountPoint: null })] })), null);
  });
  test("setFolders stores the listing for its key", () => {
    const key = folderKey("/org/block/sdz1", "");
    const s = setFolders(pullState(), key, ["DCIM", "Music"], null);
    assert.deepEqual(s.pullFolders, { key, names: ["DCIM", "Music"], error: null });
    const failed = setFolders(pullState(), key, undefined, "not mounted");
    assert.deepEqual(failed.pullFolders, { key, names: [], error: "not mounted" });
  });
  test("showCollision and clearCollision", () => {
    const s = showCollision(pullState(), "/data/x", "preflight");
    assert.deepEqual(s.pullCollision, { dest: "/data/x", source: "preflight" });
    assert.equal(clearCollision(s).pullCollision, null);
    assert.equal(collisionText("/data/x"), "/data/x already has files in it.");
  });
});

describe("pullSummaryText", () => {
  test("says what was copied, where and how long", () => {
    assert.equal(
      pullSummaryText({ files: 3, bytes: 1_500_000, secs: 1.8, wallSecs: 4, mountPoint: null, dest: "/home/x/USB-2026-11-09" }),
      "Copied 3 files (1.5 MB) to /home/x/USB-2026-11-09 in 1.80 s",
    );
  });
  test("singular file, and no destination when there is none", () => {
    assert.equal(
      pullSummaryText({ files: 1, bytes: 2_000, secs: 12.25, wallSecs: 13, mountPoint: null, dest: null }),
      "Copied 1 file (2.0 kB) in 12.3 s",
    );
  });
  test("empty without a summary", () => {
    assert.equal(pullSummaryText(null), "");
  });
});

describe("stageText for a pull", () => {
  test("copying says it is copying from the drive", () => {
    assert.equal(stageText(upd({ op: "pull", stage: "running", index: 0, total: 1 })), "Copying from the drive…");
    assert.equal(stageText(upd({ stage: "running", op: "add" })), "Copying item 1 of 2…");
  });
});

describe("applyPullRequest", () => {
  const two = [exfat(), vfat({ trusted: true, serial: "OLD0002" })];
  test("switches to pull, selects the drive and sets the folder", () => {
    const s = applyPullRequest(baseState({ drives: two, mode: "copy", pullCollision: { dest: "/x", source: "conflict" } }), {
      driveId: vfat().id,
      from: "/Documents/2026/",
      error: null,
    });
    assert.equal(s.mode, "pull");
    assert.equal(s.selectedId, vfat().id);
    assert.equal(s.pullFrom, "Documents/2026");
    assert.equal(s.pullCollision, null);
    assert.equal(s.notice, null);
  });
  test("an unknown drive keeps the selection; an invalid folder becomes the root", () => {
    const s = applyPullRequest(baseState({ drives: two }), { driveId: "/gone", from: "a/../b", error: null });
    assert.equal(s.selectedId, "/org/block/sdz1");
    assert.equal(s.pullFrom, "");
  });
  test("the backend error is shown as the notice", () => {
    const s = applyPullRequest(baseState({ drives: two }), {
      driveId: null,
      from: "",
      error: "That folder is not on a USB exFAT drive flashcp can see",
    });
    assert.equal(s.mode, "pull");
    assert.equal(s.notice, "That folder is not on a USB exFAT drive flashcp can see");
  });
  test("null or malformed requests change nothing", () => {
    const s = baseState();
    assert.equal(applyPullRequest(s, null), s);
    assert.equal(applyPullRequest(s, "x"), s);
  });
});
