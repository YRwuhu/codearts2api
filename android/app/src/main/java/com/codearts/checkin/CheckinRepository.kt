package com.codearts.checkin

import android.content.Context
import android.net.Uri
import androidx.documentfile.provider.DocumentFile
import org.json.JSONArray
import org.json.JSONObject
import java.math.BigInteger
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import java.security.AlgorithmParameters
import java.security.KeyFactory
import java.security.MessageDigest
import java.security.Signature
import java.security.spec.ECGenParameterSpec
import java.security.spec.ECParameterSpec
import java.security.spec.ECPrivateKeySpec
import java.text.SimpleDateFormat
import java.time.Instant
import java.util.Date
import java.util.Locale
import java.util.TimeZone
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

data class CheckinAccount(val id: String, val ak: String, val sk: String, val oauth: Boolean, val enabled: Boolean)
data class ActivityStatus(val title: String, val type: String, val amount: Double, val status: String)
data class AccountStatus(
    val account: String,
    val ok: Boolean,
    val message: String,
    val claimedToday: Boolean = false,
    val benefitAmount: Double = 0.0,
    val activities: List<ActivityStatus> = emptyList()
)

object CheckinRepository {
    const val FOLDER_PREF = "codearts_folder_uri"
    private const val BASE = "https://snap-access.cn-north-4.myhuaweicloud.com"
    private const val STS = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens"
    private const val CLIENT_ID = "codearts-agent"

    fun loadAccounts(context: Context, treeUri: Uri): List<CheckinAccount> {
        val root = DocumentFile.fromTreeUri(context, treeUri) ?: error("无法访问 CodeArts 文件夹")
        if (!root.name.equals("CodeArts", ignoreCase = true)) error("请选择内部存储根目录下的 CodeArts 文件夹")
        val file = root.findFile("accounts.json") ?: error("文件夹中找不到 accounts.json")
        val json = JSONObject(readText(context, file))
        val rows = json.optJSONArray("accounts") ?: JSONArray()
        return (0 until rows.length()).mapNotNull { index ->
            val row = rows.optJSONObject(index) ?: return@mapNotNull null
            val oauth = row.optBoolean("oauth", false)
            val id = row.optString("id").trim().ifEmpty { "account-${index + 1}" }
            val ak = row.optString("ak").trim()
            val sk = row.optString("sk").trim()
            if (!oauth && (ak.isEmpty() || sk.isEmpty())) return@mapNotNull null
            CheckinAccount(id, ak, sk, oauth, row.optBoolean("enabled", true))
        }
    }

    fun refreshAll(context: Context, treeUri: Uri, accounts: List<CheckinAccount>): List<AccountStatus> =
        accounts.filter { it.enabled }.map { account ->
            try {
                val credential = resolveCredential(context, treeUri, account)
                val activities = fetchActivities(credential)
                val daily = activities.firstOrNull(::isDaily)
                AccountStatus(
                    account = account.id,
                    ok = true,
                    message = if (daily?.let(::isClaimed) == true) "今日已签到" else "今日未签到",
                    claimedToday = daily?.let(::isClaimed) ?: false,
                    benefitAmount = daily?.optDouble("benefitAmount", 0.0) ?: 0.0,
                    activities = activities.map(::toActivityStatus)
                )
            } catch (e: Exception) {
                AccountStatus(account.id, false, e.message ?: "状态查询失败")
            }
        }

    fun runAll(context: Context, treeUri: Uri, accounts: List<CheckinAccount>): List<AccountStatus> =
        accounts.filter { it.enabled }.map { account -> runOne(context, treeUri, account) }

