package com.codearts.checkin

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors

class MainActivity : Activity() {
    private val worker = Executors.newSingleThreadExecutor()
    private val main = Handler(Looper.getMainLooper())
    private lateinit var overview: TextView
    private lateinit var lastRun: TextView
    private lateinit var accountsView: LinearLayout
    private lateinit var folderButton: Button
    private lateinit var refreshButton: Button
    private lateinit var checkinButton: Button
    private var folderUri: Uri? = null
    private var accounts: List<CheckinAccount> = emptyList()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        buildScreen()
        val saved = prefs().getString(CheckinRepository.FOLDER_PREF, null)
        if (saved == null) {
            renderEmpty("请选择内部存储根目录下的 CodeArts 文件夹。")
        } else {
            folderUri = Uri.parse(saved)
            loadFolder()
        }
    }

    private fun buildScreen() {
        val scroll = ScrollView(this).apply { setBackgroundColor(Color.rgb(246, 248, 250)) }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(30), dp(20), dp(32))
        }
        val title = text("每日签到", 26f, Color.rgb(25, 35, 46), bold = true)
        overview = text("正在检查…", 18f, Color.rgb(25, 35, 46), bold = true).apply {
            setPadding(0, dp(24), 0, dp(6))
        }
        val schedule = text("自动检查：启动后约 15 秒首次检查，此后每 30 分钟检查一次", 13f, Color.rgb(95, 105, 115))
        lastRun = text("最近签到：暂无记录", 14f, Color.rgb(80, 90, 100)).apply {
            setPadding(0, dp(12), 0, dp(16))
        }
        val actions = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        folderButton = Button(this).apply {
            text = "选择 CodeArts 文件夹"
            setOnClickListener { chooseFolder() }
        }
        refreshButton = Button(this).apply {
            text = "刷新状态"
            isEnabled = false
            setOnClickListener { refreshStatuses() }
        }
        checkinButton = Button(this).apply {
            text = "立即签到"
            isEnabled = false
            setOnClickListener { runCheckin() }
        }
        actions.addView(refreshButton, LinearLayout.LayoutParams(0, dp(52), 1f).apply { marginEnd = dp(4) })
        actions.addView(checkinButton, LinearLayout.LayoutParams(0, dp(52), 1f).apply { marginStart = dp(4) })
        val folderAction = LinearLayout(this).apply { addView(folderButton, matchWrap()) }
        val section = text("账号状态", 18f, Color.rgb(25, 35, 46), bold = true).apply {
            setPadding(0, dp(24), 0, dp(10))
        }
        accountsView = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }

        root.addView(title, matchWrap())
        root.addView(overview, matchWrap())
        root.addView(schedule, matchWrap())
        root.addView(lastRun, matchWrap())
        root.addView(actions, matchWrap())
        root.addView(folderAction, matchWrap())
        root.addView(section, matchWrap())
        root.addView(accountsView, matchWrap())
        scroll.addView(root)
        setContentView(scroll)
    }

    private fun chooseFolder() {
        startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
            addFlags(
                Intent.FLAG_GRANT_READ_URI_PERMISSION or
                    Intent.FLAG_GRANT_WRITE_URI_PERMISSION or
                    Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION or
                    Intent.FLAG_GRANT_PREFIX_URI_PERMISSION
            )
        }, PICK_FOLDER)
    }

    private fun loadFolder() {
        val uri = folderUri ?: return
        try {
            accounts = CheckinRepository.loadAccounts(this, uri)
            if (accounts.none { it.enabled }) error("accounts.json 中没有启用的账号")
            prefs().edit().putString(CheckinRepository.FOLDER_PREF, uri.toString()).apply()
            AutoCheckinWorker.schedule(this)
            refreshButton.isEnabled = true
            checkinButton.isEnabled = true
            updateLastRun()
            refreshStatuses()
        } catch (e: Exception) {
            accounts = emptyList()
            renderEmpty(e.message ?: "读取 CodeArts 数据失败")
        }
    }

    private fun refreshStatuses() {
        val uri = folderUri ?: return
        setBusy(true)
        overview.text = "正在查询各账号签到状态…"
        worker.execute {
            val result = runCatching { CheckinRepository.refreshAll(this, uri, accounts) }
            main.post {
                setBusy(false)
                result.onSuccess(::renderStatuses).onFailure {
                    overview.text = "查询失败：${it.message ?: "未知错误"}"
                }
            }
        }
    }

    private fun runCheckin() {
        val uri = folderUri ?: return
        setBusy(true)
        overview.text = "正在对所有启用账号签到…"
        worker.execute {
            val results = CheckinRepository.runAll(this, uri, accounts)
            val summary = results.joinToString("\n") { "${it.account}：${it.message}" }
            prefs().edit()
                .putString("last_result", summary.ifEmpty { "没有启用的账号" })
                .putLong("last_result_at", System.currentTimeMillis())
                .putString("last_result_source", "手动签到")
                .apply()
            val updated = runCatching { CheckinRepository.refreshAll(this, uri, accounts) }
            main.post {
                setBusy(false)
                updateLastRun()
                if (updated.isSuccess) renderStatuses(updated.getOrThrow())
                else overview.text = "签到已执行，状态刷新失败：${updated.exceptionOrNull()?.message.orEmpty()}"
            }
        }
    }

    private fun renderStatuses(statuses: List<AccountStatus>) {
        accountsView.removeAllViews()
        val successes = statuses.count { it.ok }
        val signed = statuses.count { it.ok && it.claimedToday }
        overview.text = when {
            statuses.isEmpty() -> "没有启用的账号"
            successes == 0 -> "账号状态查询失败"
            signed == statuses.size -> "今日已签到 · $signed/${statuses.size} 个账号"
            else -> "可签到 · $signed/${statuses.size} 个账号已完成"
        }
        for (status in statuses) {
            val item = LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(14), dp(12), dp(14), dp(12))
                setBackgroundColor(Color.WHITE)
            }
            val heading = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
            }
            val name = text(status.account, 16f, Color.rgb(25, 35, 46), bold = true)
            val badge = text(
                if (!status.ok) "查询失败" else if (status.claimedToday) "今日已签到" else "今日未签到",
                13f,
                if (!status.ok) Color.rgb(185, 60, 54) else if (status.claimedToday) Color.rgb(35, 125, 83) else Color.rgb(175, 111, 18)
            ).apply { gravity = Gravity.END }
            heading.addView(name, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            heading.addView(badge, wrap())
            item.addView(heading, matchWrap())
            if (!status.ok) {
                item.addView(text(status.message, 14f, Color.rgb(150, 55, 48)).apply { setPadding(0, dp(8), 0, 0) }, matchWrap())
            } else {
                if (status.benefitAmount > 0) {
                    item.addView(text("每日奖励：${status.benefitAmount.toInt()} 积分", 14f, Color.rgb(75, 85, 95)).apply {
                        setPadding(0, dp(8), 0, 0)
                    }, matchWrap())
                }
                status.activities.forEach { activity ->
                    val kind = when (activity.type) {
                        "USER_LOGIN", "DAILY_CLAIM" -> "每日签到"
                        "NEW_USER_REGISTER" -> "新用户注册"
                        "INVITE_USER" -> "邀请好友"
                        "STUDENT_CERTIFIED" -> "学生认证"
                        else -> activity.type.ifEmpty { "活动" }
                    }
                    val state = when (activity.status) {
                        "CONFIRMED" -> "已到账"
                        "CLAIMED" -> "已领取"
                        "CONSUME", "CONSUMED" -> "已使用"
                        else -> "待领取"
                    }
                    val reward = if (activity.amount > 0) "${activity.amount.toInt()} 积分" else "—"
                    val row = listOf(activity.title.ifEmpty { kind }, kind, reward, state).joinToString(" · ")
                    item.addView(text(row, 13f, Color.rgb(90, 100, 110)).apply { setPadding(0, dp(6), 0, 0) }, matchWrap())
                }
            }
            val params = matchWrap().apply { bottomMargin = dp(8) }
            accountsView.addView(item, params)
        }
        if (statuses.isEmpty()) accountsView.addView(text("尚无账号", 14f, Color.GRAY), matchWrap())
    }

    private fun renderEmpty(message: String) {
        overview.text = message
        lastRun.text = "最近签到：暂无记录"
        accountsView.removeAllViews()
        refreshButton.isEnabled = false
        checkinButton.isEnabled = false
        accountsView.addView(text("请确保 CodeArts 文件夹中包含 accounts.json 和可选的 auths/ 文件夹。", 14f, Color.rgb(90, 100, 110)), matchWrap())
    }

    private fun updateLastRun() {
        val prefs = prefs()
        val result = prefs.getString("last_result", null)
        val at = prefs.getLong("last_result_at", 0L)
        val source = prefs.getString("last_result_source", "")
        val time = if (at > 0) SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()).format(Date(at)) else ""
        lastRun.text = if (result == null || at == 0L) "最近签到：暂无记录"
        else "最近签到（$source · $time）\n$result"
    }

    private fun setBusy(busy: Boolean) {
        refreshButton.isEnabled = !busy && folderUri != null
        checkinButton.isEnabled = !busy && folderUri != null
        folderButton.isEnabled = !busy
    }

    private fun text(value: String, size: Float, color: Int, bold: Boolean = false) = TextView(this).apply {
        text = value
        textSize = size
        setTextColor(color)
        if (bold) setTypeface(typeface, android.graphics.Typeface.BOLD)
    }

    private fun matchWrap() = LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT)
    private fun wrap() = LinearLayout.LayoutParams(LinearLayout.LayoutParams.WRAP_CONTENT, LinearLayout.LayoutParams.WRAP_CONTENT)
    private fun dp(value: Int) = (value * resources.displayMetrics.density).toInt()
    private fun prefs() = getSharedPreferences("checkin", MODE_PRIVATE)

    @Deprecated("Use the document picker")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == PICK_FOLDER && resultCode == RESULT_OK) {
            val uri = data?.data ?: return
            val flags = data.flags and (Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            contentResolver.takePersistableUriPermission(uri, flags)
            folderUri = uri
            loadFolder()
        }
    }

    override fun onDestroy() {
        worker.shutdownNow()
        super.onDestroy()
    }

    companion object {
        private const val PICK_FOLDER = 1001
    }
}
