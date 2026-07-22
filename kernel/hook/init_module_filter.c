#include <linux/uaccess.h>
#include <linux/slab.h>
#include <linux/elf.h>
#include <linux/string.h>
#include <linux/fs.h>
#include <linux/file.h>

#include "arch.h"
#include "klog.h" // IWYU pragma: keep
#include "hook/syscall_hook.h"
#include "hook/init_module_filter.h"

// Bounds for ELF metadata parsing. We only need the ELF header, section
// headers, section-name string table and .modinfo to identify the module.
#define KSU_MAX_SECTIONS 512
#define KSU_MAX_SHSTRTAB (64 * 1024)
#define KSU_MAX_MODINFO (16 * 1024)

struct ksu_elf_reader {
    const char __user *umod;
    unsigned long umod_len;
    struct file *file;
    loff_t file_size;
};

static int ksu_reader_read(struct ksu_elf_reader *reader, loff_t offset, void *buf, size_t len)
{
    loff_t total = reader->umod ? (loff_t)reader->umod_len : reader->file_size;

    if (len == 0)
        return -EINVAL;
    if (offset < 0 || (loff_t)len > total || offset > total - (loff_t)len)
        return -ERANGE;

    if (reader->umod) {
        if (copy_from_user(buf, reader->umod + offset, len))
            return -EFAULT;
        return 0;
    }

    {
        loff_t pos = offset;
        ssize_t n = kernel_read(reader->file, buf, len, &pos);
        if (n < 0 || (size_t)n != len)
            return -EIO;
    }

    return 0;
}

static bool ksu_module_is_vr(struct ksu_elf_reader *reader)
{
    Elf64_Ehdr ehdr;
    Elf64_Shdr *shdrs = NULL;
    char *shstrtab = NULL;
    char *modinfo = NULL;
    Elf64_Shdr *shstr_sh;
    Elf64_Shdr *modinfo_sh = NULL;
    unsigned int shnum;
    unsigned int i;
    unsigned long shtab_bytes;
    bool is_vr = false;

    if (ksu_reader_read(reader, 0, &ehdr, sizeof(ehdr)))
        return false;

    if (memcmp(ehdr.e_ident, ELFMAG, SELFMAG) != 0)
        return false;
    if (ehdr.e_ident[EI_CLASS] != ELFCLASS64)
        return false;
    if (ehdr.e_type != ET_REL)
        return false;
    if (ehdr.e_shentsize != sizeof(Elf64_Shdr))
        return false;

    shnum = ehdr.e_shnum;
    if (shnum == 0 || shnum > KSU_MAX_SECTIONS)
        return false;
    if (ehdr.e_shstrndx >= shnum)
        return false;

    shtab_bytes = (unsigned long)shnum * sizeof(Elf64_Shdr);
    shdrs = kmalloc(shtab_bytes, GFP_KERNEL);
    if (!shdrs)
        return false;
    if (ksu_reader_read(reader, ehdr.e_shoff, shdrs, shtab_bytes))
        goto out;

    shstr_sh = &shdrs[ehdr.e_shstrndx];
    if (shstr_sh->sh_size == 0 || shstr_sh->sh_size > KSU_MAX_SHSTRTAB)
        goto out;
    shstrtab = kmalloc(shstr_sh->sh_size, GFP_KERNEL);
    if (!shstrtab)
        goto out;
    if (ksu_reader_read(reader, shstr_sh->sh_offset, shstrtab, shstr_sh->sh_size))
        goto out;

    for (i = 0; i < shnum; i++) {
        Elf64_Shdr *sh = &shdrs[i];
        const char *name;
        unsigned long remaining;

        if (sh->sh_name >= shstr_sh->sh_size)
            continue;
        name = shstrtab + sh->sh_name;
        remaining = shstr_sh->sh_size - sh->sh_name;
        if (strnlen(name, remaining) >= remaining)
            continue;
        if (strcmp(name, ".modinfo") == 0) {
            modinfo_sh = sh;
            break;
        }
    }
    if (!modinfo_sh)
        goto out;

    if (modinfo_sh->sh_size == 0 || modinfo_sh->sh_size > KSU_MAX_MODINFO)
        goto out;
    modinfo = kmalloc(modinfo_sh->sh_size, GFP_KERNEL);
    if (!modinfo)
        goto out;
    if (ksu_reader_read(reader, modinfo_sh->sh_offset, modinfo, modinfo_sh->sh_size))
        goto out;

    {
        const char *p = modinfo;
        const char *end = modinfo + modinfo_sh->sh_size;

        while (p < end) {
            const char *nul = memchr(p, '\0', end - p);
            size_t entlen;

            if (!nul)
                break;
            entlen = nul - p;
            if (entlen > 5 && memcmp(p, "name=", 5) == 0) {
                const char *value = p + 5;
                size_t vlen = entlen - 5;

                if (vlen == 2 && value[0] == 'v' && value[1] == 'r')
                    is_vr = true;
                break;
            }
            p = nul + 1;
        }
    }

out:
    kfree(modinfo);
    kfree(shstrtab);
    kfree(shdrs);
    return is_vr;
}

static long (*orig_sys_init_module)(const struct pt_regs *regs);
static long ksu_sys_init_module(const struct pt_regs *regs)
{
    struct ksu_elf_reader reader = {
        .umod = (const char __user *)PT_REGS_PARM1(regs),
        .umod_len = (unsigned long)PT_REGS_PARM2(regs),
        .file = NULL,
        .file_size = 0,
    };

    if (reader.umod && reader.umod_len >= sizeof(Elf64_Ehdr) && ksu_module_is_vr(&reader)) {
        pr_info("init_module_filter: blocked vr (init_module)\n");
        return 0;
    }

    return orig_sys_init_module(regs);
}

static long (*orig_sys_finit_module)(const struct pt_regs *regs);
static long ksu_sys_finit_module(const struct pt_regs *regs)
{
    int fd = (int)PT_REGS_PARM1(regs);
    struct file *file = fget(fd);
    bool is_vr = false;

    if (file) {
        struct ksu_elf_reader reader = {
            .umod = NULL,
            .umod_len = 0,
            .file = file,
            .file_size = i_size_read(file_inode(file)),
        };

        if (reader.file_size >= (loff_t)sizeof(Elf64_Ehdr))
            is_vr = ksu_module_is_vr(&reader);
        fput(file);
    }

    if (is_vr) {
        pr_info("init_module_filter: blocked vr (finit_module)\n");
        return 0;
    }

    return orig_sys_finit_module(regs);
}

void __init ksu_init_module_filter_init(void)
{
#ifdef __aarch64__
    // vr.ko is loaded by vendor init, which is not routed through the
    // tracepoint dispatcher. Patch the syscall table directly so every process
    // hits this filter.
    ksu_syscall_table_hook(__NR_init_module, ksu_sys_init_module, &orig_sys_init_module);
    ksu_syscall_table_hook(__NR_finit_module, ksu_sys_finit_module, &orig_sys_finit_module);
    pr_info("init_module_filter: hooked init_module + finit_module\n");
#else
    pr_info("init_module_filter: skipped (not arm64)\n");
#endif
}

void __exit ksu_init_module_filter_exit(void)
{
#ifdef __aarch64__
    ksu_syscall_table_unhook(__NR_init_module);
    ksu_syscall_table_unhook(__NR_finit_module);
    pr_info("init_module_filter: unhooked\n");
#endif
}
