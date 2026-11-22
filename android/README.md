# CodeArts2API Android

Android 版使用 Rust + Tauri（工程位于 `tauri/src-tauri`）。管理面板复用仓库根目录的 `static/`，代理、账号、OAuth、签到、额度和用量复用电脑版 Rust 库。原生 Kotlin 签到版工程仅保留作旧版参考。

## 安装和使用

1. 安装 APK，打开 CodeArts2API。首次启动按提示授权访问文件，以便 Rust 直接读写内部共享存储根目录的 `CodeArts/`。Android 11 及以上使用系统的「所有文件访问」设置。
2. 可以在面板的「账号」页新增 AK/SK 账号，或通过系统浏览器登录华为云。也可以把电脑版的账号数据复制到以下位置：

   ```text
   内部存储/CodeArts/
   ├── .env                         # 可选
   ├── accounts.json                # 电脑版账号列表；兼容旧名 account.json
   └── auths/
       └── codearts-{账号ID}.json     # OAuth 账号使用；兼容旧目录名 auth/
   ```

   文件格式和对应账号 ID 与电脑版一致。`accounts.json` 优先；同一账号在两个凭证目录都有文件时优先用 `auths/`。应用会把账号调整和续期凭证写回原文件。复制后点「刷新模型」重新加载账号。若凭证已过期且续期失败，在「账号」页点「登录华为云」重新授权。避免手机和电脑同时使用同一份拷贝的 OAuth 续期令牌，尤其是上游轮换令牌时。
3. 在「客户端接入」页复制 Base URL 和本机生成的 API Key。默认地址为 `http://127.0.0.1:8787/v1`。支持 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages 和模型列表；代理接受 `Authorization: Bearer …` 或 `x-api-key`。
4. 如需局域网接入，在 `CodeArts/.env` 写入 `CODEARTS_PROXY_HOST=0.0.0.0`，然后停止并重开应用。其他设备将 Base URL 中的 `127.0.0.1` 换成手机局域网 IP。管理面板仍仅限手机内的 Tauri 会话。局域网连接使用 HTTP，请在可信网络使用。`CODEARTS_PROXY_PORT` 可自定义端口。

应用启动后有常驻通知，切到后台仍可接收代理请求和执行同电脑版的签到检查。点通知的「停止代理」会结束服务；被系统强制停止后需再次打开应用。系统省电设置可能影响后台执行。

## 构建 APK

安装 Tauri CLI 2、JDK 17、Android SDK（平台 36）和 NDK，并设置 `JAVA_HOME`、`ANDROID_HOME`、`NDK_HOME`。从 `android/tauri/` 执行：

```powershell
rustup target add aarch64-linux-android
cargo tauri android build --ci --debug --apk --target aarch64
```

APK 位于 `android/tauri/src-tauri/gen/android/app/build/outputs/apk/`。若需要给模拟器构建，可将目标改成 `x86_64`。
