package com.mausfer.app

import android.app.Activity
import android.net.Uri
import android.provider.OpenableColumns
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class FileMetadataArgs { lateinit var uri: String }

@TauriPlugin
class FileMetadataPlugin(private val activity: Activity) : Plugin(activity) {
    @Command
    fun displayName(invoke: Invoke) {
        try {
            val uri = Uri.parse(invoke.parseArgs(FileMetadataArgs::class.java).uri)
            var name: String? = if (uri.scheme == "file") java.io.File(uri.path ?: "").name else null
            if (uri.scheme != "file") activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
                val column = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (column >= 0 && cursor.moveToFirst()) name = cursor.getString(column)
            }
            val result = JSObject()
            result.put("name", name ?: "picked.bin")
            invoke.resolve(result)
        } catch (error: Exception) {
            invoke.reject(error.message ?: "Cannot read file name")
        }
    }
}
