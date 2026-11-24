package dev.fxharness.app

import android.Manifest
import android.app.Activity
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.graphics.Color
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.view.WindowInsets
import android.webkit.JavascriptInterface
import android.webkit.RenderProcessGoneDetail
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import java.io.File

class MainActivity : Activity() {
    private lateinit var web: WebView
    @Volatile private var daemonPort = -1

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE != 0) {
            WebView.setWebContentsDebuggingEnabled(true)
        }
        web = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            settings.setSupportZoom(false)
            setBackgroundColor(Color.TRANSPARENT)
            addJavascriptInterface(KestrelBridge(this@MainActivity), "Kestrel")
            webViewClient = LocalOnlyClient()
        }
        setContentView(web)
        applySystemBarInsets()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0)
        }
        startForegroundService(Intent(this, FxService::class.java))
        Thread { waitForDaemon() }.start()
    }

    /** Targeting Android 15 draws behind the bars, so the page is padded by what the bars cover. */
    private fun applySystemBarInsets() {
        web.setOnApplyWindowInsetsListener { v, insets ->
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
                v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            } else {
                @Suppress("DEPRECATION")
                v.setPadding(insets.systemWindowInsetLeft, insets.systemWindowInsetTop, insets.systemWindowInsetRight, insets.systemWindowInsetBottom)
            }
            insets
        }
    }

    private fun waitForDaemon() {
        val deadline = System.currentTimeMillis() + START_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            val port = Daemon.verifiedPort(this)
            val token = Daemon.token(this)
            if (port != null && token != null) {
                daemonPort = port
                runOnUiThread { web.loadUrl("http://127.0.0.1:$port/?t=$token") }
                return
            }
            Thread.sleep(POLL_MS)
        }
        runOnUiThread {
            web.loadData("The harness did not start. See fxh.log in the app files.", "text/plain", "utf-8")
        }
    }

    /** The page may only talk to the local daemon, because it can reach the settings bridge. */
    private inner class LocalOnlyClient : WebViewClient() {
        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean =
            !(request.url.host == "127.0.0.1" && request.url.port == daemonPort)

        override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
            runOnUiThread { recreate() }
            return true
        }
    }

    companion object {
        private const val START_TIMEOUT_MS = 60_000L
        private const val POLL_MS = 500L
    }
}

private const val MAX_SHARE_CHARS = 200_000

/** What the page may ask of Android: read settings, open the screens that change them, share text. */
class KestrelBridge(private val activity: Activity) {
    @JavascriptInterface
    fun batteryExempt(): Boolean =
        activity.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(activity.packageName)

    @JavascriptInterface
    fun notificationsEnabled(): Boolean =
        activity.getSystemService(NotificationManager::class.java).areNotificationsEnabled()

    @JavascriptInterface
    fun appVersion(): String = activity.packageManager.getPackageInfo(activity.packageName, 0).versionName ?: ""

    /** True on mobile data or any connection the user pays for by the megabyte. */
    @JavascriptInterface
    fun isMetered(): Boolean =
        activity.getSystemService(android.net.ConnectivityManager::class.java).isActiveNetworkMetered

    /** Installs the update the daemon has already downloaded and checked. Returns a short status. */
    @JavascriptInterface
    fun installUpdate(): String = UpdateInstaller.install(activity)

    @JavascriptInterface
    fun openInstallSettings() = open(
        Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${activity.packageName}")),
        Intent(Settings.ACTION_SETTINGS),
    )

    @JavascriptInterface
    fun quietHours(): Boolean =
        activity.getSharedPreferences(FxService.PREFS, Context.MODE_PRIVATE).getBoolean(FxService.KEY_QUIET, true)

    @JavascriptInterface
    fun setQuietHours(on: Boolean) {
        activity.getSharedPreferences(FxService.PREFS, Context.MODE_PRIVATE).edit().putBoolean(FxService.KEY_QUIET, on).apply()
    }

    /** Opens the share sheet with a text file's contents. Very long text keeps its newest part. */
    @JavascriptInterface
    fun shareText(name: String, text: String) {
        val shared = if (text.length > MAX_SHARE_CHARS) "(older rows left out)\n" + text.takeLast(MAX_SHARE_CHARS) else text
        val send = Intent(Intent.ACTION_SEND).apply {
            type = "text/plain"
            putExtra(Intent.EXTRA_SUBJECT, name)
            putExtra(Intent.EXTRA_TEXT, shared)
        }
        activity.runOnUiThread { activity.startActivity(Intent.createChooser(send, name)) }
    }

    @JavascriptInterface
    fun requestBatteryExemption() = open(
        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${activity.packageName}")),
        Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS),
    )

    @JavascriptInterface
    fun openAppSettings() = open(
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${activity.packageName}")),
        Intent(Settings.ACTION_SETTINGS),
    )

    private fun open(primary: Intent, fallback: Intent) {
        activity.runOnUiThread {
            try {
                activity.startActivity(primary)
            } catch (_: Exception) {
                activity.startActivity(fallback)
            }
        }
    }
}
