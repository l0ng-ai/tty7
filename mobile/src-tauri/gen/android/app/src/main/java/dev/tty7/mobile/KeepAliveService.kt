package dev.tty7.mobile

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat

// Keeps the app's connection to a computer up while it is in the background,
// as terminal apps on Android do: a foreground service with its notification,
// and the CPU kept awake enough for the connection's keep-alives. Without it
// the phone dozes, the computer stops hearing from it, and the link drops.
// Runs while a screen holds a connection (MainActivity.keepAlive).
class KeepAliveService : Service() {
  private var wakeLock: PowerManager.WakeLock? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val host = intent?.getStringExtra(EXTRA_HOST) ?: "your computer"
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val channel = NotificationChannel(CHANNEL, "Connection", NotificationManager.IMPORTANCE_LOW).apply {
        description = "Shown while tty7 keeps a connection up in the background"
        setShowBadge(false)
      }
      getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }
    val open = PendingIntent.getActivity(
      this,
      0,
      Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )
    val notification = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(R.drawable.ic_stat_connected)
      .setContentTitle("Connected to $host")
      .setContentText("Kept up while tty7 is in the background")
      .setContentIntent(open)
      .setOngoing(true)
      .setSilent(true)
      .setCategory(NotificationCompat.CATEGORY_SERVICE)
      .build()
    val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
    } else {
      0
    }
    ServiceCompat.startForeground(this, NOTIFICATION, notification, type)
    if (wakeLock == null) {
      wakeLock = getSystemService(PowerManager::class.java)
        .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "tty7:connection")
        .apply { acquire() }
    }
    return START_NOT_STICKY
  }

  // Swiped away from recents, the app is done: nothing is left to keep up.
  override fun onTaskRemoved(rootIntent: Intent?) {
    stopSelf()
  }

  override fun onDestroy() {
    wakeLock?.release()
    wakeLock = null
    super.onDestroy()
  }

  companion object {
    const val EXTRA_HOST = "host"
    private const val CHANNEL = "connection"
    private const val NOTIFICATION = 1
  }
}
