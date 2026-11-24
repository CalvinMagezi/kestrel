package dev.fxharness.app

import android.app.Notification
import android.app.PendingIntent
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import org.json.JSONObject
import java.io.File
import java.util.Calendar

/** Keeps the fxh daemon alive: runs it as a child process and restarts it with backoff. */
class FxService : Service() {
    @Volatile private var stopping = false
    @Volatile private var proc: Process? = null

    override fun onCreate() {
        super.onCreate()
        startForegroundWithNotification()
        Thread(::supervise, "fxh-supervisor").start()
        Thread(::watchAlerts, "fxh-alerts").start()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int) = START_STICKY

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        stopping = true
        proc?.destroy()
        super.onDestroy()
    }

    private fun startForegroundWithNotification() {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.notif_channel), NotificationManager.IMPORTANCE_LOW),
        )
        val n = Notification.Builder(this, CHANNEL)
            .setContentTitle(getString(R.string.notif_title))
            .setContentText(getString(R.string.notif_text))
            .setSmallIcon(R.drawable.ic_stat_kestrel)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIF_ID, n)
        }
    }

    private fun supervise() {
        var backoff = MIN_BACKOFF_MS
        while (!stopping) {
            val started = System.currentTimeMillis()
            val code = runOnce()
            log("fxh exited with $code after ${(System.currentTimeMillis() - started) / 1000}s")
            if (System.currentTimeMillis() - started > HEALTHY_RUN_MS) backoff = MIN_BACKOFF_MS
            if (stopping) break
            Thread.sleep(backoff)
            backoff = (backoff * 2).coerceAtMost(MAX_BACKOFF_MS)
        }
    }

    private fun runOnce(): Int {
        val bin = File(applicationInfo.nativeLibraryDir, "libfxh.so")
        val data = File(filesDir, "data").apply { mkdirs() }
        val logFile = File(filesDir, "fxh.log")
        if (logFile.length() > MAX_LOG_BYTES) logFile.delete()
        return try {
            val p = ProcessBuilder(bin.path, "--data-dir", data.path, "--port", "0", "--app-version", appVersion())
                .directory(filesDir)
                .redirectErrorStream(true)
                .redirectOutput(ProcessBuilder.Redirect.appendTo(logFile))
                .start()
            proc = p
            p.waitFor()
        } catch (e: Exception) {
            log("could not start fxh: $e")
            -1
        }
    }

    /** Turns new setup, trade and data events from the daemon into notifications. */
    private fun watchAlerts() {
        val prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(ALERTS, "Setups and trades", NotificationManager.IMPORTANCE_DEFAULT))
        nm.createNotificationChannel(NotificationChannel(ALERTS_QUIET, "Setups and trades (silent)", NotificationManager.IMPORTANCE_LOW))
        var since = prefs.getLong(KEY_SINCE, -1L)
        while (!stopping) {
            val port = Daemon.verifiedPort(this)
            val token = Daemon.token(this)
            val body = if (port != null && token != null) Daemon.get(port, "/api/events?since=$since", token) else null
            if (body != null) since = handleEvents(body, since, prefs, nm)
            try {
                Thread.sleep(ALERT_POLL_MS)
            } catch (_: InterruptedException) {
                return
            }
        }
    }

    private fun handleEvents(body: String, since: Long, prefs: android.content.SharedPreferences, nm: NotificationManager): Long {
        val j = runCatching { JSONObject(body) }.getOrNull() ?: return since
        val last = j.optLong("last_id", since)
        // A first run only learns where the journal is; a smaller id means the journal was reset.
        if (since < 0 || last < since) return last.also { prefs.edit().putLong(KEY_SINCE, it).apply() }
        val events = j.optJSONArray("events")
        val shown = minOf(events?.length() ?: 0, MAX_NOTIFICATIONS)
        for (i in 0 until shown) {
            val e = events!!.getJSONObject(i)
            val text = e.optString("text")
            val cut = text.indexOf(": ")
            val title = if (e.optString("kind") == "feed") "Market data" else if (cut > 0) text.substring(0, cut) else "Kestrel"
            val bodyText = if (e.optString("kind") == "feed" || cut < 0) text else text.substring(cut + 2)
            notify(nm, e.optLong("id").toInt(), title, bodyText)
        }
        val more = (events?.length() ?: 0) - shown
        if (more > 0) notify(nm, SUMMARY_ID, "Kestrel", "$more more updates. Open the app to read them.")
        return last.also { prefs.edit().putLong(KEY_SINCE, it).apply() }
    }

    private fun notify(nm: NotificationManager, id: Int, title: String, text: String) {
        val quiet = getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean(KEY_QUIET, true) && inQuietHours()
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        val n = Notification.Builder(this, if (quiet) ALERTS_QUIET else ALERTS)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setSmallIcon(R.drawable.ic_stat_kestrel)
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        nm.notify(ALERT_ID_BASE + id % ALERT_ID_SPAN, n)
    }

    private fun inQuietHours(): Boolean {
        val hour = Calendar.getInstance().get(Calendar.HOUR_OF_DAY)
        return hour >= QUIET_FROM || hour < QUIET_TO
    }

    private fun appVersion(): String = packageManager.getPackageInfo(packageName, 0).versionName ?: "0.0.0"

    private fun log(line: String) {
        File(filesDir, "fxh.log").appendText("[service] $line\n")
    }

    companion object {
        const val PREFS = "alerts"
        const val KEY_QUIET = "quiet"
        private const val KEY_SINCE = "since"
        private const val ALERTS = "alerts"
        private const val ALERTS_QUIET = "alerts_quiet"
        private const val ALERT_POLL_MS = 30_000L
        private const val MAX_NOTIFICATIONS = 5
        private const val ALERT_ID_BASE = 1_000
        private const val ALERT_ID_SPAN = 4_000
        private const val SUMMARY_ID = 999
        private const val QUIET_FROM = 22
        private const val QUIET_TO = 6
        private const val CHANNEL = "harness"
        private const val NOTIF_ID = 1
        private const val MIN_BACKOFF_MS = 2_000L
        private const val MAX_BACKOFF_MS = 60_000L
        private const val HEALTHY_RUN_MS = 60_000L
        private const val MAX_LOG_BYTES = 1_000_000L
    }
}
