package dev.fxharness.app

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import org.json.JSONObject
import java.io.File
import java.security.MessageDigest

/**
 * Hands a downloaded update to Android's installer. The daemon has already checked the file
 * against the release's published checksum; this checks it again, because the file sits in a
 * folder the daemon can write to, and confirms it is this app and a newer version. Android then
 * refuses it unless it is signed with the same key as the installed app.
 */
object UpdateInstaller {
    private const val BUF = 64 * 1024

    fun install(ctx: Context): String {
        val pm = ctx.packageManager
        if (!pm.canRequestPackageInstalls()) return "needs-permission"
        val dir = File(ctx.filesDir, "data/update")
        val apk = File(dir, "kestrel.apk")
        val marker = File(dir, "ready.json")
        if (!apk.exists() || !marker.exists()) return "error: no update has been downloaded"
        val wanted = runCatching { JSONObject(marker.readText()).getString("sha256") }.getOrNull()
            ?: return "error: the update record is unreadable"
        if (sha256(apk) != wanted) return "error: the file changed after it was checked"
        val info = pm.getPackageArchiveInfo(apk.path, 0) ?: return "error: the file is not an app package"
        if (info.packageName != ctx.packageName) return "error: the file is for a different app"
        val installed = pm.getPackageInfo(ctx.packageName, 0).longVersionCode
        if (info.longVersionCode <= installed) return "error: the file is not a newer version"
        return try {
            commit(ctx, apk)
            "started"
        } catch (e: Exception) {
            "error: ${e.message}"
        }
    }

    private fun commit(ctx: Context, apk: File) {
        val installer = ctx.packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
        params.setAppPackageName(ctx.packageName)
        val id = installer.createSession(params)
        installer.openSession(id).use { session ->
            apk.inputStream().use { input ->
                session.openWrite("kestrel.apk", 0, apk.length()).use { out ->
                    input.copyTo(out, BUF)
                    session.fsync(out)
                }
            }
            val result = PendingIntent.getBroadcast(
                ctx, id, Intent(ctx, InstallResultReceiver::class.java),
                PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            session.commit(result.intentSender)
        }
    }

    private fun sha256(f: File): String {
        val md = MessageDigest.getInstance("SHA-256")
        f.inputStream().use { i ->
            val buf = ByteArray(BUF)
            while (true) {
                val n = i.read(buf)
                if (n < 0) break
                md.update(buf, 0, n)
            }
        }
        return md.digest().joinToString("") { "%02x".format(it) }
    }
}

/** Shows Android's own confirmation screen when it asks, and records a failure if it fails. */
class InstallResultReceiver : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                @Suppress("DEPRECATION")
                val confirm = intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
                confirm?.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)?.let(ctx::startActivity)
            }
            PackageInstaller.STATUS_SUCCESS -> Unit
            else -> File(ctx.filesDir, "fxh.log")
                .appendText("[update] install failed: ${intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)}\n")
        }
    }
}

/** After an update replaces the app, start the daemon again without waiting for the owner. */
class PackageReplacedReceiver : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        runCatching { ctx.startForegroundService(Intent(ctx, FxService::class.java)) }
    }
}
