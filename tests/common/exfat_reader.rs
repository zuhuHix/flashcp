//! Independent read-back parser for exFAT volumes.
//!
//! Written from the Microsoft "exFAT file system specification" (2019). It shares no code with
//! the flashcp writer: every offset and every check below comes from the spec's tables, so a
//! bug in the writer cannot hide itself by being mirrored here.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

const END_OF_CHAIN: u32 = 0xFFFF_FFFF;
const MEDIA_DESCRIPTOR_ENTRY: u32 = 0xFFFF_FFF8;
const ENTRY: usize = 32;
const ET_END: u8 = 0x00;
const ET_BITMAP: u8 = 0x81;
const ET_UPCASE: u8 = 0x82;
const ET_LABEL: u8 = 0x83;
const ET_FILE: u8 = 0x85;
const ET_STREAM: u8 = 0xC0;
const ET_NAME: u8 = 0xC1;
const IN_USE: u8 = 0x80;
const STREAM_ALLOC_POSSIBLE: u8 = 0x01;
const STREAM_NO_FAT_CHAIN: u8 = 0x02;
const ATTR_DIRECTORY: u16 = 0x0010;
const UTC_OFFSET_VALID: u8 = 0x80;
const MAX_DEPTH: usize = 64;
const UPCASE_CHARS: usize = 0x1_0000;

/// Fields of the main boot sector, plus the derived geometry.
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    pub cluster_size: u64,
    pub cluster_count: u64,
    /// Byte offsets of the FAT and of cluster 2.
    pub fat_offset: u64,
    pub heap_offset: u64,
    pub root_cluster: u32,
}

/// One decoded file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadNode {
    pub is_dir: bool,
    /// File contents (empty for directories).
    pub bytes: Vec<u8>,
    /// Last-modified time as (unix seconds, nanoseconds), at the exFAT 10 ms resolution.
    pub mtime: (i64, u32),
    /// True when the stream extension says the data is contiguous (NoFatChain).
    pub no_fat_chain: bool,
}

#[derive(Debug)]
pub struct Volume {
    pub info: BootInfo,
    /// Volume label from the root directory's 0x83 entry (empty if absent).
    pub label: String,
    /// Relative path ("dir/sub/file") to node. The root itself is not included.
    pub tree: BTreeMap<String, ReadNode>,
}

struct Child {
    name: String,
    is_dir: bool,
    first: u32,
    no_chain: bool,
    len: u64,
    mtime: (i64, u32),
}

struct Reader {
    file: File,
    info: BootInfo,
    fat: Vec<u32>,
    upcase: Vec<u16>,
    /// Cluster -> the object that owns it. Every allocated cluster must have exactly one owner.
    claimed: BTreeMap<u32, String>,
    /// Read what an interrupted `flashcp sync` leaves: skip orphan secondaries (in-use
    /// secondaries after a deleted 0x85 within its SecondaryCount) and, of two live sets with
    /// one case-folded name, see only the later (newer) one.
    crashed: bool,
}

fn err<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_at(file: &File, off: u64, len: u64) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, off).map_err(|e| format!("reading {len} bytes at offset {off}: {e}"))?;
    Ok(buf)
}

/// Boot checksum (spec "Boot Checksum"): rotate-right-by-one and add each byte, skipping the
/// VolumeFlags (106, 107) and PercentInUse (112) bytes of the main boot sector.
fn boot_checksum(data: &[u8]) -> u32 {
    data.iter().enumerate().fold(0u32, |c, (i, &b)| {
        if matches!(i, 106 | 107 | 112) { c } else { c.rotate_right(1).wrapping_add(b as u32) }
    })
}

/// Directory entry-set checksum (spec "EntrySetChecksum"), 16-bit, skipping bytes 2 and 3.
fn set_checksum(set: &[u8]) -> u16 {
    set.iter()
        .enumerate()
        .fold(0u16, |c, (i, &b)| if i == 2 || i == 3 { c } else { c.rotate_right(1).wrapping_add(b as u16) })
}

/// Up-case table checksum (spec "UpCaseTable checksum"), 32-bit, over every byte.
fn table_checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |c, &b| c.rotate_right(1).wrapping_add(b as u32))
}

