/* 打开有界面的浏览器供用户登录一次，之后 cookie 持久化在 profile 目录里。 */
const { chromium } = require('playwright');
const path = require('path');

const PROFILE = process.env.CK_PROFILE || path.join(__dirname, 'profile');
const URL = 'https://devcloud.cn-north-4.huaweicloud.com/chat/home';

(async () => {
  console.log('[login] profile:', PROFILE);
  console.log('[login] 打开浏览器，请在窗口里完成华为云登录（扫码或账号密码均可）');
  console.log('[login] 登录成功后窗口会保持 5 分钟，之后自动关闭并保存 cookie');

  const ctx = await chromium.launchPersistentContext(PROFILE, {
    headless: false,
    args: ['--no-sandbox', '--disable-blink-features=AutomationControlled'],
    viewport: { width: 1280, height: 860 },
  });
  const page = ctx.pages()[0] || await ctx.newPage();
  await page.goto(URL, { waitUntil: 'domcontentloaded', timeout: 60000 });

  const deadline = Date.now() + 5 * 60 * 1000;
  let logged = false;
  while (Date.now() < deadline) {
    await page.waitForTimeout(3000);
    try {
      const state = await page.evaluate(() => {
        const txt = document.body.innerText || '';
        return { loggedIn: !txt.includes('登录华为云') && !txt.includes('登录参与') };
      });
      if (state.loggedIn) { logged = true; break; }
    } catch (_) {}
  }

  if (logged) {
    // 顺便拉一次活动列表确认可用
    try {
      const r = await page.evaluate(async () => {
        const parts = document.cookie.split(';');
        let cftk = '';
        for (const p of parts) {
          const i = p.indexOf('=');
          if (i > -1 && p.slice(0,i).trim() === 'devclouddevuibjtcftk') cftk = decodeURIComponent(p.slice(i+1));
        }
        const H = { 'Accept':'application/json, text/plain, */*','X-Requested-With':'XMLHttpRequest','Agent-Type':'PromptCenter','X-Language':'zh-cn','cftk':cftk };
        const resp = await fetch('/chat/PromptCenterService/v1/ops/delivery?channel=WEB&_='+Date.now(), { credentials:'same-origin', headers:H });
        const t = await resp.text();
        if (!t.trim()) return { ok:false, note:'empty' };
        const j = JSON.parse(t);
        const items = (j.data && j.data.items) || [];
        return { ok:true, count: items.length, daily: items.find(x=>x.type==='USER_LOGIN')?.status || null };
      });
      console.log('[login] 验证结果:', JSON.stringify(r));
    } catch (e) {
      console.log('[login] 验证失败:', e.message);
    }
    console.log('[login] ✅ 登录成功，cookie 已保存到 profile');
  } else {
    console.log('[login] ❌ 超时未检测到登录');
  }

  await ctx.close();
})().catch(e => { console.error('ERR', e.message); process.exit(1); });
