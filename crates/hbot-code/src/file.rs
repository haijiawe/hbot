//! ctx.files 的槽位（C3.file）。
//!
//! 与 kv / sql 同一形状：Mutex 里放可替换的端口 + 声明，每次调用现读，
//! 所以「改了清单但 isolate 没换」不会变成静默失效。

use hbot_core::{
    FileEntry, FileError, FileStore, FilesCapability, ReadFile, normalize_file_path,
    validate_file_list, validate_file_read, validate_file_remove, validate_file_write,
};
use std::sync::{Arc, Mutex, PoisonError};

/// 宿主还没装后端时的统一说明（`HBOT_FILE_UNAVAILABLE` 的后半句）。
pub(crate) const UNASSEMBLED: &str = "宿主还没有可用的文件后端（bot 未启动或项目根不可用）";

/// 清单声明的状态 + 宿主装上的端口。
#[derive(Default)]
struct FileSlotState {
    declared: FilesCapability,
    backend: Option<Arc<dyn FileStore>>,
}

/// 文件能力的槽位。
#[derive(Default)]
pub(crate) struct FileSlot {
    state: Mutex<FileSlotState>,
}

impl FileSlot {
    /// 空槽位：什么都没声明、宿主也没装配。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 整体覆盖声明（每次装配 isolate 时现读清单）。
    pub(crate) fn set_declared(&self, declared: FilesCapability) {
        self.lock().declared = declared;
    }

    /// 装上端口。
    pub(crate) fn set_backend(&self, backend: Arc<dyn FileStore>) {
        self.lock().backend = Some(backend);
    }