/// File name hash (spec "NameHash"): over the up-cased UTF-16 name, low byte then high byte.
fn name_hash(units: &[u16], upcase: &[u16]) -> u16 {
    let mut h: u16 = 0;
    for &u in units {
        for b in upcase[u as usize].to_le_bytes() {
            h = h.rotate_right(1).wrapping_add(b as u16);
        }
    }
    h
}

/// Days since 1970-01-01 of a proleptic Gregorian date (civil calendar arithmetic).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Decodes a spec timestamp: 32-bit DOS-style field, 10 ms increment byte (0..=199), and UTC
/// offset byte (bit 7 valid, signed 7-bit count of quarter hours). Returns UTC (secs, nanos).
fn decode_ts(ts: u32, tens_ms: u8, offset: u8) -> Result<(i64, u32), String> {
    let sec2 = ts & 0x1F;
    let min = (ts >> 5) & 0x3F;
    let hour = (ts >> 11) & 0x1F;
    let day = (ts >> 16) & 0x1F;
    let month = (ts >> 21) & 0x0F;
    let year = 1980 + ((ts >> 25) & 0x7F) as i64;
    if sec2 > 29 || min > 59 || hour > 23 || !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return err(format!("invalid timestamp field 0x{ts:08X}"));
    }
    if tens_ms > 199 {
        return err(format!("10 ms increment {tens_ms} out of range"));
    }
    if offset & UTC_OFFSET_VALID == 0 {
        return err("UTC offset not marked valid");
    }
    let q = (offset & 0x7F) as i64;
    let q = if q >= 64 { q - 128 } else { q };
    let local = days_from_civil(year, month as i64, day as i64) * 86_400
        + hour as i64 * 3_600
        + min as i64 * 60
        + sec2 as i64 * 2
        + (tens_ms / 100) as i64;
    Ok((local - q * 900, (tens_ms % 100) as u32 * 10_000_000))
}

/// Decodes the compressed up-case table (spec "UpCaseTable"): a 0xFFFF word is followed by the
/// number of consecutive characters that map to themselves.
fn decode_upcase(raw: &[u8]) -> Result<Vec<u16>, String> {
    if !raw.len().is_multiple_of(2) {
        return err("up-case table has an odd byte length");
    }
    let (pairs, _) = raw.as_chunks::<2>();
    let words: Vec<u16> = pairs.iter().map(|&c| u16::from_le_bytes(c)).collect();
    let mut map: Vec<u16> = (0..UPCASE_CHARS as u32).map(|c| c as u16).collect();
    let mut ch = 0usize;
    let mut i = 0usize;
    while i < words.len() {
        // A 0xFFFF followed by a count is an identity run. A 0xFFFF in the last word is the
        // literal mapping of U+FFFF itself, which is how the table ends.
        if words[i] == 0xFFFF && i + 1 < words.len() {
            ch += words[i + 1] as usize;
            i += 2;
        } else {
            if ch >= UPCASE_CHARS {
                return err("up-case table maps more than 65536 characters");
            }
            map[ch] = words[i];
            ch += 1;
            i += 1;
        }
    }
    if ch != UPCASE_CHARS {
        return err(format!("up-case table covers {ch} characters, expected {UPCASE_CHARS}"));
    }
    Ok(map)
}

