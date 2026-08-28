//! 内部状态锁助手：Mutex poisoning 自愈。
//!
//! 内部状态（引擎 / 中继 / 发现 / 后端 / 接收统计）的加锁统一经
//! [`MutexExt::lock_poisoned`]：某线程持锁期间 panic 后锁进入 poisoned 状态，
//! 继续 `unwrap()` 会让应用连环 panic；自愈后内部状态可能不完整，但状态查询
//! / 停止类操作仍可安全继续，避免 GUI 整体崩溃。

pub(crate) trait MutexExt<T> {
    /// 加锁并自愈 poisoned 状态（等效 `unwrap_or_else(|e| e.into_inner())`）。
    fn lock_poisoned(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for std::sync::Mutex<T> {
    fn lock_poisoned(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}
