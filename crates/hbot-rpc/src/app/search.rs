//! 正在跑的搜索登记表：让 `search_cancel` 真的能打断一次同步搜索。
//!
//! ## 为什么需要它
//!
//! [`hbot_workspace::Workspace::search`] 是**同步**的（`ignore` 遍历 + 逐文件匹配），
//! 它只认一个 `Arc<AtomicBool>` 取消旗标：置位后，下一次「每个文件 / 每条命中」的
//! 检查就会停下（`crates/hbot-workspace/src/search.rs`）。而 `search_cancel` 是另一条
//! 命令、跑在另一个任务里——两者之间唯一能共享的就是这张表。于是：
//!
//! 1. `search_start` 开跑前 [`begin`](SearchRegistry::begin) 拿到旗标；
//! 2. `search_cancel` 用 [`cancel`](SearchRegistry::cancel) 置位；
//! 3. 搜索结束（正常 / 出错 / 被取消）后 [`finish`](SearchRegistry::finish) 注销。
//!
//! ## 只放"正在跑"的搜索
//!
//! 搜完就删，所以 UI 迟到的取消命令命中不到任何东西——那是**幂等的 no-op**（返回
//! 成功），不是错误：取消一个已经结束的搜索，用户期望的结果与成功一致。反过来，
//! 查询一个已经结束的 ID 也**不会**伪造 `file.search` 事件（事件的唯一来源是搜索
//! 本身跑完时的 `search_start`）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hbot_core::SearchId;

/// 正在跑的搜索 → 取消旗标。
#[derive(Debug, Default)]
pub struct SearchRegistry {
    running: Mutex<BTreeMap<SearchId, Arc<AtomicBool>>>,
}

impl SearchRegistry {
    /// 空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次搜索，返回交给 `Workspace::search` 的取消旗标。
    ///
    /// 必须先登记再开跑：否则取消命令可能落在"还没登记"的空档里，白取消。
    pub fn begin(&self, search_id: SearchId) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.running_map().insert(search_id, Arc::clone(&flag));
        flag
    }

    /// 取消一次搜索；返回是否命中了一个仍在跑的搜索。
    ///
    /// 幂等：重复取消同一个 ID、取消一个已经结束 / 不存在的 ID 都只是返回 `false`。
    pub fn cancel(&self, search_id: SearchId) -> bool {
        match self.running_map().get(&search_id) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// 注销一次搜索；返回之前是否登记过。正常结束、出错、被取消都要调用。
    pub fn finish(&self, search_id: SearchId) -> bool {
        self.running_map().remove(&search_id).is_some()
    }

    /// 取消并注销**全部**搜索（切换 / 关闭项目时用）；返回被取消的数量。
    ///
    /// 切换项目后旧搜索继续跑没有意义：它扫的是上一个项目的目录，结果推给 UI 只会
    /// 制造"打开 B 项目却看到 A 项目命中"的错觉。
    pub fn cancel_all(&self) -> usize {
        let mut running = self.running_map();
        for flag in running.values() {
            flag.store(true, Ordering::SeqCst);
        }
        let count = running.len();
        running.clear();
        count
    }

    /// 正在跑的搜索数（测试与诊断用）。
    pub fn len(&self) -> usize {
        self.running_map().len()
    }

    /// 是否没有正在跑的搜索。
    pub fn is_empty(&self) -> bool {
        self.running_map().is_empty()
    }

    fn running_map(&self) -> MutexGuard<'_, BTreeMap<SearchId, Arc<AtomicBool>>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> SearchId {
        text.parse().expect("搜索 ID 应是合法 UUID")
    }

    fn first() -> SearchId {
        id("11111111-1111-1111-1111-111111111111")
    }

    fn second() -> SearchId {
        id("22222222-2222-2222-2222-222222222222")
    }

    #[test]
    fn begin_registers_and_cancel_flips_the_flag() {
        let registry = SearchRegistry::new();
        let flag = registry.begin(first());
        assert_eq!(registry.len(), 1);
        assert!(!flag.load(Ordering::SeqCst));

        assert!(
            registry.cancel(first()),
            "cancelling a running search must hit"
        );
        assert!(flag.load(Ordering::SeqCst), "cancel must set the flag");
        assert!(
            registry.cancel(first()),
            "cancel is idempotent while running"
        );
    }

    #[test]
    fn finish_unregisters_and_later_cancel_is_a_noop() {
        let registry = SearchRegistry::new();
        let _flag = registry.begin(first());
        assert!(registry.finish(first()));
        assert!(!registry.finish(first()), "finish is idempotent");
        assert!(
            !registry.cancel(first()),
            "an unknown/finished id is a no-op"
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn cancel_all_flags_every_running_search() {
        let registry = SearchRegistry::new();
        let first_flag = registry.begin(first());
        let second_flag = registry.begin(second());

        assert_eq!(registry.cancel_all(), 2);
        assert!(first_flag.load(Ordering::SeqCst));
        assert!(second_flag.load(Ordering::SeqCst));
        assert!(registry.is_empty(), "cancel_all 同时注销");
        assert_eq!(registry.cancel_all(), 0);
    }
}
