package me.weishu.kernelsu.ui.component.choosekmidialog

import androidx.compose.runtime.Composable
import me.weishu.kernelsu.ui.LocalUiMode
import me.weishu.kernelsu.ui.UiMode

@Composable
fun ChooseKmiDialog(
    show: Boolean,
    preferredKmi: String? = null,
    currentKmi: String = "",
    onDismissRequest: () -> Unit,
    onSelected: (String?) -> Unit
) {
    when (LocalUiMode.current) {
        UiMode.Miuix -> ChooseKmiDialogMiuix(show, preferredKmi, currentKmi, onDismissRequest, onSelected)
        UiMode.Material -> ChooseKmiDialogMaterial(show, preferredKmi, currentKmi, onDismissRequest, onSelected)
    }
}
