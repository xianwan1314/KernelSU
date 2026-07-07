package me.weishu.kernelsu.ui.component.choosekmidialog

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import me.weishu.kernelsu.R
import me.weishu.kernelsu.ui.component.material.SegmentedColumn
import me.weishu.kernelsu.ui.component.material.SegmentedRadioItem
import me.weishu.kernelsu.ui.util.filterVivoKmis
import me.weishu.kernelsu.ui.util.getSupportedKmis
import me.weishu.kernelsu.ui.util.preferVivoKmi

@Composable
fun ChooseKmiDialogMaterial(
    show: Boolean,
    preferredKmi: String? = null,
    currentKmi: String = "",
    onDismissRequest: () -> Unit,
    onSelected: (String?) -> Unit
) {
    if (!show) return

    val supportedKMIs by produceState(initialValue = emptyList()) {
        value = getSupportedKmis()
    }

    val orderedKMIs = remember(supportedKMIs) {
        filterVivoKmis(supportedKMIs)
    }

    val preferred = remember(preferredKmi, currentKmi) {
        preferVivoKmi(preferredKmi, currentKmi)
    }
    val selectedKmi = remember(currentKmi, preferred, orderedKMIs) {
        mutableStateOf(
            orderedKMIs.firstOrNull { it == preferred }
                ?: orderedKMIs.firstOrNull()
                ?: preferred
        )
    }

    AlertDialog(
        onDismissRequest = {
            onDismissRequest()
            selectedKmi.value = preferred
        },
        confirmButton = {
            TextButton(
                onClick = {
                    onSelected(selectedKmi.value)
                    onDismissRequest()
                },
                enabled = orderedKMIs.contains(selectedKmi.value)
            ) {
                Text(stringResource(id = R.string.confirm))
            }
        },
        dismissButton = {
            TextButton(onClick = {
                onDismissRequest()
                selectedKmi.value = preferred
            }) {
                Text(stringResource(id = android.R.string.cancel))
            }
        },
        title = {
            Text(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(bottom = 16.dp),
                text = stringResource(R.string.select_kmi),
                textAlign = TextAlign.Center
            )
        },
        text = {
            SegmentedColumn(
                content = orderedKMIs.map { kmi ->
                    {
                        SegmentedRadioItem(
                            title = kmi,
                            summary = if (kmi == preferred) stringResource(R.string.current_device_kmi) else null,
                            selected = selectedKmi.value == kmi,
                            onClick = { selectedKmi.value = kmi }
                        )
                    }
                }
            )
        }
    )
}
