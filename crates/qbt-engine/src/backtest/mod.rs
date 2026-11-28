use crate::data::PriceSeries;
use crate::metrics::PerformanceMetrics;
use crate::strategy::{Signal, Strategy};
use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{info, instrument};

/// A single buy lot with cost basis including fees (for FIFO P&L)
#[derive(Debug, Clone)]
struct Lot {
    quantity: f64,
    cost_per_unit: f64, // includes entry fee
    entry_time: chrono::DateTime<chrono::Utc>,
}

/// Backtest result for a single run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub strategy_name: String,
    pub initial_capital: f64,
    pub final_value: f64,
    #[serde(default)]
    pub mean_asset_weight: Option<f64>,
    #[serde(default)]
    pub dip_reserve: Option<f64>,
    #[serde(default)]
    pub max_reserve_wait_hours: usize,
    #[serde(default)]
    pub skipped_buys: usize,
    #[serde(default)]
    pub below_minimum_buys: usize,
    pub total_return: f64, // percentage
    pub cagr: f64,         // annualized
    pub sharpe_ratio: f64,
    pub sortino_ratio: f64,
    pub max_drawdown: f64,
    pub ulcer_index: f64,
    pub win_rate: f64,      // % of profitable trades
    pub profit_factor: f64, // gross profit / gross loss
    pub num_trades: usize,
    pub equity_curve: Vec<f64>,
    pub trade_log: Vec<Trade>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub entry_time: chrono::DateTime<chrono::Utc>,
    pub exit_time: Option<chrono::DateTime<chrono::Utc>>,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub quantity: f64,
    pub pnl: Option<f64>,
}

/// Backtest configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub initial_capital: f64,
    pub trading_fee: f64,    // e.g., 0.001 = 0.1%
    pub slippage: f64,       // e.g., 0.0005 = 0.05%
    pub risk_free_rate: f64, // annual, e.g., 0.03
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_capital: 10_000.0,
            trading_fee: 0.001,
            slippage: 0.0005,
            risk_free_rate: 0.03,
        }
    }
}

/// Fixed-parameter window evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardConfig {
    pub train_months: u32,
    pub test_months: u32,
    pub step_months: u32,
}

