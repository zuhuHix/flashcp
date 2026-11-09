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
  test("expectedConfirm is the label, else the device basename", () => {
    assert.equal(expectedConfirm(exfat()), "FAKEUSB");
    assert.equal(expectedConfirm(vfat()), "sdy1");
    assert.equal(expectedConfirm(null), "");
  });
  test("confirmMatches is exact and case-sensitive", () => {
    assert.equal(confirmMatches(exfat(), "FAKEUSB"), true);
    assert.equal(confirmMatches(exfat(), "fakeusb"), false);
    assert.equal(confirmMatches(exfat(), "FAKEUSB "), false);
    assert.equal(confirmMatches(exfat(), ""), false);
    assert.equal(confirmMatches(vfat(), "sdy1"), true);
    assert.equal(confirmMatches(null, ""), false);
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
  test("blocked while a job is running, allowed after done or error", () => {
    const running = applyJobUpdate(baseState(), upd({ stage: "running" }));
    assert.equal(canCopy(running), false);
    assert.equal(canCopy(applyJobUpdate(baseState(), upd({ stage: "done", summary: null }))), true);
    assert.equal(canCopy(applyJobUpdate(baseState(), upd({ stage: "error", message: "x" }))), true);
  });
});

describe("canErase", () => {
  test("needs exactly one source and the exact confirmation", () => {
    const s = baseState({ drives: [vfat()], selectedId: vfat().id, sources: ["/home/zuhu/a"] });
    assert.equal(canErase(s, "sdy1"), true);
    assert.equal(canErase(s, "SDY1"), false);
    assert.equal(canErase(s, ""), false);
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
    assert.match(eraseSourceHint(3), /but 3 are selected/);
  });
});
