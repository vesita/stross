// Stross 前端 —— 防火墙自动放行域（script 全局作用域）：
// ufw 自检（缺放行则显示「一键放行」横幅）+ polkit 一键放行（精确端口 × 子网）。

/** 防火墙自检：ufw 入站拦截 Stross 端口时显示「一键放行」横幅（仅 Linux 桌面）。 */
async function checkFirewall(): Promise<void> {
  if (IS_ANDROID) return;
  try {
    const st = (await call('firewall_status')) as FirewallStatus;
    if (st.missing && st.missing.length > 0) {
      $('fw-missing').textContent = st.missing.join('、');
      $('fw-banner').classList.remove('hidden');
    }
  } catch (_) { /* 非 Linux / 未安装 ufw：静默忽略 */ }
}

/** 一键放行：polkit 弹一次系统授权，自动添加精确放行规则。 */
async function allowFirewall(): Promise<void> {
  const btn = $btn('fw-allow-btn');
  btn.disabled = true;
  try {
    await call('firewall_allow');
    $('fw-banner').classList.add('hidden');
  } catch (e) {
    const box = $('grid-error');
    box.textContent = '防火墙放行失败：' + errMsg(e);
    box.classList.remove('hidden');
  } finally {
    btn.disabled = false;
  }
}
