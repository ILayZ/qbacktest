# QBT

**A fast, scriptable, reproducible backtesting engine for retail traders and quant hobbyists.**

Run a strategy over your own OHLCV data in seconds, get risk-adjusted metrics and a
self-contained HTML report — no cloud subscription, no lock-in, no waiting.

QBT is the simulation engine for a strategy-testing platform. See the [platform architecture](docs/architecture.md) for the proposed service boundaries, run contract and delivery plan.

## Why

Most retail traders who want to backtest a strategy are forced into either:

- Heavy cloud platforms (paid, slow, closed source), or
- Complex Python notebooks (dependency hell, slow on large data).

`qbt` is a single, self-contained CLI binary. It reads CSV or Parquet, runs a
strategy, computes standard performance and risk metrics, and writes an HTML report.
Fast, reproducible, and yours.

## Features

- **Single static binary** — no runtime, no package manager, no virtualenv.
- **Real backtesting** — configurable initial capital, trading fee, slippage, risk-free rate.
- **Built-in strategies:** DCA, DipDCA, MomentumSMA, VolAdjustedDCA and RegimeSwitch.
- **Windowed evaluation** — each test window starts with fresh capital and indicators. The CLI uses fixed parameters. It reports mean window return and the fraction of profitable windows.
- **Metrics:** total/annualized return, Sharpe, Sortino, max drawdown, win rate, profit factor, volatility.
- **HTML report** — self-contained, shareable, with charts.
- **Fast** — Rust + Polars; handles large datasets without notebooks.

## Install

Build from source (requires Rust stable):

```bash
git clone <repo-url> qbt
cd qbt
cargo build --release
# binary at: target/release/qbt
```

The [local HTTP service](apps/platform/README.md) validates and catalogs CSV and Parquet datasets, queues runs and serves reports, order ledgers and replay manifests.

## Repository layout

The root Cargo workspace builds `qbt-engine` (`crates/qbt-engine`) and `qbt-cli` (`apps/cli`). Run `cargo test --workspace --locked` to test both packages, or `cargo run -p qbt-cli -- strategies` to launch the CLI during development.

## Quick start

```bash
# List available strategies
qbt strategies --verbose

# Run a DCA backtest on hourly OHLCV data
qbt run \
  --input data/sample_btc_1h.csv \
  --strategy DCA \
  --dca-interval 24 \
  --dca-amount 100 \
  --capital 10000 \
  --fee 0.001 \
  --output report.html

# Walk-forward validation of a momentum strategy
qbt walk-forward \
  --input data/sample_btc_1h.csv \
  --strategy MomentumSMA \
  --train 1 --test 1 --step 1 \
  --output wf_report.html
```

## Input data format

CSV with header row and these columns:

```csv
timestamp,open,high,low,close,volume
2024-01-01T00:00:00Z,42000.00,42014.52,41968.55,41977.89,1280.79
```

- `timestamp` is an ISO 8601 datetime. QBT normalizes timestamps to UTC and requires consecutive hourly bars in ascending order.
- A signal uses the current close and fills at the following open when funds and position allow. Equity is valued at each close. The final close signal expires.
- Prices must be finite and positive. The low and high must contain both the open and the close. Volume must be finite and nonnegative.
- Parquet uses the same column names with a datetime `timestamp` column.

The sample at `data/sample_btc_1h.csv` contains 4,320 hourly bars. Run `qbt inspect-dataset --input data/sample_btc_1h.csv` to validate the file and print its schema version, timeframe, row count and UTC coverage as JSON.

## Commands

### `run`

| Flag | Default | Description |
|------|---------|-------------|
| `--input` | *(required)* | Input CSV/Parquet (OHLCV) |
| `--symbol` | *(optional)* | Market symbol for the summary and manifest |
| `--quote-currency` | *(optional)* | Quote currency for the summary and manifest |
| `--output` | `report.html` | Output HTML report path |
| `--strategy` | `DCA` | `DCA`, `DipDCA`, `MomentumSMA`, `VolAdjustedDCA`, `RegimeSwitch`, or `all` |
| `--dca-interval` | `24` | DCA interval in hours |
| `--dca-amount` | `100` | DCA amount per interval |
| `--capital` | `10000` | Initial capital |
| `--fee` | `0.001` | Trading fee (fraction) |
| `--slippage` | `0.0005` | Slippage (fraction) |
| `--risk-free` | `0.03` | Annual risk-free rate |

