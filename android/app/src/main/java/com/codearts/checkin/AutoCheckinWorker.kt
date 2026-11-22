package com.codearts.checkin

import android.content.Context
import android.net.Uri
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters
import java.util.concurrent.TimeUnit

class AutoCheckinWorker(context: Context, params: WorkerParameters) : Worker(context, params) {
    override fun doWork(): Result = try {
        val prefs = applicationContext.getSharedPreferences("checkin", Context.MODE_PRIVATE)
        val uriText = prefs.getString(CheckinRepository.FOLDER_PREF, null) ?: return Result.success()
        val folder = Uri.parse(uriText)
        val accounts = CheckinRepository.loadAccounts(applicationContext, folder).filter { it.enabled }
        val statuses = CheckinRepository.refreshAll(applicationContext, folder, accounts)
        val dueIds = statuses.filter { it.ok && !it.claimedToday }.map { it.account }.toSet()
        if (dueIds.isNotEmpty()) {
            val due = accounts.filter { it.id in dueIds }
            val results = CheckinRepository.runAll(applicationContext, folder, due)
            val summary = results.joinToString("\n") { "${it.account}：${it.message}" }
            prefs.edit()
                .putString("last_result", summary)
                .putLong("last_result_at", System.currentTimeMillis())
                .putString("last_result_source", "自动签到")
                .apply()
        }
        Result.success()
    } catch (_: Exception) {
        Result.retry()
    }

    companion object {
        private const val WORK_NAME = "codearts-periodic-checkin"

        fun schedule(context: Context) {
            val request = PeriodicWorkRequestBuilder<AutoCheckinWorker>(30, TimeUnit.MINUTES)
                .setInitialDelay(15, TimeUnit.SECONDS)
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                .build()
            WorkManager.getInstance(context).enqueueUniquePeriodicWork(
                WORK_NAME,
                ExistingPeriodicWorkPolicy.KEEP,
                request
            )
        }
    }
}
