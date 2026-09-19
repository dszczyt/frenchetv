package com.frenchetv

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.util.Log
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

/**
 * Optional "remember me" storage for operator credentials.
 *
 * The session token in `sessions.json` is deliberately plain (see
 * `core/src/session.rs`) — a token is cookie-equivalent and expires. A password
 * is neither, so it goes through EncryptedSharedPreferences, which seals its
 * contents with a key held in the AndroidKeyStore. The key material never
 * leaves the keystore, so the stored blob is useless if the file is lifted off
 * the device (via backup extraction, or off a rooted box).
 *
 * This is still "the app can decrypt the password on demand", which is what
 * remembering a password means. It protects the file at rest, not against a
 * compromised device.
 *
 * Requires API 23. minSdk is 21, so every entry point degrades to "not stored"
 * on older devices rather than failing — the setup screen hides the toggle in
 * that case.
 */
object CredentialStore {
    private const val TAG = "frenchetv"
    private const val FILE = "frenchetv_credentials"
    private const val KEY_USERNAME = "username"
    private const val KEY_PASSWORD = "password"

    @JvmStatic
    fun isAvailable(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.M

    private fun prefs(context: Context): SharedPreferences? {
        if (!isAvailable()) return null
        return try {
            val masterKey = MasterKey.Builder(context)
                .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
                .build()
            EncryptedSharedPreferences.create(
                context,
                FILE,
                masterKey,
                EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
                EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
            )
        } catch (e: Exception) {
            // A corrupted keystore entry (restored backup, cleared lock screen)
            // makes this throw. Losing a remembered password is recoverable —
            // crashing the app on launch is not.
            Log.w(TAG, "CredentialStore unavailable: ${e.message}")
            null
        }
    }

    @JvmStatic
    fun save(context: Context, username: String, password: String) {
        val p = prefs(context) ?: return
        p.edit().putString(KEY_USERNAME, username).putString(KEY_PASSWORD, password).apply()
    }

    /** Returns "username\npassword", or an empty string when nothing is stored. */
    @JvmStatic
    fun load(context: Context): String {
        val p = prefs(context) ?: return ""
        val u = p.getString(KEY_USERNAME, null) ?: return ""
        val pw = p.getString(KEY_PASSWORD, null) ?: return ""
        return "$u\n$pw"
    }

    @JvmStatic
    fun clear(context: Context) {
        val p = prefs(context) ?: return
        p.edit().remove(KEY_USERNAME).remove(KEY_PASSWORD).apply()
    }
}
