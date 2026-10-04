//! xssh — an SSH client designed to be driven by AI agents.

// First, so its print!/println!/eprint!/eprintln! shadow std's in every module below.
#[macro_use]
mod out;

mod cli;
mod client;
mod dialog;
mod render;

use clap::Parser;
use cli::Cli;

fn main() {
    xssh_core::api::set_build_id(env!("XSSH_BUILD_ID"));
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            // --help/--version exit 0; every argument error uses the documented usage code.
            std::process::exit(if e.use_stderr() { 64 } else { 0 });
        }
    };
    let json = cli.json;
    // The command dispatcher and engine futures are large; unoptimized builds overflowed the
    // 1 MB Windows main-thread stack and the 2 MB default worker stacks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .thread_stack_size(8 << 20)
        .build()
        .expect("tokio runtime");
    let result = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 << 20)
            .spawn_scoped(s, || rt.block_on(Box::pin(cli::run(cli))))
            .expect("main thread")
            .join()
    });
    let result = match result {
        Ok(r) => r,
        Err(p) => std::panic::resume_unwind(p),
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            if json {
                out::write_stdout(&format!("{}\n", out::json_text(&serde_json::json!({ "error": e }).to_string())));
            } else {
                let code = serde_json::to_value(e.code)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                out::write_stderr(&format!("error[{code}]: {}\n", e.message));
                if let Some(h) = &e.hint {
                    out::write_stderr(&format!("hint: {h}\n"));
                }
            }
            e.code.exit_code()
        }
    };
    // Do not wait for background tasks (e.g. pooled SSH connections) to wind down.
    rt.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}
