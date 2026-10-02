package dev.tty7.mobile

import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
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
}
