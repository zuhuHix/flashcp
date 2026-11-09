//! udisks2 over the system bus (blocking zbus). Lists USB drives, unmounts, rescans, mounts,
//! and watches for devices appearing or disappearing.

use crate::drives::{select_drives, Drive, RawBlock, RawDrive};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SERVICE: &str = "org.freedesktop.UDisks2";
const ROOT: &str = "/org/freedesktop/UDisks2";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const PARTITION: &str = "org.freedesktop.UDisks2.Partition";
const PARTITION_TABLE: &str = "org.freedesktop.UDisks2.PartitionTable";
const FAKE_ENV: &str = "FLASHCP_GUI_FAKE_DRIVES";
const MOUNT_TRIES: u32 = 10;
const MOUNT_RETRY_DELAY: Duration = Duration::from_millis(300);

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// True when the dev switch FLASHCP_GUI_FAKE_DRIVES=1 is set. No udisks calls then.
pub fn fake_mode() -> bool {
    std::env::var(FAKE_ENV).is_ok_and(|v| v == "1")
}

/// A change on the bus, carrying the udisks object path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Removed(String),
}

fn connection() -> Result<Connection, String> {
    Connection::system().map_err(|e| format!("system bus unavailable: {e}"))
}

fn bytes_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn prop<'a>(props: &'a HashMap<String, OwnedValue>, key: &str) -> Option<&'a OwnedValue> {
    props.get(key)
}

fn prop_string(props: &HashMap<String, OwnedValue>, key: &str) -> String {
    prop(props, key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    prop(props, key).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false)
}

fn prop_u64(props: &HashMap<String, OwnedValue>, key: &str) -> u64 {
    prop(props, key).and_then(|v| u64::try_from(v.clone()).ok()).unwrap_or(0)
}

fn prop_path(props: &HashMap<String, OwnedValue>, key: &str) -> String {
    prop(props, key).and_then(|v| OwnedObjectPath::try_from(v.clone()).ok()).map(|p| p.to_string()).unwrap_or_default()
}

fn prop_bytes_list(props: &HashMap<String, OwnedValue>, key: &str) -> Vec<Vec<u8>> {
    prop(props, key).and_then(|v| Vec::<Vec<u8>>::try_from(v.clone()).ok()).unwrap_or_default()
}

fn raw_block(path: &str, ifaces: &HashMap<String, HashMap<String, OwnedValue>>) -> Option<RawBlock> {
    let block = ifaces.get(BLOCK)?;
    let filesystem = ifaces.get(FILESYSTEM);
    let device = prop(block, "Device")
        .and_then(|v| Vec::<u8>::try_from(v.clone()).ok())
        .map(|b| bytes_to_string(&b))
        .unwrap_or_default();
    let mount_points = filesystem
        .map(|fs| prop_bytes_list(fs, "MountPoints").iter().map(|b| bytes_to_string(b)).collect())
        .unwrap_or_default();
    Some(RawBlock {
        object_path: path.to_string(),
        device,
        id_type: prop_string(block, "IdType"),
        id_label: prop_string(block, "IdLabel"),
        size: prop_u64(block, "Size"),
        hint_system: prop_bool(block, "HintSystem"),
        hint_ignore: prop_bool(block, "HintIgnore"),
        drive_path: prop_path(block, "Drive"),
        mount_points,
        is_partition: ifaces.contains_key(PARTITION),
        is_partition_table: ifaces.contains_key(PARTITION_TABLE),
        has_filesystem: filesystem.is_some(),
    })
}

fn raw_drive(ifaces: &HashMap<String, HashMap<String, OwnedValue>>) -> Option<RawDrive> {
    let drive = ifaces.get(DRIVE)?;
    Some(RawDrive {
        connection_bus: prop_string(drive, "ConnectionBus"),
        removable: prop_bool(drive, "Removable"),
        media_removable: prop_bool(drive, "MediaRemovable"),
        vendor: prop_string(drive, "Vendor"),
        model: prop_string(drive, "Model"),
    })
}

/// Blocks and drives from one GetManagedObjects call.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub blocks: Vec<RawBlock>,
    pub drives: HashMap<String, RawDrive>,
}

