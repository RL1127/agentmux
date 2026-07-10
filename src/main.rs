/// 启动 agentmux，并将命令返回码原样交给操作系统。
fn main() {
    let exit_code = match agentmux::app::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("错误: {error:#}");
            1
        }
    };
    std::process::exit(exit_code);
}
