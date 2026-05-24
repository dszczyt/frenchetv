package com.frenchetv

import com.google.androidgamesdk.GameActivity

/**
 * MainActivity — thin wrapper around GameActivity that hosts the Rust/egui UI.
 *
 * All application logic runs in Rust via the android_main() entry point in
 * libui_android.so. GameActivity handles the Surface lifecycle, keyboard and
 * D-pad input, and passes events to the native code via the android-activity
 * (game-activity feature) bridge.
 *
 * The activity is always landscape and full-screen — correct for Android TV
 * and FireTV where there is no system chrome.
 */
class MainActivity : GameActivity()