    private fun runOne(context: Context, treeUri: Uri, account: CheckinAccount): AccountStatus {
        return try {
            val credential = resolveCredential(context, treeUri, account)
            val activities = fetchActivities(credential)
            val daily = activities.firstOrNull(::isDaily)
                ?: return AccountStatus(account.id, false, "未找到每日签到活动")
            if (isClaimed(daily)) {
                return AccountStatus(account.id, true, "今日已签到", true, daily.optDouble("benefitAmount", 0.0), activities.map(::toActivityStatus))
            }

            val campaign = daily.optLong("campaignId", daily.optLong("campaign_id", 1L))
            val claim = JSONObject()
                .put("campaignId", campaign)
                .put("channel", "IDE")
                .put("idempotentKey", "claim_${campaign}_${System.currentTimeMillis()}")
                .toString()
            try {
                request(credential, "POST", "$BASE/v1/ops/claim", claim)
            } catch (e: Exception) {
                val message = e.message.orEmpty()
                if (message.contains("尚未到达") || message.contains("已领取")) {
                    return AccountStatus(account.id, true, "今日已签到", true, daily.optDouble("benefitAmount", 0.0), activities.map(::toActivityStatus))
                }
                throw e
            }

            val confirmed = runCatching {
                request(credential, "POST", "$BASE/v1/ops/confirm", JSONObject().put("campaignId", campaign).toString())
            }.isSuccess
            val amount = daily.optDouble("benefitAmount", 0.0)
            val message = when {
                !confirmed -> "领取成功，确认步骤稍后自动到账"
                amount > 0.0 -> "签到成功，获得 ${amount.toInt()} 积分"
                else -> "签到成功"
            }
            AccountStatus(account.id, true, message, true, amount, activities.map(::toActivityStatus))
        } catch (e: Exception) {
            AccountStatus(account.id, false, e.message ?: "签到失败")
        }
    }

    private fun fetchActivities(credential: Credential): List<JSONObject> {
        val result = request(credential, "GET", "$BASE/v1/ops/delivery?channel=IDE")
        val data = result.opt("data")
        val rows = when (data) {
            is JSONObject -> data.optJSONArray("items") ?: JSONArray()
            is JSONArray -> data
            else -> JSONArray()
        }
        return (0 until rows.length()).mapNotNull { rows.optJSONObject(it) }
    }

    private fun isDaily(activity: JSONObject): Boolean =
        activity.optLong("campaignId", activity.optLong("campaign_id", 0L)) == 1L ||
            activity.optString("type").equals("USER_LOGIN", true) ||
            activity.optString("type").equals("DAILY_CLAIM", true)

    private fun isClaimed(activity: JSONObject): Boolean =
        activity.optString("status") in setOf("CLAIMED", "CONFIRMED", "CONSUME", "CONSUMED")

    private fun toActivityStatus(value: JSONObject) = ActivityStatus(
        title = value.optString("title"),
        type = value.optString("type"),
        amount = value.optDouble("benefitAmount", 0.0),
        status = value.optString("status")
    )

    private data class Credential(val account: String, val ak: String, val sk: String, val token: String?)

    private fun resolveCredential(context: Context, treeUri: Uri, account: CheckinAccount): Credential {
        if (!account.oauth) return Credential(account.id, account.ak, account.sk, null)
        val identityFile = findAuthFile(context, treeUri, account.id)
            ?: error("账号 ${account.id} 的 OAuth 凭证不存在：CodeArts/auths/codearts-${safeId(account.id)}.json")
        val identity = JSONObject(readText(context, identityFile))
        val expiration = identity.optString("expiration")
        val expiresAt = runCatching { Instant.parse(expiration).toEpochMilli() }.getOrNull()
        val needsRefresh = expiresAt != null && expiresAt <= System.currentTimeMillis() + 30 * 60 * 1000L
        val resolved = if (needsRefresh) {
            try {
                val fresh = refreshIdentity(identity)
                writeIdentity(context, identityFile, fresh)
                fresh
            } catch (e: Exception) {
                if (expiresAt!! <= System.currentTimeMillis()) throw IllegalStateException("账号 ${account.id} 凭证已过期且续期失败：${e.message}")
                identity
            }
        } else identity
        val ak = resolved.optString("access_key_id")
        val sk = resolved.optString("secret_access_key")
        val token = resolved.optString("security_token")
        if (ak.isEmpty() || sk.isEmpty() || token.isEmpty()) error("账号 ${account.id} 的 OAuth 临时凭证不完整")
        return Credential(account.id, ak, sk, token)
    }

    private fun findAuthFile(context: Context, treeUri: Uri, accountId: String): DocumentFile? {
        val root = DocumentFile.fromTreeUri(context, treeUri) ?: return null
        val auths = root.findFile("auths") ?: return null
        return auths.findFile("codearts-${safeId(accountId)}.json")
    }

    private fun safeId(value: String) = value.map { if (it.isLetterOrDigit() || it == '-' || it == '_') it else '_' }.joinToString("")

