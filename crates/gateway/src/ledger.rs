//! Spending: one line per agent turn in `.agentsflow/ledger.jsonl`, with
//! totals per department and month kept in memory.

use crate::home::Prices;
use agentsflow_events::AgentKind;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub ts_ms: u64,
    pub month: String,
    pub department: String,
    pub task: String,
    pub run: String,
    pub agent: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
    /// `cli` when the CLI reported the cost, `price` when computed, `none`
    /// when neither was possible (Codex model without a price).
    pub source: String,
}

#[derive(Clone)]
pub struct Ledger {
    inner: Arc<Mutex<Inner>>,
}
struct Inner {
    path: Option<PathBuf>,
    months: HashMap<(String, String), f64>,
    tokens: HashMap<(String, String), Tokens>,
}

/// Tokens of a month: what the dollars stand for, and the only measure of
/// Codex turns without a price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    /// Turns whose dollars are unknown (Codex without a price).
    pub unpriced_input: u64,
    pub unpriced_output: u64,
}

impl Tokens {
    fn add(&mut self, entry: &Entry) {
        self.input += entry.input_tokens;
        self.output += entry.output_tokens;
        if entry.source == "none" {
            self.unpriced_input += entry.input_tokens;
            self.unpriced_output += entry.output_tokens;
        }
    }
}

/// UTC calendar month of a unix time in milliseconds, as `YYYY-MM`.
pub fn month_of(ts_ms: u64) -> String {
    // Civil-from-days (Howard Hinnant), valid for the whole u64 range we use.
    let days = (ts_ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}")
}

/// Token count for people: 950, 12k, 1.6M.
pub fn tokens_short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", (n + 500) / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// "$0.25 · 340k → 3k токенов": dollars and the tokens behind them.
pub fn spent(usd: f64, input: u64, output: u64) -> String {
    if input == 0 && output == 0 {
        return format!("${usd:.2}");
    }
    format!(
        "${usd:.2} · {} → {} токенов",
        tokens_short(input),
        tokens_short(output)
    )
}

/// Dollars for one turn and where the number came from.
pub fn cost(
    kind: AgentKind,
    model: Option<&str>,
    input_tokens: u64,
    output_tokens: u64,
    reported: Option<f64>,
    prices: &Prices,
) -> (f64, &'static str) {
    if let Some(usd) = reported.filter(|usd| usd.is_finite() && *usd >= 0.0) {
        return (usd, "cli");
    }
    if kind == AgentKind::Codex {
        if let Some(price) = model.and_then(|model| prices.codex.get(model)) {
            let usd = (input_tokens as f64 * price.input + output_tokens as f64 * price.output)
                / 1_000_000.0;
            return (usd, "price");
        }
    }
    (0.0, "none")
}

