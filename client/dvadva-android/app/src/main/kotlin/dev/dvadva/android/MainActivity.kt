package dev.dvadva.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import dev.dvadva.android.session.SessionViewModel
import dev.dvadva.android.ui.DvaDvaApp
import dev.dvadva.android.ui.DvaDvaTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            DvaDvaTheme {
                DvaDvaApp()
            }
        }
    }
}
