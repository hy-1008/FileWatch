/* 权限弹窗的触发规则 —— 单独一个文件，因为它必须能被直接跑真值表。
 *
 * 这段判断很短，但决定的是「该不该打扰用户」以及「会不会把自己重启成死循环」。
 * 放这里而不塞进 app.js 的原因和 wire.js 一样：app.js 一被加载就要摸
 * `window.__TAURI__` 与 `document`，没法在 Node 里单独验证；而下面这几种
 * 「不问」的情形每漏一条，症状都是只在特定机器上才复现的毛病：
 *
 *   · 漏掉 `privileged`     → 已经是管理员的用户每次启动都被追问
 *   · 漏掉 `attempted`      → 提权无效的机器（UAC 关掉）上无限重启
 *   · 漏掉 `ask_admin_on_start` → 用户勾了「不再询问」也照问，勾选框形同虚设
 *
 * 所以这里只放纯函数、不碰 DOM，由 `tools/check-admin.js` 跑真值表。
 */

'use strict';

/**
 * 进入软件时的权限动作。
 *
 * @param {object|null} info  后端 `elevation_info` 的返回
 * @param {object|null} prefs 后端 `gui_prefs` 的返回
 * @returns {'none'|'ask'|'notify-failed'}
 *   `none`            —— 什么都不做（已有权限，或用户关了自动询问）
 *   `ask`             —— 弹权限窗
 *   `notify-failed`   —— 本次启动已经试过提权却仍未生效：**只提示，不弹窗**
 */
function fwAdminBootAction(info, prefs) {
  // 还没问过后端：不弹。宁可漏一次，也不要在状态未知时先糊一个窗上去。
  if (!info) return 'none';
  // 已经有 MFT 直读能力（提权令牌，或 UAC 被关掉从而卷可直接打开）
  if (info.privileged) return 'none';
  // 本次启动已经带着 --elevation-attempted 走过一轮了 —— 再弹就是转圈
  if (info.attempted) return 'notify-failed';
  // 用户自己勾过「下次不再自动询问」
  return prefs && prefs.ask_admin_on_start ? 'ask' : 'none';
}

/**
 * 状态栏权限按钮的 class。
 *
 * 有权限 = 绿色，降级 = 橙色。颜色是这里唯一的"提醒"手段，
 * 所以它和 `privileged` 的绑定关系值得单独钉住。
 */
function fwAdminButtonClass(info) {
  return 'mini perm ' + (info && info.privileged ? 'ok' : 'warn');
}

/** 状态栏权限按钮的文本。 */
function fwAdminButtonText(info) {
  if (!info) return '权限：—';
  return '权限：' + (info.label || '未知');
}
