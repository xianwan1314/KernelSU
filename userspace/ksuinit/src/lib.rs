use anyhow::{Context, Result, bail};
use goblin::elf::{Elf, section_header, sym::Sym};
use scroll::{Pwrite, ctx::SizeWith};
use std::collections::HashMap;
use std::ffi::CStr;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::io::{ErrorKind, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use syscalls::{Sysno, syscall};

struct Kptr {
    value: String,
}

impl Kptr {
    pub fn new() -> Result<Self> {
        let value = fs::read_to_string("/proc/sys/kernel/kptr_restrict")?;
        fs::write("/proc/sys/kernel/kptr_restrict", "1")?;
        Ok(Kptr { value })
    }
}

impl Drop for Kptr {
    fn drop(&mut self) {
        let _ = fs::write("/proc/sys/kernel/kptr_restrict", self.value.as_bytes());
    }
}

pub struct KptrOwnedIter<I> {
    _kptr: Kptr,
    iter: I,
}

impl<I: Iterator> Iterator for KptrOwnedIter<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}

pub fn kernel_symbols_iter() -> Result<impl Iterator<Item = (String, u64)>> {
    let kptr = Kptr::new()?;

    let iter = BufReader::new(File::open("/proc/kallsyms")?)
        .lines()
        .map_while(|line| {
            line.ok().and_then(|line| {
                let mut splits = line.split_whitespace();
                splits
                    .next()
                    .and_then(|addr| u64::from_str_radix(addr, 16).ok())
                    .and_then(|addr| {
                        splits
                            .nth(1)
                            .take_if(|_| splits.next().is_none())
                            .map(|symbol| {
                                (
                                    symbol
                                        .find("$")
                                        .or_else(|| symbol.find(".llvm."))
                                        .map(|pos| &symbol[0..pos])
                                        .unwrap_or(symbol)
                                        .to_owned(),
                                    addr,
                                )
                            })
                    })
            })
        });

    Ok(KptrOwnedIter { _kptr: kptr, iter })
}

pub fn for_each_kernel_symbols<F: FnMut(&(String, u64)) -> Result<bool>>(mut f: F) -> Result<()> {
    for item in kernel_symbols_iter()? {
        if !f(&item)? {
            break;
        }
    }
    Ok(())
}

pub fn load_module(data: &[u8], params: &CStr) -> Result<()> {
    load_module_inner(data, "module", None, params)
}

pub fn load_module_with_vermagic_fallback(
    data: &[u8],
    fallback_data: Option<&[u8]>,
    params: &CStr,
) -> Result<()> {
    load_module_inner(
        data,
        "primary module",
        fallback_data.map(|data| (data, "fallback module")),
        params,
    )
}

pub fn load_module_with_named_vermagic_fallback(
    data: &[u8],
    primary_name: &str,
    fallback: Option<(&[u8], &str)>,
    params: &CStr,
) -> Result<()> {
    load_module_inner(data, primary_name, fallback, params)
}

fn load_module_inner(
    data: &[u8],
    module_name: &str,
    fallback: Option<(&[u8], &str)>,
    params: &CStr,
) -> Result<()> {
    let mut buffer = data.to_vec();
    let elf = Elf::parse(&buffer)?;
    let ctx = *elf.syms.ctx();

    let mut unresolved_symbols: HashMap<String, (Sym, usize)> = HashMap::new();
    for (index, sym) in elf.syms.iter().enumerate() {
        if index == 0 {
            continue;
        }

        if sym.st_shndx != section_header::SHN_UNDEF as usize {
            continue;
        }

        let Some(name) = elf.strtab.get_at(sym.st_name) else {
            continue;
        };

        let offset = elf.syms.offset() + index * Sym::size_with(elf.syms.ctx());
        unresolved_symbols.insert(name.to_owned(), (sym, offset));
    }

    if !unresolved_symbols.is_empty() {
        for_each_kernel_symbols(|(symbol, addr)| {
            if let Some((mut sym, offset)) = unresolved_symbols.remove(symbol) {
                sym.st_shndx = section_header::SHN_ABS as usize;
                sym.st_value = *addr;
                buffer.pwrite_with(sym, offset, ctx)?;
            }

            Ok(!unresolved_symbols.is_empty())
        })
        .context("Cannot parse kallsyms")?;
    }

    for name in unresolved_symbols.keys() {
        log::warn!("Cannot find symbol: {}", name);
    }

    let mut kmsg = match open_kmsg_at_end() {
        Ok(file) => Some(file),
        Err(error) => {
            log::warn!("Cannot prepare kmsg fallback: {error:#}");
            None
        }
    };

    match do_init_module(&buffer, params) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            log::warn!(
                "init_module failed for {} on first attempt: {:?}",
                module_name,
                first_error
            );

            let logs = match kmsg.as_mut() {
                Some(file) => read_new_kmsg(file).unwrap_or_default(),
                None => String::new(),
            };

            let Some(required_vermagic) = extract_required_vermagic(&logs) else {
                log::error!(
                    "init_module failed without a version-magic mismatch. Kernel log:\n{}",
                    logs
                );
                return Err(anyhow::anyhow!("init_module failed: {:?}", first_error));
            };

            log::warn!(
                "Kernel requires vermagic {:?} while loading {}",
                required_vermagic,
                module_name
            );

            if let Some((fallback_data, fallback_name)) = fallback {
                log::warn!(
                    "Version-magic mismatch for {}; switching to fallback {}",
                    module_name,
                    fallback_name
                );
                load_module_inner(fallback_data, fallback_name, None, params)
                    .with_context(|| format!("Fallback module load failed for {fallback_name}"))?;
                log::info!(
                    "Fallback module {} loaded successfully after {} vermagic mismatch",
                    fallback_name,
                    module_name
                );
                return Ok(());
            }

            log::warn!(
                "No fallback module for {}; patching vermagic in-place and retrying",
                module_name
            );
            replace_module_vermagic(&mut buffer, &required_vermagic)
                .context("Cannot replace module vermagic")?;

            do_init_module(&buffer, params).map_err(|error| {
                anyhow::anyhow!("init_module failed after replacing vermagic: {:?}", error)
            })?;
            log::info!(
                "Loaded {} successfully after in-place vermagic replacement",
                module_name
            );
            Ok(())
        }
    }
}

