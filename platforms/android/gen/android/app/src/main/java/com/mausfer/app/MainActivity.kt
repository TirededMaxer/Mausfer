package com.mausfer.app

import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.system.Os
import androidx.activity.OnBackPressedCallback
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit

/**
 * Mausfer Android shell.
 *
 * Extends the Tauri-generated [TauriActivity] so the standard Tauri bootstrap
 * (WebView + Rust library) runs, and additionally:
 *  - sets `MAUSFER_FILES_DIR` / `MAUSFER_DL_PRIVATE_DIR` (required by
 *    `mausfer_core::AndroidPaths` before the Rust core initialises),
 *  - drains the publication queue into `MediaStore.Downloads` on startup and
 *    on resume (files finished while the app was backgrounded are published
 *    when it comes back),
 *  - checks runtime notification permission (PostNotifications) on Android 13+.
 */
class MainActivity : TauriActivity() {
  private val publisher = Executors.newSingleThreadScheduledExecutor()
  private var publicationTask: ScheduledFuture<*>? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    setEnvForCore()
    super.onCreate(savedInstanceState)
    WindowCompat.setDecorFitsSystemWindows(window, false)
    val content = findViewById<android.view.View>(android.R.id.content)
    content.setBackgroundColor(android.graphics.Color.rgb(13, 17, 23))
    WindowCompat.getInsetsController(window, content).apply {
      isAppearanceLightStatusBars = false
      isAppearanceLightNavigationBars = false
    }
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val safe = insets.getInsets(WindowInsetsCompat.Type.systemBars() or
        WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime())
      view.setPadding(safe.left, safe.top, safe.right, safe.bottom)
      WindowInsetsCompat.CONSUMED
    }
    ViewCompat.requestApplyInsets(content)
    // Keep the running transfer session when leaving the root screen.
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        moveTaskToBack(true)
      }
    })
    requestNotificationPermissionIfNeeded()
    if (Build.VERSION.SDK_INT < 29 && checkSelfPermission("android.permission.WRITE_EXTERNAL_STORAGE") != PackageManager.PERMISSION_GRANTED) {
      requestPermissions(arrayOf("android.permission.WRITE_EXTERNAL_STORAGE"), 1002)
    }
  }

  override fun onResume() {
    super.onResume()
    publicationTask?.cancel(false)
    publicationTask = publisher.scheduleWithFixedDelay({
      try { MediaStorePublisher.drainQueue(applicationContext, privateDownloadDir()) } catch (_: Exception) {}
    }, 0, 1, TimeUnit.SECONDS)
  }

  override fun onPause() {
    publicationTask?.cancel(false)
    publicationTask = null
    super.onPause()
  }

  override fun onDestroy() {
    publisher.shutdown()
    super.onDestroy()
  }

  private fun setEnvForCore() {
    val filesDir = filesDir
    val dlDir = privateDownloadDir()
    if (!dlDir.exists()) {
      dlDir.mkdirs()
    }
    // Real process env vars (`Os.setenv`), not JVM system properties, because
    // the Rust core reads them with `std::env::var`.
    val systemName = android.provider.Settings.Global.getString(contentResolver, "device_name")
      ?.takeIf { it.isNotBlank() } ?: Build.MODEL
    Os.setenv("MAUSFER_SYSTEM_DEVICE_NAME", systemName, true)
    Os.setenv("MAUSFER_FILES_DIR", filesDir.absolutePath, true)
    Os.setenv("MAUSFER_DL_PRIVATE_DIR", dlDir.absolutePath, true)
  }

  private fun privateDownloadDir(): File = File(filesDir, "mausfer-downloads")

  private fun requestNotificationPermissionIfNeeded() {
    if (Build.VERSION.SDK_INT >= 33) {
      val permission = "android.permission.POST_NOTIFICATIONS"
      if (checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
        // Ask only once per install (the OS shows the system dialog only
        // once anyway; repeated calls would re-trigger it on every launch).
        val prefs = getSharedPreferences("mausfer", MODE_PRIVATE)
        if (!prefs.getBoolean("notif_asked", false)) {
          prefs.edit().putBoolean("notif_asked", true).apply()
          requestPermissions(arrayOf(permission), 1001)
        }
      } else {
        // Granted: remember it so a later request is never re-asked.
        getSharedPreferences("mausfer", MODE_PRIVATE)
          .edit().putBoolean("notif_asked", true).apply()
      }
    }
  }
}