impl Ledger {
    /// In-memory ledger, for tests.
    #[cfg(test)]
    pub fn memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                path: None,
                months: HashMap::new(),
                tokens: HashMap::new(),
            })),
        }
    }

    pub fn open(path: &Path) -> Self {
        let mut months = HashMap::new();
        let mut tokens: HashMap<_, Tokens> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                if let Ok(entry) = serde_json::from_str::<Entry>(line) {
                    let key = (entry.month.clone(), entry.department.clone());
                    tokens.entry(key.clone()).or_default().add(&entry);
                    *months.entry(key).or_default() += entry.usd;
                }
            }
        }
        Self {
            inner: Arc::new(Mutex::new(Inner {
                path: Some(path.to_path_buf()),
                months,
                tokens,
            })),
        }
    }

    /// Records a turn and returns the department total for its month before
    /// and after it.
    pub fn record(&self, entry: Entry) -> (f64, f64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(path) = &inner.path {
            let result = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| {
                    let mut line = serde_json::to_vec(&entry).map_err(std::io::Error::other)?;
                    line.push(b'\n');
                    file.write_all(&line)
                });
            if let Err(error) = result {
                crate::server_diag!("ledger write failed: {error}");
            }
        }
        inner
            .tokens
            .entry((entry.month.clone(), entry.department.clone()))
            .or_default()
            .add(&entry);
        let total = inner
            .months
            .entry((entry.month.clone(), entry.department.clone()))
            .or_default();
        let before = *total;
        *total += entry.usd;
        (before, *total)
    }

    pub fn month_total(&self, month: &str, department: &str) -> f64 {
        self.inner
            .lock()
            .unwrap()
            .months
            .get(&(month.to_owned(), department.to_owned()))
            .copied()
            .unwrap_or(0.0)
    }

    /// Totals of one month per department.
    pub fn month(&self, month: &str) -> HashMap<String, f64> {
        self.inner
            .lock()
            .unwrap()
            .months
            .iter()
            .filter(|((m, _), _)| m == month)
            .map(|((_, dept), usd)| (dept.clone(), *usd))
            .collect()
    }

    /// Tokens of one month per department.
    pub fn month_tokens(&self, month: &str) -> HashMap<String, Tokens> {
        self.inner
            .lock()
            .unwrap()
            .tokens
            .iter()
            .filter(|((m, _), _)| m == month)
            .map(|((_, dept), tokens)| (dept.clone(), *tokens))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::Price;

    #[test]
    fn spent_shows_dollars_and_tokens() {
        assert_eq!(spent(0.0, 0, 0), "$0.00");
        assert_eq!(spent(0.25, 340_400, 3_100), "$0.25 · 340k → 3k токенов");
        assert_eq!(tokens_short(950), "950");
        assert_eq!(tokens_short(1_549_325), "1.5M");
    }

    #[test]
    fn months_are_utc_calendar_months() {
        assert_eq!(month_of(0), "1970-01");
        // 2026-11-01T00:00:00Z
        assert_eq!(month_of(1_793_491_200_000 - 1), "2026-10");
        assert_eq!(month_of(1_793_491_200_000), "2026-11");
        assert_eq!(month_of(951_782_400_000), "2000-02");
    }

    #[test]
    fn cost_prefers_cli_then_price() {
        let mut prices = Prices::default();
        prices.codex.insert(
            "m".into(),
            Price {
                input: 2.0,
                cached_input: None,
                output: 10.0,
            },
        );
        assert_eq!(
            cost(AgentKind::Claude, None, 1, 1, Some(0.5), &prices),
            (0.5, "cli")
        );
        assert_eq!(
            cost(
                AgentKind::Codex,
                Some("m"),
                1_000_000,
                100_000,
                None,
                &prices
            ),
            (3.0, "price")
        );
        assert_eq!(
            cost(AgentKind::Codex, Some("x"), 5, 5, None, &prices),
            (0.0, "none")
        );
    }

    #[test]
    fn ledger_totals_survive_reopen() {
        let path =
            std::env::temp_dir().join(format!("agentsflow-ledger-{}.jsonl", uuid::Uuid::new_v4()));
        let ledger = Ledger::open(&path);
        let entry = |usd| Entry {
            ts_ms: 0,
            month: "2026-11".into(),
            department: "pr".into(),
            task: "t".into(),
            run: "r".into(),
            agent: "a".into(),
            model: None,
            input_tokens: 1000,
            output_tokens: 10,
            usd,
            source: if usd > 0.0 { "cli" } else { "none" }.into(),
        };
        assert_eq!(ledger.record(entry(1.0)), (0.0, 1.0));
        assert_eq!(ledger.record(entry(0.5)), (1.0, 1.5));
        assert_eq!(ledger.record(entry(0.0)), (1.5, 1.5));
        let reopened = Ledger::open(&path);
        assert_eq!(reopened.month_total("2026-11", "pr"), 1.5);
        assert_eq!(reopened.month("2026-11")["pr"], 1.5);
        let tokens = Tokens {
            input: 3000,
            output: 30,
            unpriced_input: 1000,
            unpriced_output: 10,
        };
        assert_eq!(reopened.month_tokens("2026-11")["pr"], tokens);
        assert_eq!(ledger.month_tokens("2026-11")["pr"], tokens);
        std::fs::remove_file(path).unwrap();
    }
}