fn do_init_module(buffer: &[u8], params: &CStr) -> std::result::Result<(), syscalls::Errno> {
    unsafe {
        syscall!(
            Sysno::init_module,
            buffer.as_ptr(),
            buffer.len(),
            params.as_ptr()
        )
    }
    .map(|_| ())
}

#[cfg(unix)]
const O_NONBLOCK_FLAG: i32 = 0x800;

fn open_kmsg_at_end() -> Result<File> {
    #[cfg(not(unix))]
    {
        bail!("kernel log fallback is only supported on unix targets");
    }

    #[cfg(unix)]
    {
        let mut last_error = None;

        for path in ["/dev/kmsg", "/kmsg"] {
            match OpenOptions::new()
                .read(true)
                .custom_flags(O_NONBLOCK_FLAG)
                .open(path)
            {
                Ok(mut file) => {
                    file.seek(SeekFrom::End(0))
                        .with_context(|| format!("Cannot seek {path} to end"))?;
                    log::info!("Reading kernel log from {path}");
                    return Ok(file);
                }
                Err(error) => {
                    last_error = Some((path, error));
                }
            }
        }

        match last_error {
            Some((path, error)) => Err(error)
                .with_context(|| format!("Cannot open kernel log device, last tried {path}")),
            None => bail!("No kernel log device candidate"),
        }
    }
}

