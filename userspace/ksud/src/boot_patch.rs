#![allow(clippy::ref_option, clippy::needless_pass_by_value)]

use std::fs::File;
use std::io::{Cursor, Seek, SeekFrom};
use std::path::Path;
use std::path::PathBuf;

use android_bootimg::cpio::{Cpio, CpioEntry};
use android_bootimg::parser::{BootImage, BootImageVersion, RamdiskImage};
use android_bootimg::patcher::BootImagePatchOption;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use memmap2::{Mmap, MmapOptions};
use regex_lite::Regex;

use crate::assets;

// This branch targets the vivo-only patch flow, so boot-family names stay centralized.
const BOOT_PARTITION_BOOT: &str = "boot";
const BOOT_PARTITION_INIT_BOOT: &str = "init_boot";
const BOOT_PARTITION_VENDOR_BOOT: &str = "vendor_boot";
const BOOT_FAMILY_PARTITIONS: [&str; 3] = [
    BOOT_PARTITION_BOOT,
    BOOT_PARTITION_INIT_BOOT,
    BOOT_PARTITION_VENDOR_BOOT,
];

#[cfg(target_os = "android")]
mod android {
    use super::Result;
    pub(super) use crate::defs::{BACKUP_FILENAME, KSU_BACKUP_DIR, KSU_BACKUP_FILE_PREFIX};
    use crate::defs::{DEFAULT_PACKAGE_NAME, KSU_TEMP_BACKUP_DIR_NAME};
    use android_bootimg::cpio::{Cpio, CpioEntry};
    use anyhow::{Context, anyhow, bail, ensure};
    use regex_lite::Regex;
    use rustix::process::getuid;
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{
        BOOT_FAMILY_PARTITIONS, BOOT_PARTITION_BOOT, BOOT_PARTITION_INIT_BOOT,
        BOOT_PARTITION_VENDOR_BOOT,
    };
    use crate::utils;

    pub(super) fn ensure_gki_kernel() -> Result<()> {
        let version = get_kernel_version()?;
        let is_gki = version.0 == 5 && version.1 >= 10 || version.2 > 5;
        ensure!(is_gki, "only support GKI kernel");
        Ok(())
    }

    pub fn get_kernel_version() -> Result<(i32, i32, i32)> {
        let uname = rustix::system::uname();
        let version = uname.release().to_string_lossy();
        let re = Regex::new(r"(\d+)\.(\d+)\.(\d+)")?;
        if let Some(captures) = re.captures(&version) {
            let major = captures
                .get(1)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .ok_or_else(|| anyhow!("Major version parse error"))?;
            let minor = captures
                .get(2)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .ok_or_else(|| anyhow!("Minor version parse error"))?;
            let patch = captures
                .get(3)
                .and_then(|m| m.as_str().parse::<i32>().ok())
                .ok_or_else(|| anyhow!("Patch version parse error"))?;
            Ok((major, minor, patch))
        } else {
            Err(anyhow!("Invalid kernel version string"))
        }
    }

    fn parse_kmi(version: &str) -> Result<String> {
        let re = Regex::new(r"(.* )?(\d+\.\d+)(\S+)?(android\d+)(.*)")?;
        let cap = re
            .captures(version)
            .ok_or_else(|| anyhow::anyhow!("Failed to get KMI from boot/modules"))?;
        let android_version = cap.get(4).map_or("", |m| m.as_str());
        let kernel_version = cap.get(2).map_or("", |m| m.as_str());
        Ok(format!("{android_version}-{kernel_version}"))
    }

    const VIVO_KMI_SUFFIX: &str = "_vivo";

    fn to_vivo_kmi_name(kmi: String) -> String {
        if kmi.ends_with(VIVO_KMI_SUFFIX) {
            kmi
        } else {
            format!("{kmi}{VIVO_KMI_SUFFIX}")
        }
    }

    fn parse_kmi_from_uname() -> Result<String> {
        let uname = rustix::system::uname();
        let version = uname.release().to_string_lossy();
        parse_kmi(&version)
    }

    fn parse_kmi_from_modules() -> Result<String> {
        use std::io::BufRead;
        // find a *.ko in /vendor/lib/modules
        let modfile = std::fs::read_dir("/vendor/lib/modules")?
            .filter_map(Result::ok)
            .find(|entry| entry.path().extension().is_some_and(|ext| ext == "ko"))
            .map(|entry| entry.path())
            .ok_or_else(|| anyhow!("No kernel module found"))?;
        let output = Command::new("modinfo").arg(modfile).output()?;
        for line in output.stdout.lines().map_while(Result::ok) {
            if line.starts_with("vermagic") {
                return parse_kmi(&line);
            }
        }
        bail!("Parse KMI from modules failed")
    }

    fn detect_current_base_kmi() -> Result<String> {
        parse_kmi_from_uname().or_else(|_| parse_kmi_from_modules())
    }

    fn detect_current_vivo_kmi() -> Result<String> {
        detect_current_base_kmi().map(to_vivo_kmi_name)
    }

    pub fn get_current_kmi() -> Result<String> {
        // This vivo branch only packages vivo-suffixed LKMs, so the detected KMI
        // stays mapped to the corresponding asset name.
        detect_current_vivo_kmi()
    }

    fn calculate_sha1(file_path: impl AsRef<Path>) -> Result<String> {
        use sha1::Digest;
        use std::io::Read;
        let mut file = std::fs::File::open(file_path.as_ref())?;
        let mut hasher = sha1::Sha1::new();
        let mut buffer = [0; 1024];

        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        let result = hasher.finalize();
        Ok(base16ct::lower::encode_string(&result))
    }