    private fun refreshIdentity(identity: JSONObject): JSONObject {
        val key = identity.optJSONObject("dpop_private_key") ?: error("缺少 DPoP 私钥，无法续期，请重新登录")
        val refreshToken = identity.optString("refresh_token")
        if (refreshToken.isEmpty()) error("缺少 refresh_token，请重新登录")
        val codeVerifier = identity.optString("code_verifier")
        val proof = dpopProof(key)
        val form = listOf(
            "client_id" to CLIENT_ID,
            "code_verifier" to codeVerifier,
            "grant_type" to "refresh_token",
            "refresh_token" to refreshToken
        ).joinToString("&") { "${formEncode(it.first)}=${formEncode(it.second)}" }
        val response = httpRequest(STS, "POST", form.toByteArray(Charsets.UTF_8), mapOf(
            "Content-Type" to "application/x-www-form-urlencoded",
            "Accept" to "application/json",
            "DPoP" to proof
        ))
        val payload = JSONObject(response)
        val credentials = payload.optJSONObject("credentials")
        val legacy = payload.optJSONObject("credential")
        val access = credentials?.optString("access_key_id").orEmpty().ifEmpty { legacy?.optString("access").orEmpty() }
        val secret = credentials?.optString("secret_access_key").orEmpty().ifEmpty { legacy?.optString("secret").orEmpty() }
        val token = credentials?.optString("security_token").orEmpty().ifEmpty { legacy?.optString("securitytoken").orEmpty() }
        val expiration = credentials?.optString("expiration").orEmpty().ifEmpty { legacy?.optString("expires_at").orEmpty() }
        if (access.isEmpty() || secret.isEmpty() || token.isEmpty()) {
            error("续期响应缺少凭证：${payload.optString("error_msg", response.take(180))}")
        }
        val updated = JSONObject(identity.toString())
            .put("access_key_id", access)
            .put("secret_access_key", secret)
            .put("security_token", token)
            .put("expiration", expiration)
            .put("updated_at", System.currentTimeMillis() / 1000)
        payload.optString("refresh_token").takeIf { it.isNotEmpty() }?.let { updated.put("refresh_token", it) }
        return updated
    }

    private fun dpopProof(key: JSONObject): String {
        val x = key.getString("x")
        val y = key.getString("y")
        val d = urlBase64Decode(key.getString("d"))
        val params = AlgorithmParameters.getInstance("EC").apply { init(ECGenParameterSpec("secp256r1")) }
            .getParameterSpec(ECParameterSpec::class.java)
        val privateKey = KeyFactory.getInstance("EC").generatePrivate(ECPrivateKeySpec(BigInteger(1, d), params))
        val header = JSONObject()
            .put("alg", "ES256")
            .put("typ", "dpop+jwt")
            .put("jwk", JSONObject().put("kty", "EC").put("crv", "P-256").put("x", x).put("y", y))
        val payload = JSONObject()
            .put("htm", "POST")
            .put("htu", STS)
            .put("iat", System.currentTimeMillis() / 1000)
            .put("jti", randomHex(16))
        val input = "${urlBase64(header.toString().toByteArray())}.${urlBase64(payload.toString().toByteArray())}"
        val signer = Signature.getInstance("SHA256withECDSA")
        signer.initSign(privateKey)
        signer.update(input.toByteArray(Charsets.US_ASCII))
        val rawSignature = derToJose(signer.sign())
        return "$input.${urlBase64(rawSignature)}"
    }

    private fun derToJose(der: ByteArray): ByteArray {
        var offset = 2
        if ((der[1].toInt() and 0xff) > 0x80) offset += (der[1].toInt() and 0xff) - 0x80
        fun integer(): ByteArray {
            require(der[offset++] == 0x02.toByte())
            val length = der[offset++].toInt() and 0xff
            var start = offset
            offset += length
            while (start < offset - 1 && der[start] == 0.toByte()) start++
            val value = der.copyOfRange(start, offset)
            require(value.size <= 32)
            return ByteArray(32 - value.size) + value
        }
        val r = integer()
        val s = integer()
        return r + s
    }

    private fun writeIdentity(context: Context, file: DocumentFile, identity: JSONObject) {
        context.contentResolver.openOutputStream(file.uri, "wt")?.bufferedWriter()?.use {
            it.write(identity.toString(2))
        } ?: error("无法保存更新后的 OAuth 凭证")
    }

    private fun readText(context: Context, file: DocumentFile): String =
        context.contentResolver.openInputStream(file.uri)?.bufferedReader()?.use { it.readText() }
            ?: error("无法读取 ${file.name ?: "文件"}")

