package dev.tty7.mobile

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  // Edge to edge, the WebView is never resized for the keyboard, and it does
  // not shrink the page's visual viewport either: the page could not tell the
  // keyboard is up, and it would cover the message box. Its height is handed
  // to the page instead (main.ts `native-keyboard`), which then lays out as
  // it does on iOS. The WebView still sees the insets itself.
  override fun onWebViewCreate(webView: WebView) {
    ViewCompat.setOnApplyWindowInsetsListener(webView) { view, insets ->
      val keyboard = insets.getInsets(WindowInsetsCompat.Type.ime()).bottom / view.resources.displayMetrics.density
      webView.evaluateJavascript(
        "window.dispatchEvent(new CustomEvent('native-keyboard', { detail: { height: $keyboard } }))",
        null,
      )
      ViewCompat.onApplyWindowInsets(view, insets)
    }
  }

  // Keeps the connection to `host` up in the background, or stops when it is
  // null (lib.rs `keep_alive`, called as the page moves between screens, so
  // always while the app is in front, where a foreground service may start).
  @Suppress("unused")
  fun keepAlive(host: String?) {
    val service = Intent(this, KeepAliveService::class.java)
    if (host == null) {
      stopService(service)
      return
    }
    askToNotify()
    service.putExtra(KeepAliveService.EXTRA_HOST, host)
    try {
      ContextCompat.startForegroundService(this, service)
    } catch (e: Exception) {
      // Not allowed just now: the connection does without, as it did.
    }
  }

  // The service's notification is only shown with leave to notify (Android
  // 13 on). Asked once, the first time a connection is kept up; the service
  // runs either way.
  private fun askToNotify() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return
    val prefs = getSharedPreferences("tty7", MODE_PRIVATE)
    if (prefs.getBoolean("asked_notify", false)) return
    prefs.edit().putBoolean("asked_notify", true).apply()
    ActivityCompat.requestPermissions(this, arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0)
  }
}