    fn find_backup_location(sha1: &String) -> Result<(File, String)> {
        let filename = format!("{KSU_BACKUP_FILE_PREFIX}{sha1}");
        let target = format!("{KSU_BACKUP_DIR}{filename}");
        if let Ok(target_file) = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&target)
        {
            return Ok((target_file, target));
        }

        // We have no permission to access /data/adb
        // Save it to /data/user_de/$USER/$PKG/boot_backup
        let user_id = getuid().as_raw() / 100_000;

        let backup_dir =
            format!("/data/user_de/{user_id}/{DEFAULT_PACKAGE_NAME}/{KSU_TEMP_BACKUP_DIR_NAME}");
        std::fs::remove_dir_all(&backup_dir).ok();
        std::fs::create_dir(&backup_dir)?;
        let backup_file = format!("{backup_dir}/{filename}");
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&backup_file)
        {
            return Ok((file, backup_file));
        }

        bail!("Both /data/adb/ksu and {backup_dir} are not accessible!")
    }

    pub(super) fn do_backup(cpio: &mut Cpio, image: &Path) -> Result<()> {
        let sha1 = calculate_sha1(image)?;
        let (mut target_file, target) = find_backup_location(&sha1)?;
        println!("- Backup stock boot image");
        let mut source = OpenOptions::new()
            .create(false)
            .truncate(false)
            .read(true)
            .write(false)
            .open(image)?;

        // Use io::copy instead of fs::copy to allow copy block device
        std::io::copy(&mut source, &mut target_file)
            .with_context(|| format!("failed to backup to {target}"))?;

        let backup_file = CpioEntry::regular(0o755, Box::new(sha1));
        cpio.add(BACKUP_FILENAME, backup_file)?;
        println!("- Stock image has been backup to");
        println!("- {target}");
        Ok(())
    }

    pub(super) fn clean_backup(sha1: &str) -> Result<()> {
        println!("- Clean up backup");
        let backup_name = format!("{KSU_BACKUP_FILE_PREFIX}{sha1}");
        let dir = std::fs::read_dir(KSU_BACKUP_DIR)?;
        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy().to_string();
                if name != backup_name
                    && name.starts_with(KSU_BACKUP_FILE_PREFIX)
                    && std::fs::remove_file(path).is_ok()
                {
                    println!("- removed {name}");
                }
            }
        }
        Ok(())
    }

    pub(super) fn flash_partition(partition: &str, data: &[u8]) -> Result<()> {
        let mut blk = std::fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .create(false)
            .open(partition)
            .with_context(|| format!("open {partition}"))?;
        unsafe {
            const BLKROSET: i32 = libc::_IO(0x12, 93);
            let mut val: libc::c_int = 0;
            if libc::ioctl(blk.as_raw_fd(), BLKROSET, &raw mut val) != 0 {
                bail!("Failed to set rw for {partition}: {}", *libc::__errno());
            }
        }
        blk.write_all(data).context("flash boot failed")?;
        blk.sync_all().context("sync boot failed")?;
        Ok(())
    }

    pub fn choose_boot_partition(
        kmi: &str,
        is_replace_kernel: bool,
        partition: &Option<String>,
    ) -> String {
        let slot_suffix = get_slot_suffix(false);
        let skip_init_boot = kmi.starts_with("android12-");
        let init_boot_exist = Path::new(&format!(
            "/dev/block/by-name/{BOOT_PARTITION_INIT_BOOT}{slot_suffix}"
        ))
        .exists();

        // if specific partition is specified, use it
        if let Some(part) = partition {
            return match part.as_str() {
                BOOT_PARTITION_BOOT | BOOT_PARTITION_INIT_BOOT | BOOT_PARTITION_VENDOR_BOOT => {
                    part.clone()
                }
                _ => BOOT_PARTITION_BOOT.to_string(),
            };
        }

        // if init_boot exists and not skipping it, use it
        if !is_replace_kernel && init_boot_exist && !skip_init_boot {
            return BOOT_PARTITION_INIT_BOOT.to_string();
        }

        BOOT_PARTITION_BOOT.to_string()
    }

    pub fn get_slot_suffix(ota: bool) -> String {
        let mut slot_suffix = utils::getprop("ro.boot.slot_suffix").unwrap_or_default();
        if !slot_suffix.is_empty() && ota {
            if slot_suffix == "_a" {
                slot_suffix = "_b".to_string();
            } else {
                slot_suffix = "_a".to_string();
            }
        }
        slot_suffix
    }

    pub fn list_available_partitions() -> Vec<String> {
        let slot_suffix = get_slot_suffix(false);
        BOOT_FAMILY_PARTITIONS
            .into_iter()
            .filter(|name| Path::new(&format!("/dev/block/by-name/{name}{slot_suffix}")).exists())
            .map(ToString::to_string)
            .collect()
    }

    pub(super) fn auto_boot_partition_path(
        kmi: &str,
        ota: bool,
        is_replace_kernel: bool,
        partition: &Option<String>,
    ) -> PathBuf {
        let slot_suffix = get_slot_suffix(ota);
        let name = choose_boot_partition(kmi, is_replace_kernel, partition);
        PathBuf::from(format!("/dev/block/by-name/{name}{slot_suffix}"))
    }

    pub(super) fn post_ota() -> Result<()> {
        use crate::assets::BOOTCTL_PATH;
        use crate::defs::ADB_DIR;
        let status = Command::new(BOOTCTL_PATH).arg("hal-info").status()?;
        if !status.success() {
            return Ok(());
        }

        let current_slot = Command::new(BOOTCTL_PATH)
            .arg("get-current-slot")
            .output()?
            .stdout;
        let current_slot = String::from_utf8(current_slot)?;
        let current_slot = current_slot.trim();
        let target_slot = i32::from(current_slot == "0");

        Command::new(BOOTCTL_PATH)
            .arg(format!("set-active-boot-slot {target_slot}"))
            .status()?;

        let post_fs_data = Path::new(ADB_DIR).join("post-fs-data.d");
        utils::ensure_dir_exists(&post_fs_data)?;
        let post_ota_sh = post_fs_data.join("post_ota.sh");

        let sh_content = format!(
            r"
{BOOTCTL_PATH} mark-boot-successful
rm -f {BOOTCTL_PATH}
rm -f /data/adb/post-fs-data.d/post_ota.sh
"
        );

        std::fs::write(&post_ota_sh, sh_content)?;
        std::fs::set_permissions(post_ota_sh, std::fs::Permissions::from_mode(0o755))?;

        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use android::*;

fn map_file(file: &Path) -> Result<Mmap> {
    let mut f = File::open(file).with_context(|| format!("open {}", file.display()))?;
    let len = f
        .seek(SeekFrom::End(0))
        .with_context(|| format!("seek end of {}", file.display()))? as usize;
    let mmap = unsafe { MmapOptions::new().len(len).map(&f)? };
    Ok(mmap)
}

fn parse_kmi(buffer: &[u8]) -> Result<String> {
    let re = Regex::new(r"(\d+\.\d+)(?:\S+)?(android\d+)").context("Failed to compile regex")?;
    buffer
        .windows(4)
        .enumerate()
        .filter(|(_, x)| {
            x[1] == b'.'
                && x[2].is_ascii_digit()
                && match x[0] {
                    b'5' => x[3].is_ascii_digit(),
                    b'6'..=b'9' => true,
                    _ => false,
                }
        })
        .find_map(|(i, _)| {
            let a = &buffer[i..buffer.len().min(i + 100)];
            if let Some(e) = a.iter().position(|c| *c == 0)
                && let Ok(s) = std::str::from_utf8(&a[..e])
                && let Some(caps) = re.captures(s)
                && let (Some(kernel_version), Some(android_version)) = (caps.get(1), caps.get(2))
            {
                Some(format!(
                    "{}-{}",
                    android_version.as_str(),
                    kernel_version.as_str()
                ))
            } else {
                None
            }
        })
        .ok_or_else(|| {
            println!("- Failed to get KMI version");
            anyhow!("Try to choose LKM manually")
        })
}

fn parse_kmi_from_kernel(kernel: &Path) -> Result<String> {
    let data = std::fs::read(kernel).context("Failed to read kernel file")?;
    parse_kmi(&data)
}

fn parse_kmi_from_boot(image: &Path) -> Result<String> {
    let data = map_file(image)?;
    let boot = BootImage::parse(&data)?;
    if let Some(kernel) = boot.get_blocks().get_kernel() {
        let mut output = Vec::<u8>::new();
        kernel.dump(&mut output, false)?;
        parse_kmi(&output)
    } else {
        bail!("no kernel found in boot image")
    }
}

/// For vendor boot, prefer the `init_boot` ramdisk entry over the one with empty name,
/// matching the original magiskboot lookup order (init_boot.cpio before ramdisk.cpio).
fn extract_ramdisk(ramdisk_image: &RamdiskImage) -> Result<(Cpio, Option<usize>)> {
    if ramdisk_image.is_vendor_ramdisk() {
        let (pos, target) = ramdisk_image
            .iter_vendor_ramdisk()
            .enumerate()
            .find(|e| e.1.get_name_raw() == b"init_boot")
            .or_else(|| {
                ramdisk_image
                    .iter_vendor_ramdisk()
                    .enumerate()
                    .find(|e| e.1.get_name_raw() == b"")
            })
            .ok_or_else(|| anyhow!("No suitable vendor ramdisk entry found"))?;
        let mut buf = Vec::<u8>::new();
        target.dump(&mut buf, false)?;
        Ok((Cpio::load_from_data(&buf)?, Some(pos)))
    } else {
        let mut buf = Vec::<u8>::new();
        ramdisk_image.dump(&mut buf, false)?;
        Ok((Cpio::load_from_data(&buf)?, None))
    }
}

pub fn classify_image(image: &Path) -> Result<String> {
    ensure!(image.exists(), "boot image not found");
    let boot_image_data = map_file(image)?;
    let boot_image = BootImage::parse(&boot_image_data)?;
    enforce_bootimage_version(&boot_image)?;

    let (cpio, _) = if let Some(ramdisk_image) = boot_image.get_blocks().get_ramdisk() {
        extract_ramdisk(ramdisk_image)?
    } else {
        (Cpio::new(), None)
    };

    let kind = if cpio
        .entries()
        .keys()
        .any(|path| is_kernel_module_path(path))
    {
        BOOT_PARTITION_VENDOR_BOOT
    } else {
        BOOT_PARTITION_INIT_BOOT
    };

    Ok(kind.to_string())
}

fn enforce_bootimage_version(boot: &BootImage<'_>) -> Result<()> {
    if let BootImageVersion::Android(ver) = boot.get_header().get_version()
        && ver < 3
    {
        bail!("bootimage version {ver} is not supported!")
    }
    Ok(())
}

fn normalize_module_name(name: &str) -> String {
    let trimmed = name.trim().trim_matches('/');
    let file_name = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(trimmed);
    file_name.to_string()
}

fn is_kernel_module_path(path: &str) -> bool {
    path.starts_with("lib/modules/")
        && Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ko"))
}

fn detect_boot_image_kind_by_name(path: &Path) -> Option<&'static str> {
    let normalized = path.file_name()?.to_str()?.to_ascii_lowercase();
    if normalized.ends_with(&format!("{BOOT_PARTITION_VENDOR_BOOT}.img")) {
        Some(BOOT_PARTITION_VENDOR_BOOT)
    } else if normalized.ends_with(&format!("{BOOT_PARTITION_INIT_BOOT}.img")) {
        Some(BOOT_PARTITION_INIT_BOOT)
    } else if normalized.ends_with(&format!("{BOOT_PARTITION_BOOT}.img")) {
        Some(BOOT_PARTITION_BOOT)
    } else {
        None
    }
}

