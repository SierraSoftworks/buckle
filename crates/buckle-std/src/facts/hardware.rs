//! The `hardware` fact: architecture, CPUs, memory, swap and disks.

use super::{opt_str, probe, read_text};
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Describes the machine's resources.
///
/// Keys: `arch` (`uname -m`: `x86_64`, `aarch64`), `cpus`, `mem.total_mb`,
/// `swap.total_mb` and `disks`, a list of `{mount, fstype, total_mb, free_mb,
/// free_percent}` for the real filesystems from `df -P -k -T` (tmpfs and other
/// pseudo filesystems are left out). Selectors cannot index into lists, so
/// every disk is also exposed as `disk.<name>.{mount, fstype, total_mb,
/// free_mb, free_percent}` where `<name>` is `root` for `/` and the mount path
/// with separators turned into `_` otherwise (`/var/lib/data` is
/// `var_lib_data`): `host.facts.hardware.disk.root.free_percent < 10`.
/// `free_percent` is available space over (used + available), as `df` reports.
/// Refreshed every five minutes.
pub struct HardwareFacts;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Disk {
    pub mount: String,
    pub fstype: Option<String>,
    pub total_mb: u64,
    pub free_mb: u64,
    pub free_percent: f64,
}

impl Disk {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "mount": self.mount,
            "fstype": opt_str(self.fstype.clone()),
            "total_mb": self.total_mb,
            "free_mb": self.free_mb,
            "free_percent": self.free_percent,
        })
    }

    /// The key under `disk.`: `root`, `boot_efi`, `var_lib_data`.
    pub(crate) fn name(&self) -> String {
        let trimmed = self.mount.trim_matches('/');
        if trimmed.is_empty() {
            return "root".into();
        }
        trimmed
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }
}

const PSEUDO_FS: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "squashfs",
    "ramfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "efivarfs",
    "devpts",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "bpf",
];

/// Parses `df -P -k` (optionally with `-T`) output.
pub(crate) fn parse_df(text: &str) -> Vec<Disk> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return vec![];
    };
    let has_type = header
        .split_whitespace()
        .any(|h| h.eq_ignore_ascii_case("Type"));
    let mut disks = Vec::new();
    for line in lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let skip = if has_type { 2 } else { 1 };
        // Filesystem [Type] Blocks Used Available Capacity Mounted-on...
        if parts.len() < skip + 5 {
            continue;
        }
        let fs = parts[0];
        let fstype = has_type.then(|| parts[1].to_owned());
        let nums = &parts[skip..skip + 3];
        let (Ok(blocks), Ok(used), Ok(avail)) = (
            nums[0].parse::<u64>(),
            nums[1].parse::<u64>(),
            nums[2].parse::<u64>(),
        ) else {
            continue;
        };
        let mount = parts[skip + 4..].join(" ");
        let pseudo = match &fstype {
            Some(t) => PSEUDO_FS.contains(&t.as_str()),
            None => {
                matches!(fs, "tmpfs" | "devtmpfs" | "udev" | "none" | "overlay") && mount != "/"
            }
        };
        if pseudo || blocks == 0 {
            continue;
        }
        let capacity = used + avail;
        let free_percent = if capacity == 0 {
            0.0
        } else {
            (avail as f64 / capacity as f64 * 1000.0).round() / 10.0
        };
        disks.push(Disk {
            mount,
            fstype,
            total_mb: blocks / 1024,
            free_mb: avail / 1024,
            free_percent,
        });
    }
    disks
}

/// `(MemTotal, SwapTotal)` in kB from `/proc/meminfo`.
pub(crate) fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    (field("MemTotal"), field("SwapTotal"))
}

/// The number of `processor` entries in `/proc/cpuinfo`.
pub(crate) fn count_cpus(cpuinfo: &str) -> usize {
    cpuinfo
        .lines()
        .filter(|l| l.split(':').next().is_some_and(|k| k.trim() == "processor"))
        .count()
}

#[async_trait]
impl FactProvider for HardwareFacts {
    const NAME: &'static str = "hardware";
    const TTL: Duration = Duration::from_secs(300);
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str = "Architecture, CPU count, memory, swap and disks (with flat disk.<name> keys for selectors).";

    #[tracing::instrument(name = "facts.hardware", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        let arch = probe(cx, "uname", &["-m"])
            .await
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| std::env::consts::ARCH.to_owned());

        let cpus = match read_text(cx, "/proc/cpuinfo").await.map(|t| count_cpus(&t)) {
            Some(n) if n > 0 => Some(n as u64),
            _ => probe(cx, "nproc", &[])
                .await
                .and_then(|o| o.trim().parse::<u64>().ok()),
        };

        let (mem, swap) = read_text(cx, "/proc/meminfo")
            .await
            .map(|t| parse_meminfo(&t))
            .unwrap_or((None, None));
        let mb = |kb: Option<u64>| kb.map(|kb| Value::from(kb / 1024)).unwrap_or(Value::Null);

        let df = match probe(cx, "df", &["-P", "-k", "-T"]).await {
            Some(out) => Some(out),
            None => probe(cx, "df", &["-P", "-k"]).await,
        };
        let disks = df.as_deref().map(parse_df).unwrap_or_default();
        let mut flat = serde_json::Map::new();
        for disk in &disks {
            flat.entry(disk.name()).or_insert_with(|| disk.to_json());
        }

        Ok(json!({
            "arch": arch,
            "cpus": cpus,
            "mem": {"total_mb": mb(mem)},
            "swap": {"total_mb": mb(swap)},
            "disks": disks.iter().map(Disk::to_json).collect::<Vec<_>>(),
            "disk": flat,
        }))
    }
}

