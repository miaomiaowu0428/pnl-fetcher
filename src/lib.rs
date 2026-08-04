//! 独立 PnL 计算库。
//!
//! 通过 JSON-RPC 拉取指定地址在 [start_ts, end_ts] 时间戳范围内的签名，
//! 逐笔计算余额变化，按 base mint 聚合为各 token 的 PnL
//! （quote 优先级：USD1 → USDC → USDT → SOL/WSOL）。
//!
//! ```ignore
//! use pnl_fetcher::PnlQuery;
//!
//! let report = PnlQuery::new(address, start_ts, end_ts)
//!     .max(1000)
//!     .compute()
//!     .await?;
//! ```

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::info;
use serde::Serialize;
use solana_sdk::pubkey::Pubkey;
use transaction_cache::{get_tx, tx_fetcher_v2::SignatureFetcherBuilder};
use utils::parse_rpc_fetched_json::{BalanceChange, balance_change_of};

/// 单个 token 的 PnL 汇总
#[derive(Serialize, Debug, Clone)]
pub struct TokenPnl {
    pub mint: String,
    pub change_raw: i128,
    pub change_ui: f64,
    pub decimals: u8,
    pub tx_count: usize,
}

impl std::fmt::Display for TokenPnl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:+.6} {} txs {}", self.change_ui, self.tx_count, self.mint)
    }
}

/// PnL 聚合报告
#[derive(Serialize, Debug, Clone)]
pub struct PnlReport {
    pub scanned: usize,
    pub matched_txs: usize,
    pub pnl: Vec<TokenPnl>,
}

impl PnlReport {
    /// 总净盈亏（quote 单位，raw lamports/最小单位）
    pub fn total_raw(&self) -> i128 {
        self.pnl.iter().map(|t| t.change_raw).sum()
    }

    /// 总净盈亏（UI 单位）
    pub fn total_ui(&self) -> f64 {
        self.pnl.iter().map(|t| t.change_ui).sum()
    }
}

/// 计算错误
#[derive(Debug)]
pub enum PnlError {
    /// 签名拉取失败
    SignatureFetch(String),
    /// 单笔交易拉取失败
    TxFetch(String),
    /// 余额变化解析失败
    ParseBalance(String),
}

impl std::fmt::Display for PnlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PnlError::SignatureFetch(e) => write!(f, "signature fetch error: {e}"),
            PnlError::TxFetch(e) => write!(f, "tx fetch error: {e}"),
            PnlError::ParseBalance(e) => write!(f, "balance parse error: {e}"),
        }
    }
}

impl std::error::Error for PnlError {}

/// 查询配置（builder 风格）
#[derive(Debug, Clone)]
pub struct PnlQuery {
    address: Pubkey,
    /// 起始时间戳（unix 秒，包含）
    start_ts: u64,
    /// 结束时间戳（unix 秒，包含）
    end_ts: u64,
    /// 最大扫描交易数（None = 不限制）
    max: Option<usize>,
}

impl PnlQuery {
    /// 新建查询：计算 [start_ts, end_ts]（unix 秒，含边界）范围内的 PnL
    pub fn new(address: Pubkey, start_ts: u64, end_ts: u64) -> Self {
        Self {
            address,
            start_ts,
            end_ts,
            max: None,
        }
    }

    /// 设置最大扫描交易数（None = 不限制）
    pub fn max(mut self, max: Option<usize>) -> Self {
        self.max = max;
        self
    }

    /// 执行计算
    pub async fn compute(&self) -> Result<PnlReport, PnlError> {
        compute_pnl(self.address, self.start_ts, self.end_ts, self.max).await
    }
}