pub fn snapshot() -> Result<Snapshot, String> {
    let conn = connection()?;
    let om = Proxy::new(&conn, SERVICE, ROOT, OBJECT_MANAGER).map_err(|e| format!("udisks ObjectManager: {e}"))?;
    let managed: Managed = om.call("GetManagedObjects", &()).map_err(|e| format!("GetManagedObjects failed: {e}"))?;

    let mut snap = Snapshot::default();
    for (path, ifaces) in managed {
        let path = path.to_string();
        if let Some(block) = raw_block(&path, &ifaces) {
            snap.blocks.push(block);
        }
        if let Some(drive) = raw_drive(&ifaces) {
            snap.drives.insert(path, drive);
        }
    }
    Ok(snap)
}

/// Current USB drives. Fake drives in dev mode; empty (with the error logged) when udisks
/// is unavailable.
pub fn list_drives() -> Vec<Drive> {
    if fake_mode() {
        return crate::drives::fake_drives();
    }
    match snapshot() {
        Ok(snap) => select_drives(&snap.blocks, &snap.drives),
        Err(e) => {
            eprintln!("flashcp-gui: cannot list drives: {e}");
            Vec::new()
        }
    }
}

fn empty_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

pub fn unmount(object_path: &str) -> Result<(), String> {
    let conn = connection()?;
    let fs = Proxy::new(&conn, SERVICE, object_path, FILESYSTEM).map_err(|e| format!("{object_path}: {e}"))?;
    fs.call_method("Unmount", &empty_options()).map(|_| ()).map_err(|e| format!("Unmount failed: {e}"))
}

pub fn rescan(object_path: &str) -> Result<(), String> {
    let conn = connection()?;
    let block = Proxy::new(&conn, SERVICE, object_path, BLOCK).map_err(|e| format!("{object_path}: {e}"))?;
    block.call_method("Rescan", &empty_options()).map(|_| ()).map_err(|e| format!("Rescan failed: {e}"))
}

/// Mounts the filesystem and returns its mount point. The Filesystem interface can vanish
/// for a moment after a write, so this retries; if it is already mounted, that point is used.
pub fn mount(object_path: &str) -> Result<String, String> {
    let conn = connection()?;
    let fs = Proxy::new(&conn, SERVICE, object_path, FILESYSTEM).map_err(|e| format!("{object_path}: {e}"))?;
    let mut last_error = String::new();
    for attempt in 0..MOUNT_TRIES {
        if attempt > 0 {
            thread::sleep(MOUNT_RETRY_DELAY);
        }
        match fs.call::<_, _, String>("Mount", &empty_options()) {
            Ok(mount_point) => return Ok(mount_point),
            Err(e) => last_error = e.to_string(),
        }
        if let Ok(points) = fs.get_property::<Vec<Vec<u8>>>("MountPoints") {
            if let Some(first) = points.iter().map(|b| bytes_to_string(b)).find(|p| !p.is_empty()) {
                return Ok(first);
            }
        }
    }
    Err(format!("Mount failed after {MOUNT_TRIES} tries: {last_error}"))
}

/// Starts two background threads (InterfacesAdded and InterfacesRemoved) that call
/// `on_change`. Each re-subscribes after a pause if the bus connection drops.
pub fn watch(on_change: Arc<dyn Fn(Change) + Send + Sync>) {
    for signal in ["InterfacesAdded", "InterfacesRemoved"] {
        let cb = Arc::clone(&on_change);
        let spawned = thread::Builder::new().name(format!("udisks-{signal}")).spawn(move || loop {
            if let Err(e) = listen(signal, &cb) {
                eprintln!("flashcp-gui: udisks watch ({signal}) stopped: {e}");
            }
            thread::sleep(Duration::from_secs(10));
        });
        if let Err(e) = spawned {
            eprintln!("flashcp-gui: cannot start udisks watcher: {e}");
        }
    }
}

fn listen(signal: &str, cb: &Arc<dyn Fn(Change) + Send + Sync>) -> Result<(), String> {
    let conn = connection()?;
    let om = Proxy::new(&conn, SERVICE, ROOT, OBJECT_MANAGER).map_err(|e| format!("udisks ObjectManager: {e}"))?;
    let messages = om.receive_signal(signal).map_err(|e| format!("subscribe to {signal}: {e}"))?;
    for msg in messages {
        if signal == "InterfacesAdded" {
            if let Ok((path, _)) =
                msg.body().deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>)>()
            {
                cb(Change::Added(path.to_string()));
            }
        } else if let Ok((path, _)) = msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>() {
            cb(Change::Removed(path.to_string()));
        }
    }
    Err("signal stream ended".to_string())
}
