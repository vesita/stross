"use strict";
// Stross 前端 —— 类型与共享字符串定义（script 全局作用域共享，勿加 import/export）。
//
// 本文件是**唯一**的类型/字符串定义源：所有 interface、标签映射（*_LABELS）、
// 字符串字面量联合与 wire 键常量集中在此，各域文件只消费不重定义
// （docs/layering-architecture.md：前端不持有端口等常量，wire 形状以 Rust 为真源，
// 这里仅做类型镜像 + 展示标签）。加载顺序：types.js 必须先于其它 app/*.js。
// ---------------------------------------------------------------------------
// 标签映射（wire 值 → 中文展示；未知值回退原文）
// ---------------------------------------------------------------------------
/** 可见性中文显示。 */
const VISIBILITY_LABELS = {
    public: '公开',
    confirm: '需确认',
    private: '私密',
};
/** delivery 中文显示。 */
const DELIVERY_LABELS = {
    pull: '拉取',
    push: '推送',
    both: '双向',
};
/** 设备/端点种类中文显示。 */
const DEVICE_KIND_LABELS = {
    screen: '屏幕',
    window: '窗口',
    camera: '摄像头',
    mic: '麦克风',
    systemAudio: '系统声',
    input: '输入',
    clipboard: '剪贴板',
    file: '文件',
    service: '服务',
};
/** 角色英文 → 中文显示（mDNS TXT `roles`）。 */
const ROLE_LABELS = {
    sender: '共享',
    viewer: '接收',
    relay: '中继',
};
/** 查标签：未知 wire 值回退原文（后端枚举可能先于前端扩展）。 */
function labelOf(map, key) {
    return map[key] || key;
}
// ---------------------------------------------------------------------------
// localStorage 键（与 Rust 无关的前端持久化常量）
// ---------------------------------------------------------------------------
const LS_RELAY = 'stross.lastRelay';
const LS_TITLE = 'stross.lastTitle';
const LS_RECENT = 'stross.recentRelays';
const QUALITIES = {
    LOW: { width: 640, height: 360, fps: 24, bitrateKbps: 800 },
    MEDIUM: { width: 1280, height: 720, fps: 30, bitrateKbps: 2500 },
    HIGH: { width: 1920, height: 1080, fps: 30, bitrateKbps: 6000 },
};
// ---------------------------------------------------------------------------
// 端点种类 → 图标名（雪碧图）；与 DEVICE_KIND_LABELS 共用 EndpointKind 键
// ---------------------------------------------------------------------------
/** 设备类型 → 图标名（雪碧图）。 */
const KIND_ICONS = {
    screen: 'monitor',
    window: 'monitor',
    camera: 'camera',
    mic: 'mic',
    systemAudio: 'speaker',
    file: 'download',
};
/** 设备类型 → 图标名（未知类型回退 server）。 */
function deviceKindIcon(kind) {
    return KIND_ICONS[kind] || 'server';
}