fn read_boot(file: &File, file_len: u64) -> Result<BootInfo, String> {
    let s0 = read_at(file, 0, 512)?;
    if &s0[3..11] != b"EXFAT   " {
        return err("sector 0 has no \"EXFAT   \" file system name");
    }
    if s0[0..3] != [0xEB, 0x76, 0x90] {
        return err("bad JumpBoot bytes");
    }
    if s0[11..64].iter().any(|&b| b != 0) {
        return err("MustBeZero field (bytes 11..64) is not zero");
    }
    let bps_shift = s0[108] as u32;
    if !(9..=12).contains(&bps_shift) {
        return err(format!("BytesPerSectorShift {bps_shift} out of range"));
    }
    let bps = 1u64 << bps_shift;
    let spc_shift = s0[109] as u32;
    if spc_shift > 25 - bps_shift {
        return err(format!("SectorsPerClusterShift {spc_shift} too large"));
    }
    if s0[110] != 1 {
        return err(format!("NumberOfFats is {}, expected 1", s0[110]));
    }
    if file_len < 24 * bps {
        return err("image is too small to hold the boot regions");
    }
    // Main boot region (sectors 0..=11) followed by the backup (12..=23).
    let region = read_at(file, 0, 24 * bps)?;
    let (main, backup) = region.split_at(12 * bps as usize);
    // VolumeFlags (106..108) and PercentInUse (112) are volatile: the spec leaves them out of
    // the boot checksum, and drivers update them in the main boot sector only.
    if (0..12 * bps as usize).any(|i| !matches!(i, 106 | 107 | 112) && main[i] != backup[i]) {
        return err("backup boot region (sectors 12..=23) differs from the main boot region");
    }
    for s in 0..9usize {
        let end = (s + 1) * bps as usize;
        if region[end - 2..end] != [0x55, 0xAA] {
            return err(format!("boot sector {s} lacks the 0x55AA signature"));
        }
    }
    let want = boot_checksum(&main[..11 * bps as usize]);
    for w in main[11 * bps as usize..].as_chunks::<4>().0 {
        let stored = u32::from_le_bytes(*w);
        if stored != want {
            return err(format!("boot checksum sector holds 0x{stored:08X}, computed 0x{want:08X}"));
        }
    }
    let vol_sectors = le64(main, 72);
    let fat_offset = le32(main, 80) as u64;
    let fat_length = le32(main, 84) as u64;
    let heap_offset = le32(main, 88) as u64;
    let cluster_count = le32(main, 92) as u64;
    let spc = 1u64 << spc_shift;
    if vol_sectors * bps > file_len {
        return err(format!("VolumeLength {vol_sectors} sectors is larger than the image"));
    }
    if fat_offset < 24 {
        return err(format!("FatOffset {fat_offset} overlaps the boot regions"));
    }
    if fat_length == 0 || fat_offset + fat_length > heap_offset {
        return err("FAT overlaps the cluster heap or is empty");
    }
    if cluster_count == 0 || heap_offset + cluster_count * spc > vol_sectors {
        return err("cluster heap extends past the end of the volume");
    }
    if fat_length * bps < (cluster_count + 2) * 4 {
        return err("FAT is too short for ClusterCount");
    }
    let root = le32(main, 96);
    if root < 2 || root as u64 >= cluster_count + 2 {
        return err(format!("root cluster {root} out of range"));
    }
    Ok(BootInfo {
        cluster_size: bps * spc,
        cluster_count,
        fat_offset: fat_offset * bps,
        heap_offset: heap_offset * bps,
        root_cluster: root,
    })
}

