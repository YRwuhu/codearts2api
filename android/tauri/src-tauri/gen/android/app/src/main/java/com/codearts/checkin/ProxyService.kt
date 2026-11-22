package com.codearts.checkin

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.IBinder
import android.os.Process

class ProxyService : Service() {
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == "com.codearts.checkin.STOP_PROXY") {
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            // The Tauri window and the Rust listener share this process.
            Process.killProcess(Process.myPid())
            return START_NOT_STICKY
        }
        val channel = NotificationChannel("codearts_proxy", "CodeArts 代理", NotificationManager.IMPORTANCE_LOW)
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        val pendingFlags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        val open = PendingIntent.getActivity(this, 1, Intent(this, StorageActivity::class.java), pendingFlags)
        val stop = PendingIntent.getService(this, 2, Intent(this, ProxyService::class.java).apply {
            action = "com.codearts.checkin.STOP_PROXY"
        }, pendingFlags)
        val notification = Notification.Builder(this, channel.id)
            .setSmallIcon(android.R.drawable.stat_notify_sync_noanim)
            .setContentTitle("CodeArts2API")
            .setContentText("保持代理和签到任务在后台运行")
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "停止代理", stop).build())
            .build()
        startForeground(8787, notification)
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