impl Default for WalkForwardConfig {
    fn default() -> Self {
        Self {
            train_months: 12,
            test_months: 6,
            step_months: 6,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowBounds {
    pub history_start: chrono::DateTime<chrono::Utc>,
    pub test_start: chrono::DateTime<chrono::Utc>,
    pub test_last: chrono::DateTime<chrono::Utc>,
}

/// Walk-forward result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardResult {
    #[serde(default)]
    pub bounds: Vec<WindowBounds>,
    pub out_of_sample_results: Vec<BacktestResult>,
    /// Arithmetic mean of independent window returns (legacy field name).
    pub total_oos_return: f64,
    pub avg_oos_sharpe: f64,
    pub consistency: f64, // % of OOS periods with positive return
}

#[derive(Debug, Clone, Default)]
pub struct Backtester {
    pub config: BacktestConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuyBudget {
    pub daily_limit: Option<f64>,
    pub min_notional: Option<f64>,
    pub include_fees: bool,
    pub entry_amount: Option<f64>,
}

pub struct OrderEvent {
    pub signal_time: chrono::DateTime<chrono::Utc>,
    pub execution_time: chrono::DateTime<chrono::Utc>,
    pub side: &'static str,
    pub status: &'static str,
    pub price: f64,
    pub quantity: f64,
    pub fee: f64,
    pub cash: f64,
}

impl Backtester {
    pub fn new(config: BacktestConfig) -> Self {
        Self { config }
    }

    #[instrument(skip_all)]
    pub fn run(&self, series: &PriceSeries, strategy: &Strategy) -> Result<BacktestResult> {
        self.run_with_budget(series, strategy, &BuyBudget::default(), |_| Ok(()))
    }

    pub fn run_with_budget(
        &self,
        series: &PriceSeries,
        strategy: &Strategy,
        budget: &BuyBudget,
        mut record: impl FnMut(OrderEvent) -> Result<()>,
    ) -> Result<BacktestResult> {
        for value in [budget.daily_limit, budget.entry_amount, budget.min_notional]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                value.is_finite() && value > 0.0,
                "Budget must be finite and positive"
            );
        }
        if !self.config.initial_capital.is_finite() || self.config.initial_capital <= 0.0 {
            anyhow::bail!("Initial capital must be positive");
        }
        if !(0.0..1.0).contains(&self.config.trading_fee) {
            anyhow::bail!("Trading fee must be in [0, 1)");
        }
        if !(0.0..1.0).contains(&self.config.slippage) {
            anyhow::bail!("Slippage must be in [0, 1)");
        }
        series.validate()?;

        info!(
            symbol = %series.symbol,
            strategy = strategy.name(),
            bars = series.len(),
            "Starting backtest"
        );

        let allocation = if let Strategy::DipDCA {
            daily_allocation,
            lookback_hours,
            drop_fraction,
        } = strategy
        {
            anyhow::ensure!(
                daily_allocation.is_finite() && *daily_allocation > 0.0,
                "Daily allocation must be finite and positive"
            );
            anyhow::ensure!(
                *lookback_hours >= 2 && *drop_fraction > 0.0 && *drop_fraction < 1.0,
                "DipDCA requires at least two closes and a drop between 0 and 100 percent"
            );
            Some(*daily_allocation)
        } else {
            None
        };
        let mut reserve = allocation
            .unwrap_or_default()
            .min(self.config.initial_capital);
        let mut waiting = 0;
        let mut max_waiting = 0;
        let signals = strategy.generate_signals(series);
        let single_position = matches!(
            strategy,
            Strategy::MomentumSMA { .. } | Strategy::RegimeSwitch { .. }
        );
        let mut cash = self.config.initial_capital;
        let mut lots: Vec<Lot> = Vec::new();
        let mut equity = vec![cash];
        let mut trades = Vec::new();

        let minimum = budget.min_notional.unwrap_or(0.0);
        let mut weight_sum = 0.0;
        let mut skipped_buys = 0;
        let mut below_minimum_buys = 0;
        let mut spent = 0.0;
        let mut day = series.bars[0].timestamp.date_naive();

        // A close signal can only fill at the following open. Mark each bar at its close.
        for (signal, pair) in signals.iter().zip(series.bars.windows(2)) {
            let bar = &pair[1];
            if bar.timestamp.date_naive() != day {
                day = bar.timestamp.date_naive();
                spent = 0.0;
                reserve = (reserve + allocation.unwrap_or_default()).min(cash);
            }
            let exec_price = bar.open;
            let exec_ts = bar.timestamp;
            let event = |side, status, price, quantity, fee, cash| OrderEvent {
                signal_time: pair[0].timestamp,
                execution_time: exec_ts,
                side,
                status,
                price,
                quantity,
                fee,
                cash,
            };
            let mut dip_filled = false;
            match signal {
                Signal::Buy { .. } | Signal::BuyReserve if !single_position || lots.is_empty() => {
                    let buy_price = exec_price * (1.0 + self.config.slippage);
                    let requested = match signal {
                        Signal::Buy { amount } if single_position => {
                            budget.entry_amount.unwrap_or(*amount)
                        }
                        Signal::Buy { amount } => *amount,
                        _ => reserve / (1.0 + self.config.trading_fee),
                    };
                    let fee_factor = if budget.include_fees {
                        1.0 + self.config.trading_fee
                    } else {
                        1.0
                    };
                    let remaining = budget.daily_limit.unwrap_or(f64::INFINITY) - spent;
                    let amount = requested.min(remaining / fee_factor);
                    let fee = amount * self.config.trading_fee;
                    // A fee-inclusive cap can round a debit a few ulps above the allowance.
                    let total_cost = if budget.include_fees {
                        (amount * fee_factor).min(remaining)
                    } else {
                        amount + fee
                    };

                    let total_cost = if allocation.is_some() {
                        total_cost.min(reserve)
                    } else {
                        total_cost
                    };
                    if allocation.is_some() && reserve == 0.0 {
                        skipped_buys += 1;
                        record(event("buy", "empty_reserve", buy_price, 0.0, 0.0, cash))?;
                    } else if amount <= 0.0 {
                        skipped_buys += 1;
                        record(event("buy", "daily_limit", buy_price, 0.0, 0.0, cash))?;
                    } else if amount < minimum {
                        skipped_buys += 1;
                        below_minimum_buys += 1;
                        record(event(
                            "buy",
                            "below_min_notional",
                            buy_price,
                            0.0,
                            0.0,
                            cash,
                        ))?;
                    } else if cash < total_cost {
                        skipped_buys += 1;
                        record(event("buy", "insufficient_cash", buy_price, 0.0, 0.0, cash))?;
                    } else {
                        cash -= total_cost;
                        if allocation.is_some() {
                            reserve = if amount == requested {
                                0.0
                            } else {
                                reserve - total_cost
                            };
                            dip_filled = true;
                        }
                        spent = if requested >= remaining / fee_factor {
                            budget.daily_limit.unwrap_or(f64::INFINITY)
                        } else {
                            spent + amount * fee_factor
                        };
                        let qty = amount / buy_price;
                        record(event("buy", "filled", buy_price, qty, fee, cash))?;
                        // Cost per unit includes entry fee pro-rated
                        let cost_per_unit = buy_price * (1.0 + self.config.trading_fee);
                        lots.push(Lot {
                            quantity: qty,
                            cost_per_unit,
                            entry_time: exec_ts,
                        });
                    }
                }
                Signal::Sell { amount } => {
                    // Total position from lots
                    let position: f64 = lots.iter().map(|l| l.quantity).sum();
                    if position > 1e-9 {
                        let sell_price = exec_price * (1.0 - self.config.slippage);
                        let sell_all = *amount >= f64::MAX / 2.0;
                        let notional = if sell_all {
                            position * sell_price
                        } else {
                            (*amount).min(position * sell_price)
                        };
                        if notional < minimum {
                            record(event(
                                "sell",
                                "below_min_notional",
                                sell_price,
                                0.0,
                                0.0,
                                cash,
                            ))?;
                        } else {
                            let qty_to_sell = notional / sell_price;

                            // FIFO lot consumption
                            let mut remaining_to_sell = qty_to_sell;
                            let mut consumed_lots: Vec<Lot> = Vec::new();

                            for lot in lots.drain(..) {
                                if remaining_to_sell <= 1e-9 {
                                    consumed_lots.push(lot);
                                    continue;
                                }
                                let take = lot.quantity.min(remaining_to_sell);
                                // P&L = (sell_price - cost_per_unit) * take
                                // cost_per_unit already includes entry fee; subtract exit fee from proceeds
                                let exit_fee_per_unit = sell_price * self.config.trading_fee;
                                let net_sell_per_unit = sell_price - exit_fee_per_unit;
                                let pnl = (net_sell_per_unit - lot.cost_per_unit) * take;
                                trades.push(Trade {
                                    entry_time: lot.entry_time,
                                    exit_time: Some(exec_ts),
                                    entry_price: lot.cost_per_unit, // cost basis includes entry fee
                                    exit_price: Some(net_sell_per_unit), // net of exit fee
                                    quantity: take,
                                    pnl: Some(pnl),
                                });

                                remaining_to_sell -= take;
                                let lot_remain = lot.quantity - take;
                                if lot_remain > 1e-9 {
                                    consumed_lots.push(Lot {
                                        quantity: lot_remain,
                                        cost_per_unit: lot.cost_per_unit,
                                        entry_time: lot.entry_time,
                                    });
                                }
                            }
                            // Put back unconsumed lots
                            lots = consumed_lots;

                            let proceeds = qty_to_sell * sell_price;
                            let fee = proceeds * self.config.trading_fee;
                            cash += proceeds - fee;
                            record(event("sell", "filled", sell_price, qty_to_sell, fee, cash))?;
                        }
                    } else {
                        record(event("sell", "no_position", exec_price, 0.0, 0.0, cash))?;
                    }
                }
                Signal::Buy { .. } | Signal::BuyReserve => {
                    skipped_buys += 1;
                    record(event("buy", "position_open", exec_price, 0.0, 0.0, cash))?;
                }
                Signal::Hold => {}
            }

            if reserve > 0.0 && !dip_filled {
                waiting += 1;
                max_waiting = max_waiting.max(waiting);
            } else {
                waiting = 0;
            }
            let position_qty: f64 = lots.iter().map(|l| l.quantity).sum();
            let asset_value = position_qty * bar.close;
            let value = cash + asset_value;
            weight_sum += asset_value / value;
            equity.push(value);
        }

        let mut metrics = calculate_metrics(&equity, &trades, self.config.risk_free_rate);
        metrics.dip_reserve = allocation.map(|_| reserve);
        metrics.max_reserve_wait_hours = max_waiting;
        metrics.mean_asset_weight = Some(weight_sum / equity.len() as f64);
        metrics.skipped_buys = skipped_buys;
        metrics.below_minimum_buys = below_minimum_buys;
        metrics.strategy_name = strategy.name().to_string();

        info!(
            final_value = metrics.final_value,
            total_return = metrics.total_return,
            sharpe = metrics.sharpe_ratio,
            "Backtest complete"
        );

        Ok(metrics)
    }

    /// Run multiple parameter sets in parallel
    pub fn run_parallel(
        &self,
        series: &PriceSeries,
        strategies: &[Strategy],
    ) -> Result<Vec<BacktestResult>> {
        strategies.par_iter().map(|s| self.run(series, s)).collect()
    }

    /// Walk-forward analysis
    pub fn walk_forward(
        &self,
        series: &PriceSeries,
        strategy_factory: impl Fn(u32, u32) -> Strategy + Send + Sync,
        wf_config: &WalkForwardConfig,
    ) -> Result<WalkForwardResult> {
        self.walk_forward_with_budget(series, strategy_factory, wf_config, &BuyBudget::default())
    }

    pub fn walk_forward_with_budget(
        &self,
        series: &PriceSeries,
        strategy_factory: impl Fn(u32, u32) -> Strategy + Send + Sync,
        wf_config: &WalkForwardConfig,
        budget: &BuyBudget,
    ) -> Result<WalkForwardResult> {
        // CF-017: Validate walk-forward config at engine boundary
        if wf_config.train_months == 0 {
            anyhow::bail!("Walk-forward train_months must be positive");
        }
        if wf_config.test_months == 0 {
            anyhow::bail!("Walk-forward test_months must be positive");
        }
        if wf_config.step_months == 0 {
            anyhow::bail!("Walk-forward step_months must be positive");
        }
        series.validate()?;

        info!(
            train_months = wf_config.train_months,
            test_months = wf_config.test_months,
            "Starting walk-forward analysis"
        );

        let total_bars = series.len();
        let bar_per_day = 24usize; // assume hourly bars
        let train_bars = (wf_config.train_months as usize) * 30 * bar_per_day;
        let test_bars = (wf_config.test_months as usize) * 30 * bar_per_day;
        let step_bars = (wf_config.step_months as usize) * 30 * bar_per_day;

        if train_bars == 0 || test_bars == 0 || step_bars == 0 {
            anyhow::bail!(
                "Computed window sizes are zero; check train/test/step months and bar frequency"
            );
        }

        let mut bounds = Vec::new();
        let mut oos_results: Vec<BacktestResult> = Vec::new();
        let mut start = 0usize;

        while start + train_bars + test_bars <= total_bars {
            let train_end = start + train_bars;
            let test_end = start + train_bars + test_bars;

            // Resolve the strategy for this window before evaluating it.
            let strategy = strategy_factory(train_end as u32, test_end as u32);

            // Test: run OOS on the test window [train_end, test_end)
            let oos_bars: Vec<crate::data::Bar> = series.bars[train_end..test_end].to_vec();
            let oos_series = crate::data::PriceSeries {
                symbol: series.symbol.clone(),
                bars: oos_bars,
            };
            let oos_result = self.run_with_budget(&oos_series, &strategy, budget, |_| Ok(()))?;
            bounds.push(WindowBounds {
                history_start: series.bars[start].timestamp,
                test_start: series.bars[train_end].timestamp,
                test_last: series.bars[test_end - 1].timestamp,
            });
            oos_results.push(oos_result);

            start += step_bars;
        }

        if oos_results.is_empty() {
            anyhow::bail!("Insufficient data for walk-forward with current window sizes");
        }

        let total_oos_return: f64 =
            oos_results.iter().map(|r| r.total_return).sum::<f64>() / oos_results.len() as f64;
        let avg_oos_sharpe: f64 =
            oos_results.iter().map(|r| r.sharpe_ratio).sum::<f64>() / oos_results.len() as f64;
        let consistency = oos_results.iter().filter(|r| r.total_return > 0.0).count() as f64
            / oos_results.len() as f64;

        info!(
            oos_periods = oos_results.len(),
            total_return = total_oos_return,
            avg_sharpe = avg_oos_sharpe,
            consistency = consistency,
            "Walk-forward complete"
        );

        Ok(WalkForwardResult {
            bounds,
            out_of_sample_results: oos_results,
            total_oos_return,
            avg_oos_sharpe,
            consistency,
        })
    }
}

fn calculate_metrics(equity: &[f64], trades: &[Trade], risk_free_rate: f64) -> BacktestResult {
    let returns: Vec<_> = equity.windows(2).map(|w| w[1] / w[0] - 1.0).collect();
    let pnls: Vec<_> = trades.iter().filter_map(|t| t.pnl).collect();
    let metrics =
        PerformanceMetrics::calculate(equity, &returns, risk_free_rate, 8760.0, &[], &pnls);

    BacktestResult {
        strategy_name: String::new(),
        initial_capital: equity[0],
        final_value: *equity.last().unwrap(),
        mean_asset_weight: None,
        dip_reserve: None,
        max_reserve_wait_hours: 0,
        skipped_buys: 0,
        below_minimum_buys: 0,
        total_return: metrics.total_return,
        cagr: metrics.cagr,
        sharpe_ratio: metrics.sharpe_ratio,
        sortino_ratio: metrics.sortino_ratio,
        max_drawdown: metrics.max_drawdown,
        ulcer_index: metrics.ulcer_index,
        win_rate: metrics.win_rate,
        profit_factor: metrics.profit_factor,
        num_trades: metrics.num_trades,
        equity_curve: equity.to_vec(),
        trade_log: trades.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Bar;
    use chrono::{TimeZone, Utc};

    fn make_series(prices: Vec<f64>) -> PriceSeries {
        let bars: Vec<Bar> = prices
            .iter()
            .enumerate()
            .map(|(i, p)| Bar {
                timestamp: Utc.timestamp_opt(i as i64 * 3600, 0).unwrap(),
                open: *p,
                high: *p,
                low: *p,
                close: *p,
                volume: 1000.0,
            })
            .collect();
        PriceSeries {
            symbol: "TEST".into(),
            bars,
        }
    }

    #[test]
    fn test_simple_backtest() {
        let prices: Vec<f64> = (0..100).map(|i| 100.0 + i as f64 * 0.1).collect();
        let series = make_series(prices);
        let strategy = Strategy::DCA {
            interval_hours: 10,
            amount: 50.0,
        };
        let bt = Backtester::new(BacktestConfig::default());
        let result = bt.run(&series, &strategy).unwrap();

        // DCA only buys (no sells), so num_trades = 0 (completed round-trips).
        // Verify equity growth instead.
        assert_eq!(result.num_trades, 0);
        assert!(result.final_value > result.initial_capital);
        assert!(result.total_return > 0.0);
    }

    #[test]
    fn test_momentum_backtest() {
        // Extend series to ensure complete round-trip with causal execution:
        // - Signal at bar i uses bar i close, executes at bar i+1 open
        // - Need enough bars for: buy signal -> buy execution -> sell signal -> sell execution
        let prices = vec![
            100.0, 101.0, 102.0, 110.0, 115.0, 120.0, 125.0, 130.0, // uptrend
            125.0, 120.0, 115.0, 110.0, 105.0, 100.0, 95.0, 90.0, // downtrend triggers sell
            90.0, 90.0, 90.0, 90.0, // flat tail for execution
        ];
        let series = make_series(prices);
        let strategy = Strategy::MomentumSMA {
            lookback: 3,
            buy_threshold: 0.01,
            sell_threshold: 0.01,
        };
        let bt = Backtester::new(BacktestConfig::default());
        let result = bt.run(&series, &strategy).unwrap();

        // Should have entered and exited (one completed round-trip = 1 trade)
        assert_eq!(result.num_trades, 1);
    }

    // CF-015: First-bar costs omitted from performance.
    // Initial capital must be the configured starting capital, not the first
    // post-trade equity. Regression test: flat $100 bars, DCA buys $100 once
    // with 10% fee on $1000 capital. Expected: initial=1000, final=990, return=-1%.
    #[test]
    fn test_cf015_first_bar_costs_included() {
        // 1500 flat bars at $100
        let prices = vec![100.0; 1500];
        let series = make_series(prices);
        // DCA interval 9999 > 1500 bars => only first bar buys
        let strategy = Strategy::DCA {
            interval_hours: 9999,
            amount: 100.0,
        };
        let config = BacktestConfig {
            initial_capital: 1000.0,
            trading_fee: 0.1,
            slippage: 0.0,
            risk_free_rate: 0.03,
        };
        let bt = Backtester::new(config);
        let result = bt.run(&series, &strategy).unwrap();

        // Configured initial capital retained
        assert_eq!(result.initial_capital, 1000.0);
        // First-bar fee: $100 buy at 10% fee => $110 cost => final equity $990
        assert!(
            (result.final_value - 990.0).abs() < 1e-9,
            "final_value={}",
            result.final_value
        );
        // Return = -1% (990 - 1000) / 1000
        assert!(
            (result.total_return - (-0.01)).abs() < 1e-9,
            "total_return={}",
            result.total_return
        );
    }

    #[test]
    fn fills_at_next_open_and_reconciles_net_pnl() {
        let mut series = make_series(vec![100.0, 120.0, 125.0, 80.0, 90.0]);
        series.bars[2].open = 100.0;
        series.bars[2].low = 100.0;
        series.bars[4].open = 110.0;
        series.bars[4].high = 110.0;
        let strategy = Strategy::MomentumSMA {
            lookback: 2,
            buy_threshold: 0.01,
            sell_threshold: 0.01,
        };
        let bt = Backtester::new(BacktestConfig {
            initial_capital: 1000.0,
            trading_fee: 0.01,
            slippage: 0.0,
            risk_free_rate: 0.0,
        });
        let result = bt.run(&series, &strategy).unwrap();
        assert_eq!(result.trade_log.len(), 1);
        let trade = &result.trade_log[0];
        assert_eq!(trade.entry_time, series.bars[2].timestamp);
        assert_eq!(trade.exit_time, Some(series.bars[4].timestamp));
        assert_eq!(trade.quantity, 1.0);
        assert_eq!(trade.entry_price, 101.0);
        assert!((trade.exit_price.unwrap() - 108.9).abs() < 1e-9);
        assert!((trade.pnl.unwrap() - 7.9).abs() < 1e-9);
        assert!((result.final_value - 1007.9).abs() < 1e-9);
        assert_eq!(&result.equity_curve[..4], &[1000.0, 1000.0, 1024.0, 979.0]);

        // Truncating before the sell fill leaves the position marked at the last close.
        series.bars.pop();
        let prefix = bt.run(&series, &strategy).unwrap();
        assert_eq!(prefix.equity_curve, result.equity_curve[..4]);
        assert!(prefix.trade_log.is_empty());

        let poor = Backtester::new(BacktestConfig {
            initial_capital: 100.0,
            ..bt.config
        });
        let rejected = poor.run(&series, &strategy).unwrap();
        assert_eq!(rejected.equity_curve, vec![100.0; 4]);
        assert!(rejected.trade_log.is_empty());
    }

    #[test]
    fn minimum_allows_exact_buy_and_preserves_position_on_small_exit() {
        let series = make_series(vec![100.0, 120.0, 100.0, 80.0, 70.0]);
        let strategy = Strategy::MomentumSMA {
            lookback: 2,
            buy_threshold: 0.01,
            sell_threshold: 0.01,
        };
        let bt = Backtester::new(BacktestConfig {
            initial_capital: 1000.0,
            slippage: 0.0,
            trading_fee: 0.0,
            risk_free_rate: 0.0,
        });
        let budget = BuyBudget {
            min_notional: Some(100.0),
            ..BuyBudget::default()
        };
        let mut events = Vec::new();
        let result = bt
            .run_with_budget(&series, &strategy, &budget, |e| {
                events.push((e.side, e.status, e.cash));
                Ok(())
            })
            .unwrap();
        assert_eq!(events[0], ("buy", "filled", 900.0));
        assert_eq!(events[1], ("sell", "below_min_notional", 900.0));
        assert!(result.trade_log.is_empty());
        assert_eq!(result.final_value, 970.0);
    }

    #[test]
    fn dip_reserve_carries_over_and_fills_at_the_next_open() {
        let mut prices = vec![100.0; 51];
        prices[23..].fill(90.0);
        prices[47] = 120.0;
        let series = make_series(prices);
        let bt = Backtester::new(BacktestConfig {
            initial_capital: 100.0,
            trading_fee: 0.0,
            slippage: 0.0,
            risk_free_rate: 0.0,
        });
        let strategy = Strategy::DipDCA {
            daily_allocation: 10.0,
            lookback_hours: 24,
            drop_fraction: 0.1,
        };
        let budget = BuyBudget {
            min_notional: Some(25.0),
            ..BuyBudget::default()
        };
        let mut fills = Vec::new();
        let result = bt
            .run_with_budget(&series, &strategy, &budget, |event| {
                if event.status == "filled" {
                    fills.push((event.signal_time, event.execution_time, event.cash));
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fills,
            vec![(series.bars[48].timestamp, series.bars[49].timestamp, 70.0)]
        );
        assert!(result.below_minimum_buys > 0);
        assert_eq!(result.dip_reserve, Some(0.0));
        assert_eq!(result.final_value, 100.0);
    }

    #[test]
    fn refuses_irregular_bars_and_accepts_one_bar() {
        let mut series = make_series(vec![100.0; 3]);
        let bt = Backtester::default();
        let strategy = Strategy::DCA {
            interval_hours: 1,
            amount: 100.0,
        };
        series.bars.remove(1);
        assert!(bt.run(&series, &strategy).is_err());
        series.bars.pop();
        let result = bt.run(&series, &strategy).unwrap();
        assert_eq!(result.final_value, bt.config.initial_capital);
        assert_eq!(result.total_return, 0.0);
    }
}