impl Reader {
    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && (c as u64) < self.info.cluster_count + 2
    }

    fn cluster_offset(&self, c: u32) -> u64 {
        self.info.heap_offset + (c as u64 - 2) * self.info.cluster_size
    }

    /// Clusters holding `len` bytes starting at `first`: contiguous when NoFatChain, otherwise
    /// the FAT chain, which must end exactly where the length does.
    fn chain_clusters(&self, first: u32, no_chain: bool, len: u64, owner: &str) -> Result<Vec<u32>, String> {
        let n = len.div_ceil(self.info.cluster_size);
        if n == 0 {
            return Ok(Vec::new());
        }
        if !self.valid_cluster(first) {
            return err(format!("{owner}: first cluster {first} out of range"));
        }
        if no_chain {
            if first as u64 + n > self.info.cluster_count + 2 {
                return err(format!("{owner}: contiguous run runs past the end of the heap"));
            }
            return Ok((first..first + n as u32).collect());
        }
        let mut v = Vec::with_capacity(n as usize);
        let mut c = first;
        loop {
            v.push(c);
            let next = self.fat[c as usize];
            if v.len() as u64 == n {
                if next != END_OF_CHAIN {
                    return err(format!("{owner}: FAT chain continues past DataLength {len}"));
                }
                return Ok(v);
            }
            if next == END_OF_CHAIN || !self.valid_cluster(next) {
                return err(format!("{owner}: FAT chain ends after {} of {n} clusters", v.len()));
            }
            c = next;
        }
    }

    /// Clusters of a FAT chain that runs to end-of-chain (used for the root directory, which has
    /// no stream extension and therefore no DataLength).
    fn eoc_clusters(&self, first: u32, owner: &str) -> Result<Vec<u32>, String> {
        if !self.valid_cluster(first) {
            return err(format!("{owner}: first cluster {first} out of range"));
        }
        let mut v = Vec::new();
        let mut c = first;
        loop {
            v.push(c);
            let next = self.fat[c as usize];
            if next == END_OF_CHAIN {
                return Ok(v);
            }
            if !self.valid_cluster(next) || v.len() as u64 > self.info.cluster_count {
                return err(format!("{owner}: broken FAT chain at cluster {c}"));
            }
            c = next;
        }
    }

    fn read_clusters(&self, clusters: &[u32], len: u64) -> Result<Vec<u8>, String> {
        let cs = self.info.cluster_size;
        let mut data = Vec::with_capacity(clusters.len() * cs as usize);
        for &c in clusters {
            data.extend(read_at(&self.file, self.cluster_offset(c), cs)?);
        }
        data.truncate(len as usize);
        Ok(data)
    }

    fn claim(&mut self, clusters: &[u32], owner: &str) -> Result<(), String> {
        for &c in clusters {
            if let Some(prev) = self.claimed.get(&c) {
                return err(format!("cluster {c} is used by both {prev} and {owner}"));
            }
            self.claimed.insert(c, owner.to_string());
        }
        Ok(())
    }

    /// Walks the entries of one directory. Root-only entries (0x81, 0x82, 0x83) are skipped
    /// here because read_volume has already consumed them.
    fn parse_entries(&self, bytes: &[u8], owner: &str, is_root: bool) -> Result<Vec<Child>, String> {
        let mut out = Vec::new();
        let n = bytes.len() / ENTRY;
        let mut i = 0usize;
        while i < n {
            let e = &bytes[i * ENTRY..(i + 1) * ENTRY];
            let t = e[0];
            if t == ET_END {
                break;
            }
            if t & IN_USE == 0 {
                i += 1;
                if self.crashed && t == ET_FILE & !IN_USE {
                    for _ in 0..e[1] {
                        if i >= n || bytes[i * ENTRY] & 0x40 == 0 {
                            break;
                        }
                        i += 1;
                    }
                }
                continue;
            }
            match t {
                ET_BITMAP | ET_UPCASE | ET_LABEL if is_root => i += 1,
                ET_FILE => {
                    let sc = e[1] as usize;
                    if sc < 2 || i + 1 + sc > n {
                        return err(format!("{owner}: file entry at index {i} has bad SecondaryCount {sc}"));
                    }
                    out.push(self.parse_set(&bytes[i * ENTRY..(i + 1 + sc) * ENTRY], owner)?);
                    i += 1 + sc;
                }
                other => return err(format!("{owner}: unexpected in-use entry type 0x{other:02X} at index {i}")),
            }
        }
        Ok(out)
    }

    /// Decodes one file/directory entry set: 0x85 file entry, 0xC0 stream extension, 0xC1 names.
    fn parse_set(&self, set: &[u8], owner: &str) -> Result<Child, String> {
        let f = &set[..ENTRY];
        let s = &set[ENTRY..2 * ENTRY];
        if set_checksum(set) != le16(set, 2) {
            return err(format!("{owner}: entry-set checksum mismatch"));
        }
        if s[0] != ET_STREAM {
            return err(format!("{owner}: entry after 0x85 is 0x{:02X}, expected 0xC0", s[0]));
        }
        if s[1] & STREAM_ALLOC_POSSIBLE == 0 {
            return err(format!("{owner}: AllocationPossible flag clear"));
        }
        let no_chain = s[1] & STREAM_NO_FAT_CHAIN != 0;
        let name_len = s[3] as usize;
        let name_entries = set.len() / ENTRY - 2;
        if name_len == 0 || name_entries != name_len.div_ceil(15) {
            return err(format!("{owner}: {name_entries} name entries for a {name_len}-character name"));
        }
        let mut units = Vec::with_capacity(name_entries * 15);
        for k in 0..name_entries {
            let ne = &set[(2 + k) * ENTRY..(3 + k) * ENTRY];
            if ne[0] != ET_NAME {
                return err(format!("{owner}: name entry {k} is 0x{:02X}, expected 0xC1", ne[0]));
            }
            for j in 0..15 {
                units.push(le16(ne, 2 + 2 * j));
            }
        }
        units.truncate(name_len);
        let name = String::from_utf16(&units).map_err(|e| format!("{owner}: name is not UTF-16: {e}"))?;
        if name_hash(&units, &self.upcase) != le16(s, 4) {
            return err(format!("{owner}: name hash mismatch for {name:?}"));
        }
        let first = le32(s, 20);
        let dl = le64(s, 24);
        let vdl = le64(s, 8);
        if vdl != dl {
            return err(format!("{name:?}: ValidDataLength {vdl} != DataLength {dl}"));
        }
        let is_dir = le16(f, 4) & ATTR_DIRECTORY != 0;
        if is_dir && (dl == 0 || !dl.is_multiple_of(self.info.cluster_size) || first == 0) {
            return err(format!("{name:?}: directory with bad DataLength {dl} or first cluster {first}"));
        }
        let mtime = decode_ts(le32(f, 12), f[21], f[23]).map_err(|e| format!("{name:?}: LastModified: {e}"))?;
        Ok(Child { name, is_dir, first, no_chain, len: dl, mtime })
    }

    fn walk(
        &mut self,
        prefix: &str,
        bytes: &[u8],
        is_root: bool,
        depth: usize,
        out: &mut BTreeMap<String, ReadNode>,
    ) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return err("directory nesting too deep (cycle?)");
        }
        let owner = if is_root { "/" } else { prefix };
        let mut seen_names = std::collections::HashSet::new();
        let mut children = self.parse_entries(bytes, owner, is_root)?;
        if self.crashed {
            let mut last = std::collections::HashMap::new();
            for (k, c) in children.iter().enumerate() {
                let folded: Vec<u16> = c.name.encode_utf16().map(|u| self.upcase[u as usize]).collect();
                last.insert(folded, k);
            }
            let keep: std::collections::HashSet<usize> = last.into_values().collect();
            children = children.into_iter().enumerate().filter(|(k, _)| keep.contains(k)).map(|e| e.1).collect();
        }
        for child in children {
            let path = if prefix.is_empty() { child.name.clone() } else { format!("{prefix}/{}", child.name) };
            let folded: Vec<u16> = child.name.encode_utf16().map(|u| self.upcase[u as usize]).collect();
            if !seen_names.insert(folded) {
                return err(format!("{owner}: two names collide case-insensitively: {:?}", child.name));
            }
            if out.contains_key(&path) {
                return err(format!("duplicate path {path}"));
            }
            let clusters = self.chain_clusters(child.first, child.no_chain, child.len, &path)?;
            self.claim(&clusters, &path)?;
            let data = self.read_clusters(&clusters, child.len)?;
            let node = ReadNode {
                is_dir: child.is_dir,
                bytes: if child.is_dir { Vec::new() } else { data.clone() },
                mtime: child.mtime,
                no_fat_chain: child.no_chain,
            };
            out.insert(path.clone(), node);
            if child.is_dir {
                self.walk(&path, &data, false, depth + 1, out)?;
            }
        }
        Ok(())
    }
}

