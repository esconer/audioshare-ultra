package com.audioshare.ultra.ui

import android.content.SharedPreferences

/**
 * Per-host PIN persistence backed by `SharedPreferences`.
 *
 * Switching between multiple PCs (for example Linux <-> Windows) keeps each
 * host's pairing PIN isolated under `host_pin_v1_<host>`, while still
 * migrating a single legacy `pin_code` entry for the previously connected
 * `last_host_addr` so existing single-host users keep their saved PIN.
 */
internal object HostPinStore {
    private const val PREFIX = "host_pin_v1_"
    private const val LEGACY_KEY = "pin_code"
    private const val LAST_HOST_KEY = "last_host_addr"

    private fun normalizeHost(host: String): String = host.trim().lowercase()

    private fun keyFor(host: String): String = PREFIX + normalizeHost(host)

    /**
     * Returns the saved PIN for [host], or `null` if no PIN has ever been
     * stored for this host. Falls back to the legacy global `pin_code` only
     * when [host] matches `last_host_addr` (seamless upgrade from single-PIN
     * versions without leaking one PC's PIN onto a different PC).
     */
    fun getSavedPinForHost(prefs: SharedPreferences, host: String): String? {
        val normHost = normalizeHost(host)
        if (normHost.isEmpty()) return null

        val perHost = prefs.getString(keyFor(normHost), null)
        if (perHost != null) return perHost

        val lastHost = normalizeHost(prefs.getString(LAST_HOST_KEY, "") ?: "")
        if (lastHost.isNotEmpty() && normHost == lastHost && prefs.contains(LEGACY_KEY)) {
            return prefs.getString(LEGACY_KEY, "") ?: ""
        }
        return null
    }

    /**
     * Returns the saved PIN for [host], or `""` when [host] has no saved PIN.
     */
    fun getPin(prefs: SharedPreferences, host: String): String {
        val cleanHost = host.trim()
        if (cleanHost.isEmpty()) {
            return prefs.getString(LEGACY_KEY, "") ?: ""
        }
        return getSavedPinForHost(prefs, cleanHost) ?: ""
    }

    /**
     * Saves [pin] specifically for [host] and updates the legacy `pin_code`
     * key so background components stay aligned with the active host.
     */
    fun savePin(prefs: SharedPreferences, host: String, pin: String) {
        val normHost = normalizeHost(host)
        val cleanPin = pin.trim()
        prefs.edit().apply {
            if (normHost.isNotEmpty()) {
                putString(keyFor(normHost), cleanPin)
            }
            putString(LEGACY_KEY, cleanPin)
        }.apply()
    }
}