/// 便捷函数：计算指定地址在 [start_ts, end_ts]（unix 秒，含边界）范围内的 PnL
/// `max` 为最大扫描交易数（None = 不限制）
pub async fn compute_pnl(
    address: Pubkey,
    start_ts: u64,
    end_ts: u64,
    max: Option<usize>,
) -> Result<PnlReport, PnlError> {
    info!(
        "Fetching signatures for {} (ts range [{}, {}], max {:?})",
        address, start_ts, end_ts, max
    );

    // 下界用 max_age 表达：拉取 now - (now - start_ts) 即 start_ts 之后的所有签名
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let max_age_secs = now_secs.saturating_sub(start_ts);

    // builder 默认 max_count=1000，None 时用 usize::MAX 等效不限制
    let max_count = max.unwrap_or(usize::MAX);
    let signatures = SignatureFetcherBuilder::for_address(address)
        .max_age(Duration::from_secs(max_age_secs))
        .max_count(max_count)
        .build()
        .fetch()
        .await
        .map_err(|e| PnlError::SignatureFetch(format!("{:?}", e)))?;

    info!("fetched {} signatures (scanning ...)", signatures.len());

    // 聚合：base_mint -> (quote_change_sum, quote_decimals, tx_count, quote_mint_str)
    let mut agg: HashMap<String, (i128, u8, usize, String)> = HashMap::new();
    let mut scanned = 0usize;
    let mut matched_txs = 0usize;

    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const USD1: &str = "USD1ttGY1N17NEEHLmELoaybftRBUSErhqYiQzvEmuB";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

    fn quote_decimals(mint: &str) -> u32 {
        if mint == USD1 || mint == USDC || mint == USDT {
            6
        } else {
            9
        }
    }

    for sig in signatures {
        scanned += 1;
        let tx = match get_tx(&sig).await {
            Ok(Some(tx)) => tx,
            Ok(None) => continue, // not found
            Err(e) => {
                eprintln!("get_tx error for {}: {:?}", sig, e);
                continue;
            }
        };

        // 上界过滤：block_time 必须在 [start_ts, end_ts] 内（无 block_time 的保留）
        if let Some(bt) = tx.block_time {
            let bt = bt as u64;
            if bt < start_ts || bt > end_ts {
                continue;
            }
        }

        let changes = match balance_change_of(tx).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("parse balance changes failed for {}: {}", sig, e);
                continue;
            }
        };

        // 只保留目标账户的余额变化
        let self_changes: Vec<_> = changes.into_iter().filter(|c| c.owner == address).collect();
        if self_changes.is_empty() {
            continue;
        }

        // 选择 quote mint（USD1 > USDC > USDT > SOL/WSOL，native SOL 与 WSOL 合并）
        let wsol_pub = WSOL.parse().ok();
        let quote_candidates = [USD1, USDC, USDT, WSOL, "DEFAULT_SOL"];

        let find_by_mint = |mint_str: &str| -> Option<BalanceChange> {
            if mint_str == "DEFAULT_SOL" {
                self_changes
                    .iter()
                    .find(|c| c.mint == Pubkey::default())
                    .cloned()
            } else if let Ok(pk) = mint_str.parse() {
                self_changes.iter().find(|c| c.mint == pk).cloned()
            } else {
                None
            }
        };

        let mut selected_quote = Pubkey::default();
        let mut selected_quote_change: i128 = 0;
        for &qc in &quote_candidates {
            if qc == "DEFAULT_SOL" {
                let sol_change = find_by_mint("DEFAULT_SOL");
                let wsol_change = if let Some(pk) = wsol_pub {
                    self_changes.iter().find(|c| c.mint == pk).cloned()
                } else {
                    None
                };
                match (sol_change, wsol_change) {
                    (Some(sol), Some(w)) => {
                        if let Some(combined) = sol.combine(&w) {
                            selected_quote = Pubkey::default();
                            selected_quote_change = combined.change;
                            break;
                        }
                    }
                    (Some(sol), None) => {
                        selected_quote = Pubkey::default();
                        selected_quote_change = sol.change;
                        break;
                    }
                    (None, Some(w)) => {
                        selected_quote = Pubkey::default();
                        selected_quote_change = w.change;
                        break;
                    }
                    (None, None) => {}
                }
            } else if let Ok(pk) = qc.parse::<Pubkey>() {
                if let Some(ch) = self_changes.iter().find(|c| c.mint == pk) {
                    if let Some(wsol_pk) = wsol_pub {
                        if pk == wsol_pk {
                            selected_quote = Pubkey::default();
                        } else {
                            selected_quote = pk;
                        }
                    } else {
                        selected_quote = pk;
                    }
                    selected_quote_change = ch.change;
                    break;
                }
            }
        }

        // 归一化 quote：WSOL <-> native SOL 视为同一种 quote
        let normalized_selected_quote = if let Some(w_pk) = wsol_pub {
            if selected_quote == w_pk || selected_quote == Pubkey::default() {
                Pubkey::default()
            } else {
                selected_quote
            }
        } else {
            selected_quote
        };

        // base mint = 第一个非 quote 的 mint
        let base_mint_opt = self_changes.iter().find_map(|c| {
            let is_quote = if normalized_selected_quote == Pubkey::default() {
                c.mint == Pubkey::default() || (wsol_pub.is_some() && c.mint == wsol_pub.unwrap())
            } else {
                c.mint == normalized_selected_quote
            };
            if !is_quote {
                Some(c.mint)
            } else {
                None
            }
        });

        let Some(base_mint) = base_mint_opt else {
            continue;
        };

        let quote_mint_str = normalized_selected_quote.to_string();
        let entry = agg
            .entry(base_mint.to_string())
            .or_insert((0i128, quote_decimals(&quote_mint_str) as u8, 0usize, quote_mint_str.clone()));
        entry.0 += selected_quote_change;
        entry.1 = quote_decimals(&quote_mint_str) as u8;
        entry.2 += 1;
        entry.3 = quote_mint_str;

        matched_txs += 1;
    }

    let mut pnl_vec = Vec::with_capacity(agg.len());
    for (mint, (change, dec, cnt, quote_mint)) in agg.iter() {
        let ui = (*change as f64) / 10f64.powi(*dec as i32);
        pnl_vec.push(TokenPnl {
            mint: format!("{} (quote={})", mint, quote_mint),
            change_raw: *change,
            change_ui: ui,
            decimals: *dec,
            tx_count: *cnt,
        });
    }

    Ok(PnlReport {
        scanned,
        matched_txs,
        pnl: pnl_vec,
    })
}