fn read_new_kmsg(file: &mut File) -> Result<String> {
    #[cfg(not(unix))]
    {
        let _ = file;
        bail!("kernel log fallback is only supported on unix targets");
    }

    #[cfg(unix)]
    {
        let mut output = Vec::new();
        let mut record = [0u8; 8192];

        loop {
            match file.read(&mut record) {
                Ok(0) => break,
                Ok(length) => {
                    output.extend_from_slice(&record[..length]);
                    output.push(b'\n');
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("Cannot read kmsg"),
            }
        }

        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}

fn extract_required_vermagic(kmsg: &str) -> Option<String> {
    const PREFIX: &str = "version magic '";
    const SEPARATOR: &str = "' should be '";

    for record in kmsg.lines().rev() {
        let message = record
            .split_once(';')
            .map(|(_, message)| message)
            .unwrap_or(record);

        let Some(prefix_position) = message.find(PREFIX) else {
            continue;
        };
        let after_prefix = &message[prefix_position + PREFIX.len()..];

        let Some(separator_position) = after_prefix.find(SEPARATOR) else {
            continue;
        };
        let required = &after_prefix[separator_position + SEPARATOR.len()..];

        let Some(end_quote) = required.find('\'') else {
            continue;
        };
        let required = &required[..end_quote];

        if !required.is_empty() {
            return Some(required.to_owned());
        }
    }

    None
}

fn align_up(value: usize, alignment: usize) -> Result<usize> {
    let alignment = alignment.max(1);

    if !alignment.is_power_of_two() {
        bail!("Invalid ELF alignment: {alignment}");
    }

    value
        .checked_add(alignment - 1)
        .map(|next| next & !(alignment - 1))
        .context("ELF alignment overflow")
}

fn write_elf64_word(
    buffer: &mut [u8],
    offset: usize,
    value: u64,
    little_endian: bool,
) -> Result<()> {
    let end = offset.checked_add(8).context("ELF write overflow")?;
    let destination = buffer
        .get_mut(offset..end)
        .context("ELF write outside module buffer")?;

    let bytes = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    destination.copy_from_slice(&bytes);
    Ok(())
}

fn replace_module_vermagic(buffer: &mut Vec<u8>, required_vermagic: &str) -> Result<()> {
    struct ModinfoLocation {
        offset: usize,
        size: usize,
        section_header_offset: usize,
        alignment: usize,
        little_endian: bool,
    }

    let location = {
        let elf = Elf::parse(buffer)?;

        if !elf.is_64 {
            bail!("Only ELF64 modules are supported");
        }

        let section_table_offset =
            usize::try_from(elf.header.e_shoff).context("Section table offset overflow")?;
        let section_entry_size = usize::from(elf.header.e_shentsize);
        let mut location = None;

        for (index, section) in elf.section_headers.iter().enumerate() {
            let Some(name) = elf.shdr_strtab.get_at(section.sh_name) else {
                continue;
            };
            if name != ".modinfo" {
                continue;
            }

            let offset = usize::try_from(section.sh_offset).context(".modinfo offset overflow")?;
            let size = usize::try_from(section.sh_size).context(".modinfo size overflow")?;
            let end = offset
                .checked_add(size)
                .context(".modinfo range overflow")?;

            if end > buffer.len() {
                bail!(".modinfo is outside module buffer");
            }

            let section_header_offset = section_table_offset
                .checked_add(
                    index
                        .checked_mul(section_entry_size)
                        .context("Section index overflow")?,
                )
                .context("Section header offset overflow")?;

            location = Some(ModinfoLocation {
                offset,
                size,
                section_header_offset,
                alignment: usize::try_from(section.sh_addralign).unwrap_or(1).max(1),
                little_endian: elf.little_endian,
            });
            break;
        }

        location.context("Module has no .modinfo section")?
    };

    let old_modinfo = &buffer[location.offset..location.offset + location.size];
    let replacement = format!("vermagic={required_vermagic}");
    let mut new_modinfo = Vec::with_capacity(old_modinfo.len().max(replacement.len() + 1));
    let mut replaced = false;

    for entry in old_modinfo.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }

        if entry.starts_with(b"vermagic=") {
            if !replaced {
                new_modinfo.extend_from_slice(replacement.as_bytes());
                new_modinfo.push(0);
                replaced = true;
            }
        } else {
            new_modinfo.extend_from_slice(entry);
            new_modinfo.push(0);
        }
    }

    if !replaced {
        new_modinfo.extend_from_slice(replacement.as_bytes());
        new_modinfo.push(0);
    }

    let new_offset = align_up(buffer.len(), location.alignment)?;
    buffer.resize(new_offset, 0);
    buffer.extend_from_slice(&new_modinfo);

    write_elf64_word(
        buffer,
        location.section_header_offset + 0x18,
        new_offset as u64,
        location.little_endian,
    )?;
    write_elf64_word(
        buffer,
        location.section_header_offset + 0x20,
        new_modinfo.len() as u64,
        location.little_endian,
    )?;

    log::warn!(
        "Replaced module vermagic with kernel-required value: {:?}",
        required_vermagic
    );
    Ok(())
}

fn has_kernelsu_legacy() -> bool {
    use syscalls::{Sysno, syscall};
    let mut version = 0;
    const CMD_GET_VERSION: i32 = 2;
    unsafe {
        let _ = syscall!(
            Sysno::prctl,
            0xDEADBEEF,
            CMD_GET_VERSION,
            std::ptr::addr_of_mut!(version)
        );
    }

    log::info!("KernelSU version: {}", version);

    version != 0
}

fn has_kernelsu_v2() -> bool {
    use syscalls::{Sysno, syscall};
    const KSU_INSTALL_MAGIC1: u32 = 0xDEADBEEF;
    const KSU_INSTALL_MAGIC2: u32 = 0xCAFEBABE;
    const KSU_IOCTL_GET_INFO: u32 = 0x80104b02;
    const KSU_IOCTL_GET_INFO_LEGACY: u32 = 0x80004b02;

    #[repr(C)]
    #[derive(Default)]
    struct GetInfoCmd {
        version: u32,
        flags: u32,
        features: u32,
        uapi_version: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct GetInfoLegacyCmd {
        version: u32,
        flags: u32,
        features: u32,
    }

    let mut fd: i32 = -1;
    unsafe {
        let _ = syscall!(
            Sysno::reboot,
            KSU_INSTALL_MAGIC1,
            KSU_INSTALL_MAGIC2,
            0,
            std::ptr::addr_of_mut!(fd)
        );
    }

    let version = if fd >= 0 {
        let mut cmd = GetInfoCmd::default();
        let version = unsafe {
            let ret = syscall!(Sysno::ioctl, fd, KSU_IOCTL_GET_INFO, &mut cmd as *mut _);

            match ret {
                Ok(_) => cmd.version,
                Err(_) => {
                    let mut legacy = GetInfoLegacyCmd::default();
                    match syscall!(
                        Sysno::ioctl,
                        fd,
                        KSU_IOCTL_GET_INFO_LEGACY,
                        &mut legacy as *mut _
                    ) {
                        Ok(_) => legacy.version,
                        Err(_) => 0,
                    }
                }
            }
        };

        unsafe {
            let _ = syscall!(Sysno::close, fd);
        }

        version
    } else {
        0
    };

    log::info!("KernelSU version: {}", version);

    version != 0
}

pub fn has_kernelsu() -> bool {
    has_kernelsu_v2() || has_kernelsu_legacy()
}