fn read_fat(file: &File, info: &BootInfo) -> Result<Vec<u32>, String> {
    let n = info.cluster_count + 2;
    let raw = read_at(file, info.fat_offset, n * 4)?;
    let fat: Vec<u32> = raw.as_chunks::<4>().0.iter().map(|&c| u32::from_le_bytes(c)).collect();
    if fat[0] != MEDIA_DESCRIPTOR_ENTRY {
        return err(format!("FAT[0] is 0x{:08X}, expected 0xFFFFFFF8", fat[0]));
    }
    if fat[1] != END_OF_CHAIN {
        return err(format!("FAT[1] is 0x{:08X}, expected 0xFFFFFFFF", fat[1]));
    }
    Ok(fat)
}

/// Parses an exFAT volume image (or device) read-only and returns its tree.
///
/// Checks the boot region (signature, MustBeZero, checksum, backup copy), FAT[0..2], every
/// directory entry set (checksum, stream extension, name hash), timestamps, the up-case table
/// checksum, and that every allocated cluster is owned by exactly one object and marked in the
/// allocation bitmap, and the reverse.
pub fn read_volume(path: &Path) -> Result<Volume, String> {
    read_volume_opts(path, false)
}

/// Like [`read_volume`], but tolerates clusters that are marked allocated and referenced by
/// nothing (what an interrupted writer leaks). Everything reachable is still fully validated,
/// including that every used cluster is marked allocated and owned exactly once.
#[allow(dead_code)] // used by tests/add.rs, not by every test binary
pub fn read_volume_allow_leaks(path: &Path) -> Result<Volume, String> {
    read_volume_opts(path, true)
}

