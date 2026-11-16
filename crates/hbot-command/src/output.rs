//! 有界输出收集：不许把无界输出读进内存。
//!
//! 为什么要单独两个读者线程：子进程的 stdout/stderr 是两条独立的管道，
//! 如果只在一个线程里读一条、等另一条，另一条写满（Windows 管道默认 64 KiB）
//! 就会把子进程卡死——那是经典的"命令假死"，`AGENTS.md` §3 的
//! "长命令一律重定向日志"就是被它咬过之后写的。
//!
//! 缓冲区是**共享的**（`Arc<Mutex<Vec<u8>>>`），不是等读者线程结束后才交出结果：
//! 这样即使有某个后代进程还攥着管道写端不放（读者线程迟迟不结束），
//! 调用方照样能拿到已经读到的部分输出，不会为了"拿输出"再等一轮超时。

use std::io::{ErrorKind, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hbot_core::OutputStream;

/// 一次读取的块大小（8 KiB：比管道缓冲小，够摊薄锁竞争，也不会浪费内存）。
const CHUNK_BYTES: usize = 8 * 1024;

/// 收集中的有界输出。
pub(crate) struct BoundedOutput {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    overflow: Arc<Mutex<Option<OutputStream>>>,
    finished: Receiver<()>,
}

impl BoundedOutput {
    /// 为两个管道各起一个读者线程。
    ///
    /// 收 `Box<dyn Read + Send>` 而不是具体的 `ChildStdout`/`ChildStderr`：写隔离档的原生
    /// spawn 拿到的管道读端是裸 `HANDLE`（包成 `File`），类型与 `std::process` 的不同。
    pub(crate) fn spawn(
        stdout: Box<dyn Read + Send>,
        stderr: Box<dyn Read + Send>,
        limit: usize,
    ) -> Self {
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let overflow = Arc::new(Mutex::new(None));
        let (sender, finished) = mpsc::channel();

        spawn_reader(
            stdout,
            Arc::clone(&stdout_buf),
            Arc::clone(&overflow),
            limit,
            OutputStream::Stdout,
            sender.clone(),
        );
        spawn_reader(
            stderr,
            Arc::clone(&stderr_buf),
            Arc::clone(&overflow),
            limit,
            OutputStream::Stderr,
            sender,
        );

        Self {
            stdout: stdout_buf,
            stderr: stderr_buf,
            overflow,
            finished,
        }
    }

    /// 已经触发的输出上限（谁先超就报谁）。
    pub(crate) fn overflow_stream(&self) -> Option<OutputStream> {
        *lock(&self.overflow)
    }

    /// 等两个读者线程收尾（超时就返回，缓冲区里的内容照样可用）。
    ///
    /// 为什么允许超时返回：整棵树已经被杀掉，但某个后代进程可能仍攥着管道写端
    /// （例如它 `fork` 之后自己没退出）。此时"等到 EOF"没有意义——把阻塞留给
    /// 调用方换不回任何新字节，该返回的是已经读到的部分输出。
    pub(crate) fn wait_closed(&self, wait: Duration) {
        let deadline = std::time::Instant::now() + wait;
        let mut closed = 0;
        while closed < 2 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.finished.recv_timeout(remaining) {
                Ok(()) => closed += 1,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// 取出输出（UTF-8 有损解码）与触发过上限的流。
    pub(crate) fn into_parts(self) -> (String, String, Option<OutputStream>) {
        let overflow = *lock(&self.overflow);
        let stdout = String::from_utf8_lossy(&lock(&self.stdout)).into_owned();
        let stderr = String::from_utf8_lossy(&lock(&self.stderr)).into_owned();
        (stdout, stderr, overflow)
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    sink: Arc<Mutex<Vec<u8>>>,
    overflow: Arc<Mutex<Option<OutputStream>>>,
    limit: usize,
    stream: OutputStream,
    done: mpsc::Sender<()>,
) {
    thread::spawn(move || {
        let mut chunk = [0u8; CHUNK_BYTES];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let mut buffer = lock(&sink);
                    let room = limit.saturating_sub(buffer.len());
                    if read > room {
                        buffer.extend_from_slice(&chunk[..room]);
                        drop(buffer);
                        let mut flag = lock(&overflow);
                        if flag.is_none() {
                            *flag = Some(stream);
                        }
                        // 不再读：调用方会发现超限并杀树。继续读只会白烧内存。
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                // 管道在进程被杀后可能返回非 EOF 的错误（Windows 上是 ERROR_BROKEN_PIPE
                // 之外的几种）；都按"读完了"处理，缓冲区里的内容仍然有效。
                Err(_) => break,
            }
        }
        // 接收端可能已经走了（调用方超时返回），发送失败不是错误。
        let _ = done.send(());
    });
}

/// 锁中毒不影响数据正确性（我们只做 `extend_from_slice`），取回内部值继续用。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// 用 `Cursor` 当"管道"直接测读者逻辑（真子进程路径由 tests/isolation.rs 覆盖）。
    fn collect_from(bytes: Vec<u8>, limit: usize) -> (String, Option<OutputStream>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let overflow = Arc::new(Mutex::new(None));
        let (sender, receiver) = mpsc::channel();
        spawn_reader(
            Cursor::new(bytes),
            Arc::clone(&sink),
            Arc::clone(&overflow),
            limit,
            OutputStream::Stdout,
            sender,
        );
        receiver.recv().expect("读者线程应结束");
        let text = String::from_utf8_lossy(&lock(&sink)).into_owned();
        let overflow = *lock(&overflow);
        (text, overflow)
    }

    #[test]
    fn output_under_limit_is_kept_whole() {
        let (text, overflow) = collect_from(b"hello".to_vec(), 64);
        assert_eq!(text, "hello");
        assert!(overflow.is_none());
    }

    #[test]
    fn output_over_limit_is_truncated_and_flagged() {
        let (text, overflow) = collect_from(vec![b'x'; 100], 10);
        assert_eq!(text.len(), 10, "缓冲区不得超过上限");
        assert_eq!(overflow, Some(OutputStream::Stdout));
    }

    #[test]
    fn output_exactly_at_limit_is_not_overflow() {
        let (text, overflow) = collect_from(vec![b'x'; 10], 10);
        assert_eq!(text.len(), 10);
        assert!(overflow.is_none(), "恰好等于上限不算超限");
    }
}
