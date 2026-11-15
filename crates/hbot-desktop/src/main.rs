//! 桌面壳二进制入口；逻辑在 [`hbot_desktop::run`]。

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(hbot_desktop::run() as u8)
}