    /// 宿主是否已经装好后端。
    pub(crate) fn is_assembled(&self) -> bool {
        self.lock().backend.is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FileSlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 读一个文本文件（`range` 为 None 时从文件头读）。
    pub(crate) fn read(
        &self,
        raw_path: &str,
        max_bytes: usize,
        range: Option<hbot_core::LineRange>,
    ) -> Result<ReadFile, FileError> {
        let path = normalize_file_path(raw_path)?;
        validate_file_read(&path, max_bytes, range)?;
        let backend = self.gate(false)?;
        backend.read(&path, max_bytes, range)
    }

    /// 覆盖写一个文本文件。
    pub(crate) fn write(&self, raw_path: &str, text: &str) -> Result<FileEntry, FileError> {
        let path = normalize_file_path(raw_path)?;
        validate_file_write(&path, text)?;
        let backend = self.gate(true)?;
        backend.write(&path, text)
    }

    /// 删除一个文件或空目录。
    pub(crate) fn remove(&self, raw_path: &str) -> Result<(), FileError> {
        let path = normalize_file_path(raw_path)?;
        validate_file_remove(&path)?;
        let backend = self.gate(true)?;
        backend.remove(&path)
    }

    /// 列一个目录。
    pub(crate) fn list(&self, raw_path: &str, limit: usize) -> Result<Vec<FileEntry>, FileError> {
        let path = normalize_file_path(raw_path)?;
        validate_file_list(limit)?;
        let backend = self.gate(false)?;
        backend.list(&path, limit)
    }

    /// 声明闸 → 装配闸。
    ///
    /// 顺序是刻意的：没声明时报 NOT_DECLARED（用户该去改清单），而不是
    /// UNAVAILABLE（用户会去 IDE 设置里找根本不存在的开关）。
    fn gate(&self, need_write: bool) -> Result<Arc<dyn FileStore>, FileError> {
        let state = self.lock();
        let allowed = if need_write {
            state.declared.allows_write()
        } else {
            state.declared.allows_read()
        };
        if !allowed {
            return Err(FileError::NotDeclared {
                capability: if need_write { "write" } else { "read" }.to_owned(),
            });
        }
        state.backend.clone().ok_or_else(|| FileError::Unavailable {
            reason: UNASSEMBLED.to_owned(),
        })
    }
}

impl std::fmt::Debug for FileSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("FileSlot")
            .field("read", &state.declared.read)
            .field("write", &state.declared.write)
            .field("assembled", &state.backend.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbot_core::FileKind;

    #[derive(Default)]
    struct Recording {
        reads: Mutex<Vec<(String, usize)>>,
        read_ranges: Mutex<Vec<Option<hbot_core::LineRange>>>,
        writes: Mutex<Vec<(String, String)>>,
        removes: Mutex<Vec<String>>,
        lists: Mutex<Vec<(String, usize)>>,
        failure: Mutex<Option<FileError>>,
    }

    impl Recording {
        fn fail(&self, error: FileError) {
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
        }

        fn take_failure(&self) -> Option<FileError> {
            self.failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        }
    }

    impl FileStore for Recording {
        fn read(
            &self,
            path: &str,
            max_bytes: usize,
            range: Option<hbot_core::LineRange>,
        ) -> Result<ReadFile, FileError> {
            self.reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((path.to_owned(), max_bytes));
            self.read_ranges
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(range);
            if let Some(error) = self.take_failure() {
                return Err(error);
            }
            Ok(ReadFile {
                path: path.to_owned(),
                text: "hello".to_owned(),
                bytes: 5,
                truncated: false,
            })
        }

        fn write(&self, path: &str, text: &str) -> Result<FileEntry, FileError> {
            self.writes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((path.to_owned(), text.to_owned()));
            if let Some(error) = self.take_failure() {
                return Err(error);
            }
            Ok(FileEntry::file(path.to_owned(), text.len() as u64))
        }

        fn remove(&self, path: &str) -> Result<(), FileError> {
            self.removes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(path.to_owned());
            match self.take_failure() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn list(&self, path: &str, limit: usize) -> Result<Vec<FileEntry>, FileError> {
            self.lists
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((path.to_owned(), limit));
            if let Some(error) = self.take_failure() {
                return Err(error);
            }
            Ok(vec![FileEntry::dir(path.to_owned())])
        }
    }

    fn slot(declared: FilesCapability) -> (FileSlot, Arc<Recording>) {
        let recording = Arc::new(Recording::default());
        let slot = FileSlot::new();
        slot.set_declared(declared);
        slot.set_backend(Arc::clone(&recording) as Arc<dyn FileStore>);
        (slot, recording)
    }

    #[test]
    fn an_empty_slot_reports_not_declared() {
        let slot = FileSlot::new();
        assert!(matches!(
            slot.read("a.txt", 16, None),
            Err(FileError::NotDeclared { .. })
        ));
    }

    #[test]
    fn a_declared_but_unassembled_slot_reports_unavailable() {
        let slot = FileSlot::new();
        slot.set_declared(FilesCapability {
            read: true,
            write: true,
        });
        assert!(matches!(
            slot.read("a.txt", 16, None),
            Err(FileError::Unavailable { .. })
        ));
    }

    #[test]
    fn a_bad_path_is_reported_before_the_declaration_gate() {
        let slot = FileSlot::new();
        assert!(matches!(
            slot.read("../x", 16, None),
            Err(FileError::Denied { .. })
        ));
    }

    #[test]
    fn read_only_never_reaches_a_write() {
        let (slot, recording) = slot(FilesCapability {
            read: true,
            write: false,
        });
        assert!(matches!(
            slot.write("a.txt", "x"),
            Err(FileError::NotDeclared { .. })
        ));
        assert!(matches!(
            slot.remove("a.txt"),
            Err(FileError::NotDeclared { .. })
        ));
        assert!(recording.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn write_only_cannot_read_or_list() {
        let (slot, _) = slot(FilesCapability {
            read: false,
            write: true,
        });
        assert!(matches!(
            slot.read("a.txt", 16, None),
            Err(FileError::NotDeclared { .. })
        ));
        assert!(matches!(
            slot.list("", 10),
            Err(FileError::NotDeclared { .. })
        ));
    }

    #[test]
    fn a_valid_read_reaches_the_backend_with_the_normalized_path() {
        let (slot, recording) = slot(FilesCapability {
            read: true,
            write: false,
        });
        let file = slot.read("  data/a.txt  ", 128, None).unwrap();
        assert_eq!(file.path, "data/a.txt");
        let reads = recording.reads.lock().unwrap();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0], ("data/a.txt".to_owned(), 128));
    }

    #[test]
    fn a_valid_write_reaches_the_backend() {
        let (slot, recording) = slot(FilesCapability {
            read: false,
            write: true,
        });
        let entry = slot.write("out/a.txt", "hello").unwrap();
        assert_eq!(entry.kind, FileKind::File);
        assert_eq!(entry.size, 5);
        let writes = recording.writes.lock().unwrap();
        assert_eq!(writes[0], ("out/a.txt".to_owned(), "hello".to_owned()));
    }

    #[test]
    fn listing_the_project_root_is_allowed_when_read_is_declared() {
        let (slot, recording) = slot(FilesCapability {
            read: true,
            write: false,
        });
        let entries = slot.list("", 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(recording.lists.lock().unwrap()[0], (String::new(), 10));
    }

    #[test]
    fn a_backend_failure_surfaces_unchanged() {
        let (slot, recording) = slot(FilesCapability {
            read: true,
            write: true,
        });
        recording.fail(FileError::NotFound {
            path: "a.txt".to_owned(),
        });
        let error = slot.read("a.txt", 16, None).unwrap_err();
        assert_eq!(error.code(), "HBOT_FILE_NOT_FOUND");
    }

    #[test]
    fn the_unassembled_hint_names_the_host_side() {
        assert!(UNASSEMBLED.contains("宿主"));
    }

    #[test]
    fn a_line_range_reaches_the_backend_untouched() {
        let slot = FileSlot::new();
        slot.set_declared(FilesCapability {
            read: true,
            write: false,
        });
        let recording = Arc::new(Recording::default());
        let backend: Arc<dyn FileStore> = recording.clone();
        slot.set_backend(backend);
        slot.read("a.txt", 4096, Some(hbot_core::LineRange::new(3, 9)))
            .expect("读应当成功");
        let ranges = recording.read_ranges.lock().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], Some(hbot_core::LineRange::new(3, 9)));
    }

    #[test]
    fn a_reversed_range_is_refused_before_the_backend_is_touched() {
        let slot = FileSlot::new();
        slot.set_declared(FilesCapability {
            read: true,
            write: false,
        });
        let recording = Arc::new(Recording::default());
        let backend: Arc<dyn FileStore> = recording.clone();
        slot.set_backend(backend);
        let error = slot
            .read("a.txt", 4096, Some(hbot_core::LineRange::new(9, 3)))
            .unwrap_err();
        assert!(error.to_string().contains("HBOT_FILE_INVALID"), "{error}");
        assert!(recording.read_ranges.lock().unwrap().is_empty());
    }
}