#[cfg(target_os = "android")]
fn resolve_boot_image_kind_for_output(
    image_file: Option<&Path>,
    partition: Option<&str>,
) -> Option<String> {
    image_file
        .and_then(detect_boot_image_kind_by_name)
        .map(str::to_string)
        .or_else(|| {
            partition
                .filter(|value| BOOT_FAMILY_PARTITIONS.contains(value))
                .map(str::to_string)
        })
        .or_else(|| {
            image_file.and_then(|path| {
                classify_image(path)
                    .ok()
                    .filter(|kind| BOOT_FAMILY_PARTITIONS.contains(&kind.as_str()))
            })
        })
}

#[cfg(not(target_os = "android"))]
fn resolve_boot_image_kind_for_output(
    image_file: Option<&Path>,
    _partition: Option<&str>,
) -> Option<String> {
    image_file
        .and_then(detect_boot_image_kind_by_name)
        .map(str::to_string)
        .or_else(|| {
            image_file.and_then(|path| {
                classify_image(path)
                    .ok()
                    .filter(|kind| BOOT_FAMILY_PARTITIONS.contains(&kind.as_str()))
            })
        })
}

fn build_patched_output_name(kind: Option<&str>) -> String {
    let now = chrono::Utc::now();
    match kind {
        Some(kind) => format!(
            "kernelsu_patched_{}_{}.img",
            kind,
            now.format("%Y%m%d_%H%M%S")
        ),
        None => format!("kernelsu_patched_{}.img", now.format("%Y%m%d_%H%M%S")),
    }
}

