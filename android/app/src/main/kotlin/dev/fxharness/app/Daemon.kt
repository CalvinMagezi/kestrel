package dev.fxharness.app

import android.content.Context
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.security.SecureRandom

/** Finds the local daemon and checks it really is ours before anything is sent to it. */
object Daemon {
    private const val CONNECT_MS = 1_500
    private const val READ_MS = 5_000
    private const val NONCE_BYTES = 16

    fun token(ctx: Context): String? =
        File(ctx.filesDir, "data/token").takeIf { it.exists() }?.readText()?.trim()?.takeIf { it.length >= 32 }

    private fun port(ctx: Context): Int? =
        File(ctx.filesDir, "data/port").takeIf { it.exists() }?.readText()?.trim()?.toIntOrNull()

    /**
     * The port, but only if whatever answers on it can hash our secret token with a fresh nonce.
     * Another app that grabbed the port cannot, because the token never leaves the app's folder.
     */
    fun verifiedPort(ctx: Context): Int? {
        val port = port(ctx) ?: return null
        val token = token(ctx) ?: return null
        val nonce = ByteArray(NONCE_BYTES).also { SecureRandom().nextBytes(it) }.joinToString("") { "%02x".format(it) }
        val body = get(port, "/api/ping?nonce=$nonce", null) ?: return null
        val proof = runCatching { JSONObject(body).getString("proof") }.getOrNull() ?: return null
        return port.takeIf { proof == sha256Hex(token + nonce) }
    }

    fun get(port: Int, path: String, token: String?): String? = try {
        val c = URL("http://127.0.0.1:$port$path").openConnection() as HttpURLConnection
        c.connectTimeout = CONNECT_MS
        c.readTimeout = READ_MS
        token?.let { c.setRequestProperty("x-fx-token", it) }
        try {
            if (c.responseCode == 200) c.inputStream.bufferedReader().readText() else null
        } finally {
            c.disconnect()
        }
    } catch (_: Exception) {
        null
    }

    private fun sha256Hex(s: String): String =
        MessageDigest.getInstance("SHA-256").digest(s.toByteArray()).joinToString("") { "%02x".format(it) }
}