    private fun request(credential: Credential, method: String, url: String, body: String? = null): JSONObject {
        val payload = body?.toByteArray(Charsets.UTF_8) ?: ByteArray(0)
        val date = SimpleDateFormat("yyyyMMdd'T'HHmmss'Z'", Locale.US).apply { timeZone = TimeZone.getTimeZone("UTC") }.format(Date())
        val parsed = URL(url)
        val headers = sortedMapOf(
            "accept" to "application/json",
            "agent-type" to "PromptCenter",
            "content-type" to "application/json",
            "host" to parsed.host,
            "x-language" to "zh-cn"
        )
        credential.token?.takeIf { it.isNotEmpty() }?.let { headers["x-security-token"] = it }
        val payloadHash = sha256(payload)
        headers["x-sdk-content-sha256"] = payloadHash
        headers["x-sdk-date"] = date
        val signedHeaders = headers.keys.joinToString(";")
        val canonicalHeaders = headers.entries.joinToString("") { "${it.key}:${it.value.trim()}\r\n" }
        val query = parsed.query?.split("&")?.map {
            val pair = it.split("=", limit = 2)
            encodeQuery(URLDecoderCompat.decode(pair[0])) + "=" + encodeQuery(URLDecoderCompat.decode(pair.getOrElse(1) { "" }))
        }?.sorted()?.joinToString("&").orEmpty()
        val path = if (parsed.path.endsWith("/")) parsed.path else "${parsed.path}/"
        val canonical = "$method\r\n$path\r\n$query\r\n$canonicalHeaders\r\n$signedHeaders\r\n$payloadHash"
        val signature = hmac(credential.sk, "SDK-HMAC-SHA256\r\n$date\r\n${sha256(canonical.toByteArray(Charsets.UTF_8))}")
        val authorization = "SDK-HMAC-SHA256 Access=${credential.ak}, SignedHeaders=$signedHeaders, Signature=$signature"
        val requestHeaders = headers.filterKeys { it != "host" }.mapKeys { (name, _) ->
            name.split('-').joinToString("-") { it.replaceFirstChar(Char::uppercase) }
        } + ("Authorization" to authorization)
        val response = httpRequest(url, method, payload, requestHeaders)
        val json = JSONObject(response)
        if (json.has("code") && json.optInt("code") != 0) error("${json.optString("message")} (code=${json.optInt("code")})")
        return json
    }

    private fun httpRequest(url: String, method: String, payload: ByteArray, headers: Map<String, String>): String {
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
            requestMethod = method
            connectTimeout = 30000
            readTimeout = 30000
            headers.forEach { (name, value) -> setRequestProperty(name, value) }
            if (payload.isNotEmpty()) {
                doOutput = true
                outputStream.use { it.write(payload) }
            }
        }
        try {
            val code = connection.responseCode
            val stream = if (code in 200..299) connection.inputStream else connection.errorStream
            val text = stream?.use { it.readBytes().toString(Charsets.UTF_8) }.orEmpty()
            if (code !in 200..299) error("HTTP $code: ${text.take(220)}")
            return text
        } finally {
            connection.disconnect()
        }
    }

    private fun sha256(bytes: ByteArray): String = MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it.toInt() and 0xff) }

    private fun hmac(key: String, value: String): String {
        val mac = Mac.getInstance("HmacSHA256")
        mac.init(SecretKeySpec(key.toByteArray(Charsets.UTF_8), "HmacSHA256"))
        return mac.doFinal(value.toByteArray(Charsets.UTF_8)).joinToString("") { "%02x".format(it.toInt() and 0xff) }
    }

    private fun randomHex(bytes: Int) = ByteArray(bytes).also { java.security.SecureRandom().nextBytes(it) }
        .joinToString("") { "%02x".format(it.toInt() and 0xff) }

    private fun urlBase64(bytes: ByteArray) = android.util.Base64.encodeToString(bytes, android.util.Base64.URL_SAFE or android.util.Base64.NO_WRAP or android.util.Base64.NO_PADDING)
    private fun urlBase64Decode(value: String) = android.util.Base64.decode(value, android.util.Base64.URL_SAFE or android.util.Base64.NO_WRAP or android.util.Base64.NO_PADDING)
    private fun formEncode(value: String) = URLEncoder.encode(value, "UTF-8")
    private fun encodeQuery(value: String) = URLEncoder.encode(value, "UTF-8").replace("+", "%20").replace("%7E", "~")
}

private object URLDecoderCompat {
    fun decode(value: String) = java.net.URLDecoder.decode(value.replace("+", "%2B"), "UTF-8")
}
