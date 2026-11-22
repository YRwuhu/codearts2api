package com.codearts.checkin

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.Settings
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

class StorageActivity : Activity() {
    private var started = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val padding = (28 * resources.displayMetrics.density).toInt()
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = android.view.Gravity.CENTER_VERTICAL
            setPadding(padding, padding, padding, padding)
            addView(TextView(this@StorageActivity).apply {
                text = "访问 CodeArts 文件夹"
                textSize = 24f
            })
            addView(TextView(this@StorageActivity).apply {
                text = "授权后，应用会在内部存储根目录创建 CodeArts，并读取 accounts.json 和 auths/ 中的凭证。账号管理和自动续期也会写回这里。"
                textSize = 16f
                setPadding(0, padding / 2, 0, padding / 2)
            })
            addView(Button(this@StorageActivity).apply {
                text = "授权并启动"
                setOnClickListener {
                    if (Build.VERSION.SDK_INT >= 30) {
                        val appSettings = Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, Uri.parse("package:$packageName"))
                        val settings = if (appSettings.resolveActivity(packageManager) != null) appSettings
                            else Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)
                        try {
                            startActivity(settings)
                        } catch (_: android.content.ActivityNotFoundException) {
                            android.widget.Toast.makeText(this@StorageActivity, "请在系统设置中允许所有文件访问", android.widget.Toast.LENGTH_LONG).show()
                        }
                    } else {
                        requestPermissions(arrayOf(Manifest.permission.WRITE_EXTERNAL_STORAGE), 1)
                    }
                }
            })
        })
    }

    override fun onResume() {
        super.onResume()
        launchIfAllowed()
    }

    override fun onRequestPermissionsResult(code: Int, permissions: Array<String>, results: IntArray) {
        super.onRequestPermissionsResult(code, permissions, results)
        if (code == 2) launchProxy() else launchIfAllowed()
    }

    private fun launchIfAllowed() {
        if (started) return
        val allowed = if (Build.VERSION.SDK_INT >= 30) Environment.isExternalStorageManager()
            else checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED
        if (!allowed) return
        started = true
        val prefs = getSharedPreferences("startup", MODE_PRIVATE)
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED &&
            !prefs.getBoolean("notification_prompted", false)) {
            prefs.edit().putBoolean("notification_prompted", true).apply()
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 2)
        } else {
            launchProxy()
        }
    }

    @SuppressLint("InlinedApi")
    private fun launchProxy() {
        startForegroundService(Intent(this, ProxyService::class.java))
        startActivity(Intent(this, MainActivity::class.java))
        finish()
    }
}