fn build_restore_output_name(kind: Option<&str>) -> String {
    let now = chrono::Utc::now();
    match kind {
        Some(kind) => format!(
            "kernelsu_restore_{}_{}.img",
            kind,
            now.format("%Y%m%d_%H%M%S")
        ),
        None => format!("kernelsu_restore_{}.img", now.format("%Y%m%d_%H%M%S")),
    }
}

fn remove_module_from_index(cpio: &mut Cpio, index_path: &str, module_name: &str) -> Result<()> {
    let Some(entry) = cpio.entry_by_name(index_path) else {
        return Ok(());
    };
    let Some(data) = entry.data() else {
        return Ok(());
    };
    let text = std::str::from_utf8(data)?;
    let module_stem = module_name.trim_end_matches(".ko");
    let mut changed = false;
    let mut kept = Vec::<String>::new();

    for line in text.lines() {
        let value = line.trim();
        if value.is_empty() {
            continue;
        }

        let refs_by_path =
            value.contains(module_name) || value.contains(&format!("/{module_name}"));
        let refs_by_stem = value.contains(&format!(" {module_stem} "))
            || value.ends_with(&format!(" {module_stem}"))
            || value.starts_with(&format!("{module_stem} "))
            || value.starts_with(&format!("softdep {module_stem} "));

        if refs_by_path || refs_by_stem {
            changed = true;
            continue;
        }

        kept.push(line.to_string());
    }

    if !changed {
        return Ok(());
    }

    println!("- Cleaning reference in {index_path} for {module_name}");
    let mut rebuilt = kept.join("\n");
    if !rebuilt.is_empty() {
        rebuilt.push('\n');
    }

    cpio.rm(index_path, false);
    cpio.add(
        index_path,
        CpioEntry::regular(0o644, Box::new(rebuilt.into_bytes())),
    )?;
    Ok(())
}

