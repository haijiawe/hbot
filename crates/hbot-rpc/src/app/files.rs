//! 编辑器打开过的文件集合。
//!
//! 只记"打开过"与"读到的内容哈希"，用于两件事：
//!
//! 1. `file_write` 的乐观并发：UI 带着 `baseHash` 提交，磁盘哈希变了就拒绝覆盖
//!    （`docs/02-architecture.md` §6.6：宁可让用户重看 diff，也不静默吃掉别人的改动）；
//! 2. 保存前的订阅：文件被 Agent 或外部工具改过时，UI 需要知道（`FileChanged` 事件）。
//!
//! 这里不缓存文件内容——缓存一份内容就有了第二个真源，磁盘才是真源。

use std::collections::BTreeMap;

/// 打开文件登记表：项目相对路径 → 上次读到的哈希。
#[derive(Debug, Default)]
pub struct ActiveFiles {
    opened: BTreeMap<String, String>,
}

impl ActiveFiles {
    /// 记录一个文件的"已知内容哈希"。
    pub fn remember(&mut self, path: impl Into<String>, hash: impl Into<String>) {
        self.opened.insert(path.into(), hash.into());
    }

    /// 该文件上次读到的哈希。
    pub fn hash_of(&self, path: &str) -> Option<&str> {
        self.opened.get(path).map(String::as_str)
    }

    /// 关闭一个文件（不影响磁盘）。
    pub fn forget(&mut self, path: &str) -> Option<String> {
        self.opened.remove(path)
    }

    /// 当前打开的文件（稳定顺序）。
    pub fn paths(&self) -> Vec<&str> {
        self.opened.keys().map(String::as_str).collect()
    }

    /// 打开的文件数。
    pub fn len(&self) -> usize {
        self.opened.len()
    }

    /// 是否没有任何文件打开。
    pub fn is_empty(&self) -> bool {
        self.opened.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_forget() {
        let mut files = ActiveFiles::default();
        assert!(files.is_empty());
        files.remember("src/main.ts", "abc");
        assert_eq!(files.hash_of("src/main.ts"), Some("abc"));
        assert_eq!(files.paths(), vec!["src/main.ts"]);
        assert_eq!(files.forget("src/main.ts").as_deref(), Some("abc"));
        assert!(files.is_empty());
    }

    #[test]
    fn remembering_twice_keeps_one_entry() {
        let mut files = ActiveFiles::default();
        files.remember("a.ts", "1");
        files.remember("a.ts", "2");
        assert_eq!(files.len(), 1);
        assert_eq!(files.hash_of("a.ts"), Some("2"));
    }
}
