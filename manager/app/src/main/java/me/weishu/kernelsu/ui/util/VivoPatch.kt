package me.weishu.kernelsu.ui.util

import java.util.Locale

// This branch intentionally stays on the vivo-only patch flow, so we keep the
// shared boot-family names and command ids centralized here.
internal const val BOOT_IMAGE_KIND_BOOT = "boot"
internal const val BOOT_IMAGE_KIND_INIT_BOOT = "init_boot"
internal const val BOOT_IMAGE_KIND_VENDOR_BOOT = "vendor_boot"
internal const val BOOT_IMAGE_KIND_UNKNOWN = "unknown"

internal const val VIVO_KMI_SUFFIX = "_vivo"
internal const val VIVO_BOOT_PATCH_COMMAND = "boot-patch-vivo"

internal fun isSupportedBootImageKind(kind: String?): Boolean {
    return when (kind) {
        BOOT_IMAGE_KIND_BOOT,
        BOOT_IMAGE_KIND_INIT_BOOT,
        BOOT_IMAGE_KIND_VENDOR_BOOT,
        -> true

        else -> false
    }
}

internal fun isVendorBootTarget(bootImageKind: String?, partition: String?): Boolean {
    return bootImageKind == BOOT_IMAGE_KIND_VENDOR_BOOT || partition == BOOT_IMAGE_KIND_VENDOR_BOOT
}

internal fun isVivoKmi(kmi: String?): Boolean {
    return !kmi.isNullOrBlank() && kmi.endsWith(VIVO_KMI_SUFFIX)
}

internal fun filterVivoKmis(kmis: List<String>): List<String> {
    return kmis.filter(::isVivoKmi)
}

internal fun preferVivoKmi(preferredKmi: String?, currentKmi: String): String? {
    return when {
        isVivoKmi(preferredKmi) -> preferredKmi
        currentKmi.isBlank() -> null
        else -> currentKmi
    }
}

internal fun describeBootTarget(bootImageKind: String?, partition: String?): String {
    return when {
        isVendorBootTarget(bootImageKind, partition) -> "$BOOT_IMAGE_KIND_VENDOR_BOOT.img"
        bootImageKind == BOOT_IMAGE_KIND_INIT_BOOT || partition == BOOT_IMAGE_KIND_INIT_BOOT -> "$BOOT_IMAGE_KIND_INIT_BOOT.img"
        bootImageKind == BOOT_IMAGE_KIND_BOOT || partition == BOOT_IMAGE_KIND_BOOT -> "$BOOT_IMAGE_KIND_BOOT.img"
        else -> "boot image"
    }
}

internal fun resolveBootImageKindForOutput(bootImageKind: String?, partition: String?): String? {
    return when {
        isVendorBootTarget(bootImageKind, partition) -> BOOT_IMAGE_KIND_VENDOR_BOOT
        bootImageKind == BOOT_IMAGE_KIND_INIT_BOOT || partition == BOOT_IMAGE_KIND_INIT_BOOT -> BOOT_IMAGE_KIND_INIT_BOOT
        bootImageKind == BOOT_IMAGE_KIND_BOOT || partition == BOOT_IMAGE_KIND_BOOT -> BOOT_IMAGE_KIND_BOOT
        else -> null
    }
}

internal fun detectBootImageKindByName(fileName: String?): String? {
    val normalized = fileName
        ?.trim()
        ?.substringAfterLast('/')
        ?.substringAfterLast('\\')
        ?.lowercase(Locale.ROOT)
        ?: return null

    return when {
        normalized.endsWith("$BOOT_IMAGE_KIND_VENDOR_BOOT.img") -> BOOT_IMAGE_KIND_VENDOR_BOOT
        normalized.endsWith("$BOOT_IMAGE_KIND_INIT_BOOT.img") -> BOOT_IMAGE_KIND_INIT_BOOT
        normalized.endsWith("$BOOT_IMAGE_KIND_BOOT.img") -> BOOT_IMAGE_KIND_BOOT
        else -> null
    }
}
