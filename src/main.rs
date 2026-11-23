mod capabilities;
mod config;
mod custom;
mod error;
mod http;
mod imaging;
mod install;
mod mcp;
mod output;
mod presets;
mod providers;
mod runs;
mod task;
mod ui;
mod util;

use std::sync::Arc;

const USAGE: &str = concat!(
    "shenbi — image generation MCP server\n\n",
    "Usage:\n",
    "  shenbi                启动配置界面（默认 http://127.0.0.1:7788）\n",
    "  shenbi mcp            以 stdio 运行 MCP 服务\n",
    "  shenbi ui --port N    仅启动配置界面并指定端口\n",
    "  shenbi install        注册为 MCP 服务（用户级，AI 随用随启）\n",
    "  shenbi install user   注册到 ~/.claude.json（默认）\n",
    "  shenbi install project    注册到当前目录的 .mcp.json\n",
    "  shenbi uninstall      取消注册\n\n",
    "配置文件：~/.shenbi/config.json（可用 SHENBI_CONFIG 覆盖）\n",
);

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("ui");

    if matches!(mode, "-h" | "--help" | "help") {
        println!("{USAGE}");
        return;
    }

    let config_path = config::Config::default_path();
    let config = Arc::new(config::Config::load(config_path.clone()));

    runs::sweep(&config.output_dir_resolved(), config.run_retention_hours).await;

    match mode {
        "mcp" => {
            if config.available_generate_providers().is_empty() {
                eprintln!("shenbi: no provider keys configured; run `shenbi` and add keys in the UI");
            }
            eprintln!(
                "shenbi: MCP over stdio · providers: {} · config: {}",
                config.available_generate_providers().join(", ").if_empty("none"),
                config_path.display()
            );
            mcp::serve_stdio(config).await;
        }
        "ui" | "serve" | "" => {
            let mut port: u16 = 7788;
            if let Some(index) = args.iter().position(|a| a == "--port") {
                if let Some(value) = args.get(index + 1).and_then(|v| v.parse().ok()) {
                    port = value;
                }
            }
            if let Err(err) = ui::serve(config, port).await {
                eprintln!("shenbi: failed to start UI: {err}");
                std::process::exit(1);
            }
        }
        "install" | "uninstall" => {
            let scope = args.get(1).map(String::as_str).unwrap_or("user");
            match install::register(scope, mode == "uninstall") {
                Ok(outcome) => {
                    println!("{} → {}", outcome.detail, outcome.target);
                    if mode == "install" {
                        println!("下次启动 AI 客户端时会自动拉起 shenbi 的 MCP 服务。");
                    }
                }
                Err(err) => {
                    eprintln!("shenbi: {mode} failed: {err}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("shenbi: unknown command '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}
