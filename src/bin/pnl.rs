//! PnL 计算 CLI（pnl-fetcher 库的示例用法）
//!
//! Usage:
//!   cargo run -p pnl-fetcher --bin pnl -- --address <ADDRESS> [--hours 24] [--max 1000] [--json]

use dotenvy::dotenv;
use log::info;
use pnl_fetcher::PnlQuery;
use solana_sdk::pubkey::Pubkey;
use std::env;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use utils::init_logger;

fn usage_and_exit() -> ! {
    eprintln!("Usage: pnl --address <PUBKEY> [--hours <N>] [--max <N>] [--json]");
    eprintln!("Defaults: --hours 24 --max 1000");
    std::process::exit(2)
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    init_logger();

    let mut args = env::args().skip(1);
    let mut address: Option<String> = None;
    let mut hours: u64 = 24;
    let mut max: Option<usize> = None;
    let mut output_json = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "-a" | "--address" => address = args.next(),
            "-H" | "--hours" => {
                if let Some(v) = args.next() {
                    hours = v.parse().unwrap_or_else(|_| {
                        eprintln!("invalid hours");
                        usage_and_exit()
                    });
                } else {
                    usage_and_exit()
                }
            }
            "-n" | "--max" => {
                if let Some(v) = args.next() {
                    max = v.parse().ok().or_else(|| {
                        eprintln!("invalid max");
                        usage_and_exit()
                    });
                } else {
                    usage_and_exit()
                }
            }
            "--json" => output_json = true,
            "-h" | "--help" => usage_and_exit(),
            _ => {
                eprintln!("Unknown arg: {}", a);
                usage_and_exit()
            }
        }
    }

    let address = match address {
        Some(a) => a,
        None => {
            eprintln!("missing --address");
            usage_and_exit()
        }
    };

    let target = match Pubkey::from_str(&address) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("invalid pubkey: {}", e);
            std::process::exit(2);
        }
    };

    // --hours 转时间戳范围：[now - hours, now]
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let start_ts = now_secs.saturating_sub(hours * 3600);
    let end_ts = now_secs;

    info!("Fetching signatures for {} (last {} hours, max {:?})", target, hours, max);

    let report = match PnlQuery::new(target, start_ts, end_ts).max(max).compute().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("compute error: {}", e);
            std::process::exit(1);
        }
    };

    if output_json {
        match serde_json::to_writer_pretty(std::io::stdout().lock(), &report) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("json write error: {}", e);
                std::process::exit(1);
            }
        }
        println!();
        return;
    }

    info!("PnL report:");
    for t in &report.pnl {
        info!("{}", t);
    }
    info!("scanned: {} | matched: {} | total: {:.6}", report.scanned, report.matched_txs, report.total_ui());
    info!("Hint: run with --json to get machine-readable output");
}