buckle_core::register_facts!(HardwareFacts);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;

    pub const MEMINFO: &str = "\
MemTotal:        4025648 kB
MemFree:          812316 kB
MemAvailable:    3217044 kB
Buffers:          143212 kB
Cached:          2108312 kB
SwapCached:            0 kB
SwapTotal:       1048572 kB
SwapFree:        1048572 kB
";

    pub const CPUINFO_4: &str = "\
processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) Platinum 8272CL CPU @ 2.60GHz

processor\t: 1
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) Platinum 8272CL CPU @ 2.60GHz

processor\t: 2
vendor_id\t: GenuineIntel

processor\t: 3
vendor_id\t: GenuineIntel
";

    const CPUINFO_ARM: &str = "\
processor\t: 0
BogoMIPS\t: 50.00
Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp ssbs
CPU implementer\t: 0x41

processor\t: 1
BogoMIPS\t: 50.00
";

    pub const DF_T: &str = "\
Filesystem     Type     1024-blocks     Used Available Capacity Mounted on
udev           devtmpfs     1992124        0   1992124       0% /dev
tmpfs          tmpfs         402564      708    401856       1% /run
/dev/sda1      ext4        30298176 10523212  18467500      37% /
tmpfs          tmpfs        2012816        0   2012816       0% /dev/shm
tmpfs          tmpfs           5120        0      5120       0% /run/lock
/dev/sda15     vfat          126678     5920    120758       5% /boot/efi
/dev/sdb1      xfs        103081248 61440000  41641248      60% /var/lib/data
";

    const DF_PLAIN: &str = "\
Filesystem     1024-blocks     Used Available Capacity Mounted on
overlay           61255492 23156044  38082064      38% /
tmpfs                65536        0     65536       0% /dev
/dev/vda1         61255492 23156044  38082064      38% /etc/hosts
shm                  65536        0     65536       0% /dev/shm
";

    #[test]
    fn df_with_types_lists_real_filesystems_only() {
        let disks = parse_df(DF_T);
        assert_eq!(
            disks.iter().map(|d| d.mount.as_str()).collect::<Vec<_>>(),
            vec!["/", "/boot/efi", "/var/lib/data"]
        );
        let root = &disks[0];
        assert_eq!(root.fstype.as_deref(), Some("ext4"));
        assert_eq!(root.total_mb, 29588);
        assert_eq!(root.free_mb, 18034);
        assert_eq!(root.free_percent, 63.7);
        assert_eq!(disks[2].name(), "var_lib_data");
        assert_eq!(disks[1].name(), "boot_efi");
        assert_eq!(disks[0].name(), "root");
    }

    #[test]
    fn df_without_types_keeps_an_overlay_root() {
        let disks = parse_df(DF_PLAIN);
        assert_eq!(disks[0].mount, "/");
        assert_eq!(disks[0].fstype, None);
        assert!(disks.iter().all(|d| d.mount != "/dev"));
    }

    #[test]
    fn df_garbage_is_ignored() {
        assert!(parse_df("").is_empty());
        assert!(parse_df("df: command not found\n").is_empty());
        assert!(
            parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\nbad line\n")
                .is_empty()
        );
    }

    #[test]
    fn meminfo_and_cpuinfo_parse() {
        assert_eq!(parse_meminfo(MEMINFO), (Some(4025648), Some(1048572)));
        assert_eq!(parse_meminfo("garbage"), (None, None));
        assert_eq!(count_cpus(CPUINFO_4), 4);
        assert_eq!(count_cpus(CPUINFO_ARM), 2);
        assert_eq!(count_cpus(""), 0);
    }

    #[tokio::test]
    async fn collects_the_documented_shape() {
        let sys = FakeSystem::new()
            .with_file("/proc/meminfo", MEMINFO, FilePerms::default())
            .with_file("/proc/cpuinfo", CPUINFO_4, FilePerms::default())
            .on_command_ok("uname", &["-m"], "x86_64\n")
            .on_command_ok("df", &["-P", "-k", "-T"], DF_T);
        let f = HardwareFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["arch"], "x86_64");
        assert_eq!(f["cpus"], 4);
        assert_eq!(f["mem"]["total_mb"], 3931);
        assert_eq!(f["swap"]["total_mb"], 1023);
        assert_eq!(f["disks"].as_array().unwrap().len(), 3);
        assert_eq!(f["disk"]["root"]["free_percent"], 63.7);
        assert_eq!(f["disk"]["var_lib_data"]["fstype"], "xfs");
    }

    #[tokio::test]
    async fn falls_back_when_tools_and_files_are_missing() {
        // No /proc, no df -T (BusyBox), nproc available.
        let sys = FakeSystem::new()
            .on_command_ok("nproc", &[], "8\n")
            .on_command_fail("df", &["-P", "-k", "-T"], 1, "df: unrecognized option: T")
            .on_command_ok("df", &["-P", "-k"], DF_PLAIN);
        let f = HardwareFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["arch"], std::env::consts::ARCH);
        assert_eq!(f["cpus"], 8);
        assert!(f["mem"]["total_mb"].is_null());
        assert_eq!(f["disk"]["root"]["total_mb"], 59819);

        let bare = HardwareFacts
            .collect(&cx(&FakeSystem::new()))
            .await
            .unwrap();
        assert!(bare["cpus"].is_null());
        assert_eq!(bare["disks"], json!([]));
    }
}
