package com.mausfer.app

import android.content.ContentValues
import android.content.Context
import android.os.Build
import android.os.Environment
import android.content.pm.PackageManager
import android.media.MediaScannerConnection
import android.webkit.MimeTypeMap
import android.provider.MediaStore
import java.io.File

/**
 * Publishes files received by the Rust core (which writes them into the app's
 * private staging directory) to the public `MediaStore.Downloads` collection.
 *
 * Android scoped storage (Android 10+) forbids plain `File` writes into the
 * public Downloads directory; MediaStore is the supported path and needs no
 * storage permission for files this app creates.
 */
object MediaStorePublisher {

    /**
     * Insert [file] into the public Downloads collection.
     *
     * On success the private staging copy is deleted and the MediaStore path
     * (sync path [MediaStore.Downloads.EXTERNAL_CONTENT_URI] with the new id)
     * is returned. On failure the private file is kept for a retry.
     */
    fun publish(context: Context, file: File): String? {
        if (!file.exists()) return null

        val displayName = file.name
        val mime = mimeOf(displayName)

        if (Build.VERSION.SDK_INT < 29) return publishLegacy(context, file, mime)

        val values = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, displayName)
            put(MediaStore.Downloads.MIME_TYPE, mime)
            put(MediaStore.Downloads.SIZE, file.length())
            put(MediaStore.Downloads.IS_PENDING, 1)
        }

        val collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI
        val resolver = context.contentResolver
        val uri = try { resolver.insert(collection, values) } catch (_: Exception) { null } ?: return null
        try {
            resolver.openOutputStream(uri)?.use { out ->
                file.inputStream().use { it.copyTo(out) }
            } ?: run {
                resolver.delete(uri, null, null)
                return null
            }
            values.clear()
            values.put(MediaStore.Downloads.IS_PENDING, 0)
            if (resolver.update(uri, values, null, null) != 1) {
                resolver.delete(uri, null, null)
                return null
            }
            file.delete()
            return uri.toString()
        } catch (e: Exception) {
            try { resolver.delete(uri, null, null) } catch (_: Exception) {}
            return null
        }
    }

    private fun mimeOf(name: String): String =
        MimeTypeMap.getSingleton().getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase(java.util.Locale.ROOT))
            ?: "application/octet-stream"

    @Suppress("DEPRECATION")
    private fun publishLegacy(context: Context, file: File, mime: String): String? {
        if (context.checkSelfPermission("android.permission.WRITE_EXTERNAL_STORAGE") != PackageManager.PERMISSION_GRANTED) return null
        var destination: File? = null
        return try {
            val directory = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
            directory.mkdirs()
            var index = 0
            while (true) {
                val name = if (index == 0) file.name else {
                    val extension = file.extension.let { if (it.isEmpty()) "" else ".$it" }
                    "${file.nameWithoutExtension} ($index)$extension"
                }
                val candidate = File(directory, name)
                if (candidate.createNewFile()) { destination = candidate; break }
                index++
            }
            val target = requireNotNull(destination)
            target.outputStream().use { output -> file.inputStream().use { it.copyTo(output) } }
            MediaScannerConnection.scanFile(context, arrayOf(target.absolutePath), arrayOf(mime), null)
            file.delete()
            target.absolutePath
        } catch (_: Exception) {
            destination?.delete()
            null
        }
    }

    /**
     * Drain the publish queue: every file under
     * `<privateDownloadDir>/publish-queue/` is inserted into Downloads.
     * Returns the number of successfully published files.
     */
    @Synchronized
    fun drainQueue(context: Context, privateDownloadDir: File): Int {
        val queue = File(privateDownloadDir, "publish-queue")
        if (!queue.exists()) return 0
        var published = 0
        queue.listFiles()?.forEach { f ->
            if (f.isFile && publish(context, f) != null) {
                published++
            }
        }
        return published
    }
}
