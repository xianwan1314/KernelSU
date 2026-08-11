package me.weishu.kernelsu.ui.util

import java.util.Locale

internal const val BOOT_IMAGE_KIND_BOOT = "boot"
internal const val BOOT_IMAGE_KIND_INIT_BOOT = "init_boot"
internal const val BOOT_IMAGE_KIND_VENDOR_BOOT = "vendor_boot"
internal const val BOOT_IMAGE_KIND_UNKNOWN = "unknown"

internal const val BOOT_PATCH_COMMAND = "boot-patch"
internal const val VENDOR_BOOT_RMVR_COMMAND = "boot-patch-rmvr"

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

internal fun orderSupportedKmis(kmis: List<String>): List<String> {
    return kmis
}

internal fun resolvePreferredKmi(
    preferredKmi: String?,
    currentKmi: String,
    supportedKmis: List<String>
): String? {
    return when {
        !preferredKmi.isNullOrBlank() -> preferredKmi
        currentKmi.isBlank() -> null
        else -> supportedKmis.firstOrNull { it == currentKmi } ?: currentKmi
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