/// Like [`read_volume_allow_leaks`], for the states an interrupted `flashcp sync` leaves:
/// orphan secondaries are skipped, and of duplicate names only the later set is seen.
#[allow(dead_code)] // used by tests/sync.rs, not by every test binary
pub fn read_volume_crashed(path: &Path) -> Result<Volume, String> {
    read_volume_inner(path, true, true)
}

fn read_volume_opts(path: &Path, allow_leaks: bool) -> Result<Volume, String> {
    read_volume_inner(path, allow_leaks, false)
}

fn read_volume_inner(path: &Path, allow_leaks: bool, crashed: bool) -> Result<Volume, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    let info = read_boot(&file, file_len)?;
    let fat = read_fat(&file, &info)?;
    let mut r = Reader { file, info, fat, upcase: Vec::new(), claimed: BTreeMap::new(), crashed };
    let root_clusters = r.eoc_clusters(info.root_cluster, "root directory")?;
    r.claim(&root_clusters, "root directory")?;
    let root = r.read_clusters(&root_clusters, root_clusters.len() as u64 * info.cluster_size)?;

    // Root-only entries: label, allocation bitmap, up-case table.
    let (mut bitmap_e, mut upcase_e, mut label) = (None, None, String::new());
    for e in root.as_chunks::<ENTRY>().0 {
        match e[0] {
            ET_END => break,
            ET_BITMAP => {
                if bitmap_e.replace(e.to_vec()).is_some() {
                    return err("root directory has two allocation bitmap entries");
                }
            }
            ET_UPCASE => {
                if upcase_e.replace(e.to_vec()).is_some() {
                    return err("root directory has two up-case table entries");
                }
            }
            ET_LABEL => {
                let n = e[1] as usize;
                if n > 11 {
                    return err(format!("volume label length {n} exceeds 11"));
                }
                let units: Vec<u16> = (0..n).map(|j| le16(e, 2 + 2 * j)).collect();
                label = String::from_utf16(&units).map_err(|e| format!("label is not UTF-16: {e}"))?;
            }
            _ => {}
        }
    }

    let bm = bitmap_e.ok_or("root directory has no allocation bitmap entry (0x81)")?;
    if bm[1] & 1 != 0 {
        return err("second allocation bitmap (TexFAT) is not supported");
    }
    let bm_len = le64(&bm, 24);
    if bm_len != info.cluster_count.div_ceil(8) {
        return err(format!("bitmap DataLength {bm_len} != ceil(ClusterCount / 8)"));
    }
    let bm_clusters = r.chain_clusters(le32(&bm, 20), false, bm_len, "allocation bitmap")?;
    r.claim(&bm_clusters, "allocation bitmap")?;
    let bitmap = r.read_clusters(&bm_clusters, bm_len)?;

    let uc = upcase_e.ok_or("root directory has no up-case table entry (0x82)")?;
    let uc_len = le64(&uc, 24);
    let uc_clusters = r.chain_clusters(le32(&uc, 20), false, uc_len, "up-case table")?;
    r.claim(&uc_clusters, "up-case table")?;
    let raw = r.read_clusters(&uc_clusters, uc_len)?;
    if table_checksum(&raw) != le32(&uc, 4) {
        return err(format!("up-case table checksum 0x{:08X} != entry's 0x{:08X}", table_checksum(&raw), le32(&uc, 4)));
    }
    r.upcase = decode_upcase(&raw)?;

    let mut tree = BTreeMap::new();
    r.walk("", &root, true, 0, &mut tree)?;

    // Bitmap and ownership must agree in both directions.
    let bit = |c: u32| {
        let i = (c - 2) as usize;
        (bitmap[i / 8] >> (i % 8)) & 1 == 1
    };
    for (&c, owner) in &r.claimed {
        if !bit(c) {
            return err(format!("cluster {c} (used by {owner}) is not marked allocated in the bitmap"));
        }
    }
    for c in 2..(info.cluster_count as u32 + 2) {
        if !allow_leaks && bit(c) && !r.claimed.contains_key(&c) {
            return err(format!("cluster {c} is marked allocated but no file or directory uses it"));
        }
    }
    Ok(Volume { info, label, tree })
}