Compare all five strategies on independent accounts funded once at the start:

```bash
qbt run --input data/sample_btc_1h.csv --strategy all --capital 1800 \
  --order-amount 10 --daily-limit 10 --ledger orders.csv --output comparison.html
```

Each account can buy up to $10 per UTC execution day. Fees are charged on top by default.
Use `--limit-includes-fees` to count buy fees toward the cap. Sales do not replenish
the daily allowance. A buy that exceeds the remaining allowance is reduced, while a
buy with insufficient cash is skipped. No cash is deposited during the simulation.

`--order-amount` sizes each DCA or directional entry. For hourly VolAdjustedDCA it
sets the baseline to 1/24 of that amount before the volatility adjustment. Without
this flag, existing sizing defaults apply. The optional CSV journal lists executed
and skipped orders with their signal and execution-bar timestamps, status, price,
filled quantity, fee and remaining cash.

Add `--min-notional 1` to skip buys and sells below $1 after applying the buy
budget and before fees. Skips are recorded as `below_min_notional` and consume
no cash or daily allowance. With `--strategy all`, the report plots hourly closing
equity and compares final value against DCA, drawdown, mean invested share across
bar closes, and skipped buy counts. The cash benchmark earns zero interest.
Buy and hold invests the initial balance at the first executable open, with the
same fee, slippage and order minimum; the daily allocation limit applies only
to the five strategies.

### Save and replay a run

```bash
qbt run --input data/sample_btc_1h.csv --strategy all --capital 1800 \\
  --order-amount 10 --daily-limit 10 --save-manifest experiment.json
qbt replay --manifest experiment.json --input data/sample_btc_1h.csv \\
  --ledger replay.csv --output replay.html
```

Choose a new manifest filename for each saved run. The manifest stores resolved simulation parameters and XXH3-128 fingerprints of the parsed UTC bars and CLI executable. Preserve the binary and the dataset with the manifest. Replay checks both fingerprints before opening its outputs. CSV files may be relocated or reformatted if they parse to identical bars. This format covers `run`, including comparisons.

### Strategy catalog

Run `qbt strategies` to list the runnable strategies. Add `--verbose` to see
their CLI parameters, aliases and fixed settings, or use `qbt strategies --json`
for the versioned catalog with parameter types, defaults and ranges.

### DipDCA

```bash
qbt run --input data/sample_btc_1h.csv --strategy DipDCA --capital 1800 \
  --order-amount 10 --dip-percent 5 --dip-lookback 168 --min-notional 1 --ledger dip.csv
```

DipDCA earmarks up to the daily allocation from existing cash at the start and
on each new UTC date. After a full trailing window (including the current close),
a close at least the configured percentage below its highest close signals a buy
at the next open. While the condition persists, each qualifying close can trigger
a purchase. The reserve covers entry fees; only fills consume it. By default a
fill may spend several days of accumulated cash. Add `--daily-limit 10` to cap
execution separately. The report includes unspent reserve and the longest run
of hourly execution bars with a funded reserve and no purchase.

### `walk-forward`

| Flag | Default | Description |
|------|---------|-------------|
| `--input` | *(required)* | Input CSV/Parquet |
| `--strategy` | `MomentumSMA` | Fixed strategy to evaluate |
| `--train` | `12` | History offset (30-day months) |
| `--test` | `6` | Test window (months) |
| `--step` | `6` | Step (months) |
| `--output` | `walk_forward_report.html` | Output HTML |
| `--summary` | optional | Versioned JSON with UTC window boundaries and metrics |

Each window starts with fresh capital and strategy state. One month is 720 consecutive hourly bars. The history offset precedes the first test bar. A step shorter than the test length produces overlapping windows. The JSON summary records the arithmetic mean of independent window returns as a fraction and the first and last included UTC bars for each test.

### `report`

Generate an HTML report from a JSON file of results.

### `strategies`

List available strategies. Use `--verbose` for parameter details.

## Monetization (planned)

- **Free:** open-source core engine, basic strategies, HTML reports.
- **Pro (planned):** additional strategy library, scheduled/automated report generation,
  multi-symbol portfolio backtests, priority support.

## License

Open-source core. See `LICENSE` (TBD — MIT planned).