fn remove_vendor_modules(cpio: &mut Cpio, remove_module: &[String]) -> Result<()> {
    if remove_module.is_empty() {
        return Ok(());
    }

    let mut module_roots: Vec<String> = vec!["lib/modules".to_string()];
    let prefix = "lib/modules/";
    for path in cpio.entries().keys() {
        let Some(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        let head = rest.find('/').map_or(rest, |idx| &rest[..idx]);
        if head.is_empty() || !head.ends_with("-gki") {
            continue;
        }
        let candidate = format!("{prefix}{head}");
        if !module_roots.iter().any(|root| root == &candidate) {
            println!("- Detected vendor module root: {candidate}");
            module_roots.push(candidate);
        }
    }

    let index_files = [
        "modules.load",
        "modules.dep",
        "modules.softdep",
        "modules.load.recovery",
    ];

    for raw_name in remove_module {
        let module_name = normalize_module_name(raw_name);
        if module_name.is_empty() {
            continue;
        }

        for root in &module_roots {
            let module_path = format!("{root}/{module_name}");
            if cpio.exists(&module_path) {
                println!("- Removing vendor module {module_path}");
                cpio.rm(&module_path, false);
            }

            for index in index_files {
                let index_path = format!("{root}/{index}");
                remove_module_from_index(cpio, &index_path, &module_name)?;
            }
        }
    }

    Ok(())
}

#[allow(clippy::struct_excessive_bools)]
#[derive(clap::Args, Debug)]
pub struct BootPatchArgs {
    /// boot image path, if not specified, will try to find the boot image automatically
    #[arg(short, long)]
    pub boot: Option<PathBuf>,

    /// kernel image path to replace
    #[arg(short, long)]
    pub kernel: Option<PathBuf>,

    /// LKM module path to replace, if not specified, will use the builtin one
    #[arg(short, long)]
    pub module: Option<PathBuf>,

    /// init to be replaced
    #[arg(short, long)]
    pub init: Option<PathBuf>,

    /// will use another slot when boot image is not specified
    #[cfg(target_os = "android")]
    #[arg(short = 'u', long, default_value = "false")]
    pub ota: bool,

    /// Flash it to boot partition after patch
    #[cfg(target_os = "android")]
    #[arg(short, long, default_value = "false")]
    pub flash: bool,

    /// Force backup source image as stock image
    #[cfg(target_os = "android")]
    #[arg(long, default_value = "false")]
    pub backup: bool,

    /// Output path. If not specified, will use current directory.
    /// If specified, the boot image will be written to the directory
    /// even if --flash is specified.
    #[cfg(target_os = "android")]
    #[arg(short, long, default_value = None)]
    pub out: Option<PathBuf>,

    /// Output path. If not specified, will use current directory.
    #[cfg(not(target_os = "android"))]
    #[arg(short, long, default_value = None)]
    pub out: Option<PathBuf>,

    /// KMI version, if specified, will use the specified KMI
    #[arg(long, default_value = None)]
    pub kmi: Option<String>,

    /// target partition override (init_boot | boot | vendor_boot)
    #[cfg(target_os = "android")]
    #[arg(long, default_value = None)]
    pub partition: Option<String>,

    /// File name of the output. If specified, the boot image will be
    /// written to the output directory even if --flash is specified.
    #[cfg(target_os = "android")]
    #[arg(long, default_value = None)]
    pub out_name: Option<String>,

    /// File name of the output.
    #[cfg(not(target_os = "android"))]
    #[arg(long, default_value = None)]
    pub out_name: Option<String>,

    /// Extra cmdline to append to boot image header
    #[arg(long, default_value = None)]
    pub cmdline: Option<String>,

    /// Always allow shell to get root permission
    #[arg(long, default_value = "false")]
    allow_shell: bool,

    /// Force enable adbd and disable adbd auth
    #[arg(long, default_value = "false")]
    enable_adbd: bool,

    /// Add more adb_debug prop
    #[arg(long, required = false)]
    adb_debug_prop: Option<String>,

    /// Do not (re-)install kernelsu, only modify configs (allow_shell, etc.)
    #[arg(long, default_value = "false")]
    no_install: bool,

    /// Do not load custom rc
    #[arg(long, default_value = "false")]
    no_custom_rc: bool,

    /// Remove matching vendor ramdisk modules and their index references
    #[arg(long = "remove-module", value_name = "MODULE")]
    pub remove_module: Vec<String>,
}

pub fn patch_vivo(mut args: BootPatchArgs) -> Result<()> {
    const VIVO_REMOVE_MODULES: &[&str] = &["vr.ko"];

    // This entry point keeps the current branch behavior explicit: one vivo-only
    // flow that patches boot/init_boot and vendor_boot with the same command.
    println!("- Mode: vivo compat (auto-detect init_boot vs vendor_boot)");

    for module in VIVO_REMOVE_MODULES {
        if !args
            .remove_module
            .iter()
            .any(|value| normalize_module_name(value) == *module)
        {
            args.remove_module.push((*module).to_string());
        }
    }

    patch(args)
}

pub fn patch(args: BootPatchArgs) -> Result<()> {
    let inner = move || {
        let BootPatchArgs {
            boot: image,
            init,
            kernel,
            module: kmod,
            out,
            kmi,
            out_name,
            cmdline,
            allow_shell,
            enable_adbd,
            adb_debug_prop,
            mut no_install,
            #[cfg(target_os = "android")]
            ota,
            #[cfg(target_os = "android")]
            flash,
            #[cfg(target_os = "android")]
            backup,
            #[cfg(target_os = "android")]
            partition,
            no_custom_rc,
            remove_module,
        } = args;

        println!(include_str!("banner"));

        #[cfg(target_os = "android")]
        let patch_file = image.is_some();

        #[cfg(target_os = "android")]
        if !patch_file {
            ensure_gki_kernel()?;
        }

        let is_replace_kernel = kernel.is_some();

        if is_replace_kernel {
            ensure!(
                init.is_none() && kmod.is_none(),
                "init and module must not be specified."
            );
        }

        let image_file = if let Some(image) = image.as_ref() {
            ensure!(image.exists(), "boot image not found");
            Some(std::fs::canonicalize(image)?)
        } else {
            None
        };

        #[cfg(target_os = "android")]
        let is_vendor_boot_target = if partition.as_deref() == Some(BOOT_PARTITION_VENDOR_BOOT) {
            true
        } else if let Some(image_path) = image_file.as_ref() {
            classify_image(image_path)? == BOOT_PARTITION_VENDOR_BOOT
        } else {
            false
        };

        #[cfg(not(target_os = "android"))]
        let is_vendor_boot_target = image_file.as_ref().is_some_and(|image_path| {
            classify_image(image_path).is_ok_and(|kind| kind == BOOT_PARTITION_VENDOR_BOOT)
        });

        if is_vendor_boot_target && !is_replace_kernel {
            if !no_install {
                println!(
                    "- Auto-detected vendor_boot target before KMI parsing; skipping LKM injection"
                );
            }
            no_install = true;
        }

        let should_skip_kmi = is_vendor_boot_target && !is_replace_kernel && kmod.is_none();

        let kmi = if should_skip_kmi {
            kmi.unwrap_or_default()
        } else {
            kmi.map_or_else(
                || -> Result<_> {
                    if kmod.is_some() {
                        return Ok(String::new());
                    }
                #[cfg(target_os = "android")]
                if ota {
                    let slot_suffix = get_slot_suffix(true);
                    println!("- Trying to auto detect KMI version from boot");
                    return parse_kmi_from_boot(Path::new(&format!(
                        "/dev/block/by-name/boot{slot_suffix}"
                    )));
                }
                    #[cfg(target_os = "android")]
                    match get_current_kmi() {
                        Ok(value) => {
                            return Ok(value);
                        }
                        Err(e) => {
                            println!("- {e}");
                        }
                    }
                    Ok(if let Some(image_path) = image_file.as_ref() {
                        println!(
                            "- Trying to auto detect KMI version for {}",
                            image_path.display()
                        );
                        parse_kmi_from_boot(image_path)?
                    } else if let Some(kernel_path) = &kernel {
                        println!(
                            "- Trying to auto detect KMI version for {}",
                            kernel_path.display()
                        );
                        parse_kmi_from_kernel(kernel_path)?
                    } else {
                        String::new()
                    })
                },
                Ok,
            )?
        };

        let boot_image_file = image_file.as_ref().map_or_else(
            || {
                #[cfg(target_os = "android")]
                {
                    Ok::<PathBuf, anyhow::Error>(auto_boot_partition_path(
                        &kmi,
                        ota,
                        is_replace_kernel,
                        &partition,
                    ))
                }
                #[cfg(not(target_os = "android"))]
                {
                    bail!("Please specify a boot image");
                }
            },
            |path| Ok(path.clone()),
        )?;

        #[cfg(target_os = "android")]
        println!("- Bootdevice: {}", boot_image_file.display());

        // try extract bootctl/busybox
        #[cfg(target_os = "android")]
        let _ = assets::ensure_binaries(false);

        println!("- Preparing assets");
        println!("- Unpacking boot image");
        let boot_image_data = map_file(&boot_image_file)?;
        let boot_image = BootImage::parse(&boot_image_data)?;
        enforce_bootimage_version(&boot_image)?;

        let mut patcher = BootImagePatchOption::new(&boot_image);

        if let Some(cmdline_value) = &cmdline {
            patcher.override_cmdline(cmdline_value.as_bytes());
            println!("- Set cmdline to: {cmdline_value}");
        }

        if let Some(kernel_path) = kernel {
            println!("- Adding Kernel");
            let kernel_data = map_file(&kernel_path)?;
            patcher.replace_kernel(Box::new(Cursor::new(kernel_data)), false);
        }

        let kernelsu_ko: Box<dyn AsRef<[u8]>> = if no_install {
            Box::new(Vec::<u8>::new())
        } else if let Some(kmod_path) = kmod {
            Box::new(map_file(&kmod_path)?)
        } else {
            println!("- KMI: {kmi}");
            let name = format!("{kmi}_kernelsu.ko");
            assets::get_asset(&name).with_context(|| format!("Failed to load {name}"))?
        };

        let ksu_init: Box<dyn AsRef<[u8]>> = if no_install {
            Box::new(Vec::<u8>::new())
        } else if let Some(init_path) = init {
            Box::new(map_file(&init_path)?)
        } else {
            assets::get_asset("ksuinit").context("Failed to load ksuinit")?
        };

        let (mut cpio, vendor_ramdisk_idx) =
            if let Some(ramdisk_image) = boot_image.get_blocks().get_ramdisk() {
                extract_ramdisk(ramdisk_image)?
            } else {
                println!("- No ramdisk, create by default");
                (Cpio::new(), None)
            };

        if !no_install {
            let looks_like_vendor = cpio
                .entries()
                .keys()
                .any(|path| is_kernel_module_path(path));
            if looks_like_vendor {
                println!(
                    "- Auto-detected vendor_boot (lib/modules/*.ko present); skipping LKM injection"
                );
                no_install = true;
            }
        }

        if !no_install {
            ensure!(
                !cpio.is_magisk_patched(),
                "Cannot work with Magisk patched image"
            );

            println!("- Adding KernelSU LKM");
            let is_kernelsu_patched = cpio.exists("kernelsu.ko");

            if !is_kernelsu_patched && cpio.exists("init") {
                cpio.mv("init", "init.real")?;
            }

            cpio.add("init", CpioEntry::regular(0o755, ksu_init))?;
            cpio.add("kernelsu.ko", CpioEntry::regular(0o755, kernelsu_ko))?;

            #[cfg(target_os = "android")]
            if (backup || (!is_kernelsu_patched && flash))
                && let Err(e) = do_backup(&mut cpio, &boot_image_file)
            {
                println!("- Backup stock image failed: {e:?}");
            }
        }

        let mut ksu_config: Vec<String> = cpio
            .entry_by_name("ksu_config")
            .and_then(CpioEntry::data)
            .and_then(|v| str::from_utf8(v).ok())
            .map(|v| v.split(' ').map(std::borrow::ToOwned::to_owned).collect())
            .unwrap_or_default();

        let mut apply_config = |name: &str, value: &str, add: bool| {
            let has_value = ksu_config.iter().any(|v| v == value);

            if add {
                println!("- Adding {name} config");
                if !has_value {
                    ksu_config.push(value.to_owned());
                }
            } else if has_value {
                println!("- Removing {name} config");
                ksu_config.retain(|v| v != value);
            }
        };

        apply_config("no custom rc", "norc=1", no_custom_rc);
        apply_config("allow shell", "allow_shell=1", allow_shell);

        if ksu_config.is_empty() {
            cpio.rm("ksu_config", false);
        } else {
            let data = ksu_config.join(" ").into_bytes();
            cpio.add("ksu_config", CpioEntry::regular(0o644, Box::new(data)))?;
        }

        // remove legacy config file
        cpio.rm("allow_shell", false);

        if enable_adbd || adb_debug_prop.is_some() {
            println!("- Adding adb_debug props");
            cpio.add(
                "force_debuggable",
                CpioEntry::regular(0o644, Box::new(Vec::<u8>::new())),
            )?;

            let mut prop = Vec::<u8>::new();
            if enable_adbd {
                println!("- Adding props to enable adbd");
                prop.extend_from_slice(
                    b"ro.debuggable=1\nro.force.debuggable=1\nro.adb.secure=0\n",
                );
            }
            if let Some(extra) = adb_debug_prop {
                println!("- Adding custom props");
                prop.extend_from_slice(extra.as_bytes());
            }
            cpio.add("adb_debug.prop", CpioEntry::regular(0o644, Box::new(prop)))?;
        } else {
            if cpio.exists("force_debuggable") {
                println!("- Removing /force_debuggable");
                cpio.rm("force_debuggable", false);
            }
            if cpio.exists("adb_debug.prop") {
                println!("- Removing /adb_debug.prop");
                cpio.rm("adb_debug.prop", false);
            }
        }

        remove_vendor_modules(&mut cpio, &remove_module)?;

        let mut new_cpio = Vec::<u8>::new();
        cpio.dump(&mut new_cpio)?;

        if let Some(idx) = vendor_ramdisk_idx {
            patcher.replace_vendor_ramdisk(idx, Box::new(Cursor::new(new_cpio)), false);
        } else {
            patcher.replace_ramdisk(Box::new(Cursor::new(new_cpio)), false);
        }

        println!("- Repacking boot image");
        let mut new_boot_buf = Cursor::new(Vec::<u8>::with_capacity(boot_image.get_size()));
        patcher.patch(&mut new_boot_buf)?;
        let new_boot_bytes = new_boot_buf.into_inner();

        // Free the source mmap so the boot partition is no longer mapped read-only,
        // otherwise some kernels reject the subsequent write.
        drop(boot_image);
        drop(boot_image_data);

        #[cfg(target_os = "android")]
        if flash {
            println!("- Flashing new boot image");
            let bootdevice = boot_image_file.display().to_string();
            flash_partition(&bootdevice, &new_boot_bytes)?;
            if ota {
                post_ota()?;
            }
        }

        #[cfg(target_os = "android")]
        let should_write_output = patch_file || !flash || out_name.is_some() || out.is_some();
        #[cfg(not(target_os = "android"))]
        let should_write_output = true;

        if should_write_output {
            let output_dir = out.unwrap_or(std::env::current_dir()?);
            let output_kind = resolve_boot_image_kind_for_output(image_file.as_deref(), {
                #[cfg(target_os = "android")]
                {
                    partition.as_deref()
                }
                #[cfg(not(target_os = "android"))]
                {
                    None
                }
            });
            let name =
                out_name.unwrap_or_else(|| build_patched_output_name(output_kind.as_deref()));
            let output_image = output_dir.join(name);
            std::fs::write(&output_image, &new_boot_bytes).context("write out new boot failed")?;
            println!("- Output file is written to");
            println!("- {}", output_image.display().to_string().trim_matches('"'));
        }

        println!("- Done!");
        Ok(())
    };

    let result = inner();
    if let Err(ref e) = result {
        println!("- Patch Error: {e}");
    }
    result
}

#[derive(clap::Args, Debug)]
pub struct BootRestoreArgs {
    /// boot image path, if not specified, will try to find the boot image automatically
    #[arg(short, long)]
    pub boot: Option<PathBuf>,

    /// Flash it to boot partition after restore
    #[cfg(target_os = "android")]
    #[arg(short, long, default_value = "false")]
    pub flash: bool,

    /// Output path. If not specified, will use current directory.
    /// If specified, the boot image will be written to the directory
    /// even if --flash is specified.
    #[cfg(target_os = "android")]
    #[arg(short, long, default_value = None)]
    pub out: Option<PathBuf>,

    /// Output path. If not specified, will use current directory.
    #[cfg(not(target_os = "android"))]
    #[arg(short, long, default_value = None)]
    pub out: Option<PathBuf>,

    /// File name of the output. If specified, the boot image will be
    /// written to the output directory even if --flash is specified.
    #[cfg(target_os = "android")]
    #[arg(long, default_value = None)]
    pub out_name: Option<String>,

    /// File name of the output.
    #[cfg(not(target_os = "android"))]
    #[arg(long, default_value = None)]
    pub out_name: Option<String>,
}

pub fn restore(args: BootRestoreArgs) -> Result<()> {
    let BootRestoreArgs {
        boot: image,
        out_name,
        out,
        #[cfg(target_os = "android")]
        flash,
    } = args;

    #[cfg(target_os = "android")]
    let kmi = get_current_kmi().unwrap_or_default();

    #[cfg(target_os = "android")]
    let image_supplied = image.is_some();

    let boot_image_file = if let Some(image) = image {
        ensure!(image.exists(), "boot image not found");
        std::fs::canonicalize(image)?
    } else {
        #[cfg(target_os = "android")]
        {
            auto_boot_partition_path(&kmi, false, false, &None)
        }
        #[cfg(not(target_os = "android"))]
        {
            bail!("Please specify a boot image");
        }
    };

    #[cfg(target_os = "android")]
    println!("- Bootdevice: {}", boot_image_file.display());

    println!("- Unpacking boot image");
    let bootimage_data = map_file(&boot_image_file)?;
    let boot_image = BootImage::parse(&bootimage_data)?;
    enforce_bootimage_version(&boot_image)?;

    let (mut cpio, vendor_ramdisk_idx) =
        if let Some(ramdisk_image) = boot_image.get_blocks().get_ramdisk() {
            extract_ramdisk(ramdisk_image)?
        } else {
            bail!("No compatible ramdisk found.")
        };

    ensure!(
        cpio.exists("kernelsu.ko"),
        "boot image is not patched by KernelSU"
    );

    #[cfg(target_os = "android")]
    let mut stock_boot: Option<PathBuf> = None;

    #[cfg(target_os = "android")]
    if let Some(backup_file) = cpio.entry_by_name(BACKUP_FILENAME) {
        let sha = String::from_utf8(backup_file.data().unwrap_or_default().to_vec())?;
        let sha = sha.trim();
        let backup_path =
            PathBuf::from(KSU_BACKUP_DIR).join(format!("{KSU_BACKUP_FILE_PREFIX}{sha}"));
        if backup_path.is_file() {
            println!("- Using backup file {}", backup_path.display());
            stock_boot = Some(backup_path);
        } else {
            println!("- Warning: no backup {} found!", backup_path.display());
        }
        if let Err(e) = clean_backup(sha) {
            println!("- Warning: Cleanup backup image failed: {e}");
        }
    } else {
        println!("- Backup info is absent!");
    }

    #[cfg(target_os = "android")]
    let mut stock_source: Option<PathBuf> = None;

    let new_boot_bytes: Vec<u8> = {
        #[cfg(target_os = "android")]
        {
            if let Some(stock_path) = stock_boot {
                let bytes = std::fs::read(&stock_path)
                    .with_context(|| format!("read stock boot {}", stock_path.display()))?;
                stock_source = Some(stock_path);
                bytes
            } else {
                rebuild_without_ksu(&boot_image, &mut cpio, vendor_ramdisk_idx)?
            }
        }
        #[cfg(not(target_os = "android"))]
        {
            rebuild_without_ksu(&boot_image, &mut cpio, vendor_ramdisk_idx)?
        }
    };

    drop(boot_image);
    drop(bootimage_data);

    #[cfg(target_os = "android")]
    if flash {
        if let Some(ref source) = stock_source {
            println!("- Flashing new boot image from {}", source.display());
        } else {
            println!("- Flashing new boot image");
        }
        let bootdevice = boot_image_file.display().to_string();
        flash_partition(&bootdevice, &new_boot_bytes)?;
    }

    #[cfg(target_os = "android")]
    let should_write_output = image_supplied || !flash || out_name.is_some() || out.is_some();
    #[cfg(not(target_os = "android"))]
    let should_write_output = true;

    if should_write_output {
        let output_dir = out.unwrap_or(std::env::current_dir()?);
        let output_kind = resolve_boot_image_kind_for_output(Some(&boot_image_file), None);
        let name = out_name.unwrap_or_else(|| build_restore_output_name(output_kind.as_deref()));
        let output_image = output_dir.join(name);
        std::fs::write(&output_image, &new_boot_bytes).context("copy out new boot failed")?;
        println!("- Output file is written to");
        println!("- {}", output_image.display().to_string().trim_matches('"'));
    }

    println!("- Done!");
    Ok(())
}

fn rebuild_without_ksu(
    boot_image: &BootImage<'_>,
    cpio: &mut Cpio,
    vendor_ramdisk_idx: Option<usize>,
) -> Result<Vec<u8>> {
    println!("- Removing KernelSU from boot image");
    cpio.rm("kernelsu.ko", false);
    if cpio.exists("init.real") {
        cpio.mv("init.real", "init")?;
    }

    let mut new_cpio = Vec::<u8>::new();
    cpio.dump(&mut new_cpio)?;

    println!("- Repacking boot image");
    let mut patcher = BootImagePatchOption::new(boot_image);
    if let Some(idx) = vendor_ramdisk_idx {
        patcher.replace_vendor_ramdisk(idx, Box::new(Cursor::new(new_cpio)), false);
    } else {
        patcher.replace_ramdisk(Box::new(Cursor::new(new_cpio)), false);
    }

    let mut buf = Cursor::new(Vec::<u8>::with_capacity(boot_image.get_size()));
    patcher.patch(&mut buf)?;
    Ok(buf.into_inner())
}
