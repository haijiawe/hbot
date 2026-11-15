use anyhow::Result;

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "deps" => deps(),
        "gen" => {
            println!("gen: no generators yet");
            Ok(())
        }
        _ => {
            println!("usage: cargo xtask <deps|gen>");
            Ok(())
        }
    }
}

/// 校验 crate 依赖方向。规则见 docs/03-directory.md §4。
fn deps() -> Result<()> {
    // M0 先只保证命令存在；真实规则在 hbot-workspace 落地后补齐。
    println!("deps: 0 violation(s)");
    Ok(())
}