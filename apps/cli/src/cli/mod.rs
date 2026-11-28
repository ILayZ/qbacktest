use anyhow::Result;
use argh::FromArgs;
use qbt_engine::backtest::{Backtester, WalkForwardConfig, WalkForwardResult};
use qbt_engine::report::json::DatasetMetadata;
use qbt_engine::run::{BUILTINS, RunSpec, builtin_strategy};
use qbt_engine::strategy::catalog::{
    COMPARISON_ID, DEFAULT_DCA_AMOUNT, DEFAULT_DCA_INTERVAL, DEFAULT_DIP_LOOKBACK,
    DEFAULT_DIP_PERCENT, DEFAULT_STRATEGY, STRATEGIES, strategy_catalog,
};
use std::path::{Path, PathBuf};

use crate::manifest::{RunManifest, distinct_paths};

/// QBT - High-performance quantitative backtesting engine
#[derive(FromArgs)]
pub struct Cli {
    /// subcommand
    #[argh(subcommand)]
    pub command: Command,
}

#[derive(FromArgs)]
#[argh(subcommand)]
pub enum Command {
    /// Run a backtest on a price series
    Run(RunArgs),
    /// Validate and describe an hourly OHLCV dataset
    InspectDataset(InspectDatasetArgs),
    /// Replay a saved run
    Replay(ReplayArgs),
    /// Evaluate fixed parameters across independent time windows
    WalkForward(WalkForwardArgs),
    /// Generate HTML report
    Report(ReportArgs),
    /// List available strategies
    Strategies(StrategiesArgs),
}

/// Run a backtest on a price series
#[derive(FromArgs)]
#[argh(subcommand, name = "run")]
pub struct RunArgs {
    /// input CSV or Parquet file (OHLCV)
    #[argh(option)]
    pub input: PathBuf,
    /// market symbol to record in the results
    #[argh(option)]
    pub symbol: Option<String>,
    /// quote currency to record in the results
    #[argh(option)]
    pub quote_currency: Option<String>,
    /// output report path
    #[argh(option, default = r#"std::path::PathBuf::from("report.html")"#)]
    pub output: PathBuf,
    /// save a reproducible JSON run manifest
    #[argh(option)]
    pub save_manifest: Option<PathBuf>,
    /// write the resolved manifest without running the simulation (requires --save-manifest)
    #[argh(switch)]
    pub manifest_only: bool,
    /// trailing hourly closes used by DipDCA
    #[argh(option, default = "DEFAULT_DIP_LOOKBACK")]
    pub dip_lookback: usize,
    /// percent below trailing high required by DipDCA
    #[argh(option, default = "DEFAULT_DIP_PERCENT")]
    pub dip_percent: f64,
    /// strategy name
    #[argh(option, default = "DEFAULT_STRATEGY.to_string()")]
    pub strategy: String,
    /// DCA interval in hours
    #[argh(option, default = "DEFAULT_DCA_INTERVAL")]
    pub dca_interval: u32,
    /// DCA amount
    #[argh(option, default = "DEFAULT_DCA_AMOUNT")]
    pub dca_amount: f64,
    /// order amount, or daily reserve allocation for DipDCA; VolAdjustedDCA uses 1/24 per hour
    #[argh(option)]
    pub order_amount: Option<f64>,
    /// maximum buy notional per strategy per UTC execution day
    #[argh(option)]
    pub daily_limit: Option<f64>,
    /// minimum executed order value before fees (buys and sells)
    #[argh(option)]
    pub min_notional: Option<f64>,
    /// count buy fees towards the daily limit
    #[argh(switch)]
    pub limit_includes_fees: bool,
    /// write fills and rejected orders to this CSV
    #[argh(option)]
    pub ledger: Option<PathBuf>,
    /// portfolio summary JSON path
    #[argh(option)]
    pub summary: Option<PathBuf>,
    /// hourly equity JSON path
    #[argh(option)]
    pub equity: Option<PathBuf>,
    /// initial capital
    #[argh(option, default = r#"10000.0"#)]
    pub capital: f64,
    /// trading fee
    #[argh(option, default = r#"0.001"#)]
    pub fee: f64,
    /// slippage
    #[argh(option, default = r#"0.0005"#)]
    pub slippage: f64,
    /// risk-free rate (annual)
    #[argh(option, default = r#"0.03"#)]
    pub risk_free: f64,
}

/// Print JSON metadata for a valid hourly OHLCV dataset
#[derive(FromArgs)]
#[argh(subcommand, name = "inspect-dataset")]
pub struct InspectDatasetArgs {
    /// input CSV or Parquet file (OHLCV)
    #[argh(option)]
    pub input: PathBuf,
}

impl InspectDatasetArgs {
    pub fn execute(&self) -> Result<()> {
        let series = qbt_engine::data::PriceSeries::from_path(&self.input, "DATA".to_string())?;
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "timeframe": "1h",
                "row_count": series.len(),
                "start": series.bars[0].timestamp,
                "end": series.bars[series.len() - 1].timestamp,
            })
        );
        Ok(())
    }
}

/// Replay a saved run against its original dataset
#[derive(FromArgs)]
#[argh(subcommand, name = "replay")]
pub struct ReplayArgs {
    /// saved JSON run manifest
    #[argh(option)]
    pub manifest: PathBuf,
    /// input CSV or Parquet file (OHLCV)
    #[argh(option)]
    pub input: PathBuf,
    /// output report path
    #[argh(option, default = r#"PathBuf::from("report.html")"#)]
    pub output: PathBuf,
    /// optional order ledger CSV
    #[argh(option)]
    pub ledger: Option<PathBuf>,
    /// portfolio summary JSON path
    #[argh(option)]
    pub summary: Option<PathBuf>,
    /// hourly equity JSON path
    #[argh(option)]
    pub equity: Option<PathBuf>,
}

/// Evaluate fixed parameters across independent time windows
#[derive(FromArgs)]
#[argh(subcommand, name = "walk-forward")]
pub struct WalkForwardArgs {
    /// input CSV or Parquet file
    #[argh(option)]
    pub input: PathBuf,
    /// trailing hourly closes used by DipDCA
    #[argh(option, default = "DEFAULT_DIP_LOOKBACK")]
    pub dip_lookback: usize,
    /// percent below trailing high required by DipDCA
    #[argh(option, default = "DEFAULT_DIP_PERCENT")]
    pub dip_percent: f64,
    /// strategy name
    #[argh(option, default = r#""MomentumSMA".to_string()"#)]
    pub strategy: String,
    /// history offset in 30-day months
    #[argh(option, default = r#"12"#)]
    pub train: u32,
    /// test months
    #[argh(option, default = r#"6"#)]
    pub test: u32,
    /// step months
    #[argh(option, default = r#"6"#)]
    pub step: u32,
    /// write per-window metrics and UTC bar boundaries as JSON
    #[argh(option)]
    pub summary: Option<PathBuf>,
    /// output path
    #[argh(
        option,
        default = r#"std::path::PathBuf::from("walk_forward_report.html")"#
    )]
    pub output: PathBuf,
}

/// Generate an HTML report from a JSON results file
#[derive(FromArgs)]
#[argh(subcommand, name = "report")]
pub struct ReportArgs {
    /// input JSON file with results
    #[argh(option)]
    pub input: PathBuf,
    /// output HTML path
    #[argh(option, default = r#"std::path::PathBuf::from("report.html")"#)]
    pub output: PathBuf,
}

/// List available strategies
#[derive(FromArgs)]
#[argh(subcommand, name = "strategies")]
pub struct StrategiesArgs {
    /// show detailed strategy parameters
    #[argh(switch)]
    pub verbose: bool,
    /// print the machine-readable strategy catalog
    #[argh(switch)]
    pub json: bool,
}

impl RunArgs {
    pub fn execute(&self) -> Result<()> {
        use qbt_engine::backtest::{BacktestConfig, BuyBudget};
        use qbt_engine::data::PriceSeries;

        // CF-017: Validate positive intervals and parameters at CLI boundary
        if self.dca_interval == 0 {
            anyhow::bail!("DCA interval must be positive, got 0");
        }
        if self.capital <= 0.0 {
            anyhow::bail!("Initial capital must be positive, got {}", self.capital);
        }
        if self.fee < 0.0 || self.fee >= 1.0 {
            anyhow::bail!("Trading fee must be in [0, 1), got {}", self.fee);
        }
        if self.slippage < 0.0 || self.slippage >= 1.0 {
            anyhow::bail!("Slippage must be in [0, 1), got {}", self.slippage);
        }

        let dataset_metadata = DatasetMetadata {
            symbol: self.symbol.clone(),
            quote_currency: self.quote_currency.clone(),
        };
        let series = PriceSeries::from_path(&self.input, series_symbol(&dataset_metadata))?;

        let include_benchmarks = self.strategy.eq_ignore_ascii_case(COMPARISON_ID);
        let names = if include_benchmarks {
            BUILTINS.to_vec()
        } else {
            vec![self.strategy.as_str()]
        };
        let amount = self.order_amount.unwrap_or(self.dca_amount);
        anyhow::ensure!(
            amount.is_finite() && amount > 0.0,
            "Order amount must be positive and finite"
        );
        let vol_base = self.order_amount.map_or(self.dca_amount, |v| v / 24.0);
        let strategies = names
            .iter()
            .map(|name| {
                builtin_strategy(
                    name,
                    self.dca_interval,
                    amount,
                    vol_base,
                    self.dip_lookback,
                    self.dip_percent,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let budget = BuyBudget {
            daily_limit: self.daily_limit,
            min_notional: self.min_notional,
            include_fees: self.limit_includes_fees,
            entry_amount: self.order_amount,
        };
        let config = BacktestConfig {
            initial_capital: self.capital,
            trading_fee: self.fee,
            slippage: self.slippage,
            risk_free_rate: self.risk_free,
        };
        let spec = RunSpec {
            version: RunSpec::VERSION,
            strategies,
            config,
            budget,
            include_benchmarks,
        };
        distinct_paths(&[
            Some(&self.input),
            Some(&self.output),
            self.ledger.as_deref(),
            self.save_manifest.as_deref(),
            self.summary.as_deref(),
            self.equity.as_deref(),
        ])?;
        anyhow::ensure!(
            !self.manifest_only || self.save_manifest.is_some(),
            "--manifest-only requires --save-manifest"
        );
        if let Some(path) = &self.save_manifest {
            anyhow::ensure!(!path.try_exists()?, "Choose a new manifest path");
            let manifest = RunManifest::new(&series, spec, dataset_metadata)?;
            if self.manifest_only {
                return manifest.save(path);
            }
            render(
                &manifest.spec,
                &series,
                &manifest.dataset_metadata,
                &self.output,
                self.ledger.as_deref(),
                self.summary.as_deref(),
                self.equity.as_deref(),
            )?;
            manifest.save(path)?;
        } else {
            render(
                &spec,
                &series,
                &dataset_metadata,
                &self.output,
                self.ledger.as_deref(),
                self.summary.as_deref(),
                self.equity.as_deref(),
            )?;
        }
        Ok(())
    }
}

impl ReplayArgs {
    pub fn execute(&self) -> Result<()> {
        use qbt_engine::data::PriceSeries;

        distinct_paths(&[
            Some(&self.input),
            Some(&self.output),
            self.ledger.as_deref(),
            Some(&self.manifest),
            self.summary.as_deref(),
            self.equity.as_deref(),
        ])?;
        let manifest = RunManifest::load(&self.manifest)?;
        let series =
            PriceSeries::from_path(&self.input, series_symbol(&manifest.dataset_metadata))?;
        manifest.verify(&series)?;
        if let Some(windows) = &manifest.windows {
            anyhow::ensure!(
                self.ledger.is_none() && self.equity.is_none(),
                "Windowed runs support HTML and summary outputs"
            );
            let [strategy] = manifest.spec.strategies.as_slice() else {
                anyhow::bail!("Windowed runs require one strategy");
            };
            anyhow::ensure!(!manifest.spec.include_benchmarks, "Unexpected benchmarks");
            let bt = Backtester::new(manifest.spec.config.clone());
            let result = bt.walk_forward_with_budget(
                &series,
                |_, _| strategy.clone(),
                windows,
                &manifest.spec.budget,
            )?;
            return render_windows(&result, windows, &self.output, self.summary.as_deref());
        }
        render(
            &manifest.spec,
            &series,
            &manifest.dataset_metadata,
            &self.output,
            self.ledger.as_deref(),
            self.summary.as_deref(),
            self.equity.as_deref(),
        )
    }
}

fn series_symbol(metadata: &DatasetMetadata) -> String {
    metadata.symbol.clone().unwrap_or_else(|| "DATA".to_owned())
}

fn render(
    spec: &RunSpec,
    series: &qbt_engine::data::PriceSeries,
    metadata: &DatasetMetadata,
    output: &Path,
    ledger_path: Option<&Path>,
    summary_path: Option<&Path>,
    equity_path: Option<&Path>,
) -> Result<()> {
    use qbt_engine::report::HtmlReport;
    use std::io::{BufWriter, Write};

    let mut ledger = ledger_path
        .map(std::fs::File::create)
        .transpose()?
        .map(BufWriter::new);
    if let Some(w) = &mut ledger {
        writeln!(
            w,
            "strategy,signal_time,execution_time,side,status,price,quantity,fee,cash"
        )?;
    }
    let results = spec.execute(series, |name, e| {
        if let Some(w) = &mut ledger {
            writeln!(
                w,
                "{},{},{},{},{},{},{},{},{}",
                name,
                e.signal_time.to_rfc3339(),
                e.execution_time.to_rfc3339(),
                e.side,
                e.status,
                e.price,
                e.quantity,
                e.fee,
                e.cash
            )?;
        }
        Ok(())
    })?;
    if let Some(w) = &mut ledger {
        w.flush()?;
    }
    if let Some(path) = summary_path {
        qbt_engine::report::json::summary_with_metadata(path, &results, metadata)?;
    }
    if let Some(path) = equity_path {
        qbt_engine::report::json::equity(path, series, &results)?;
    }
    if spec.include_benchmarks {
        HtmlReport::comparison(&results, output)?;
    } else {
        HtmlReport::generate(&results, output)?;
    }

    println!("Report written to {}", output.display());
    Ok(())
}

impl WalkForwardArgs {
    pub fn execute(&self) -> Result<()> {
        use qbt_engine::backtest::BacktestConfig;
        use qbt_engine::data::PriceSeries;
        distinct_paths(&[
            Some(&self.input),
            Some(&self.output),
            self.summary.as_deref(),
        ])?;

        // CF-017: Validate positive intervals and window sizes at CLI boundary
        if self.train == 0 {
            anyhow::bail!("Train months must be positive, got 0");
        }
        if self.test == 0 {
            anyhow::bail!("Test months must be positive, got 0");
        }
        if self.step == 0 {
            anyhow::bail!("Step months must be positive, got 0");
        }

        let strategy = builtin_strategy(
            &self.strategy,
            DEFAULT_DCA_INTERVAL,
            DEFAULT_DCA_AMOUNT,
            DEFAULT_DCA_AMOUNT,
            self.dip_lookback,
            self.dip_percent,
        )?;
        let strategy_factory = move |_, _| strategy.clone();

        let series = PriceSeries::from_path(&self.input, "DATA".to_string())?;
        let bt = Backtester::new(BacktestConfig::default());

        let wf_config = WalkForwardConfig {
            train_months: self.train,
            test_months: self.test,
            step_months: self.step,
        };

        let wf_result = bt.walk_forward(&series, strategy_factory, &wf_config)?;

        render_windows(
            &wf_result,
            &wf_config,
            &self.output,
            self.summary.as_deref(),
        )
    }
}

fn render_windows(
    result: &WalkForwardResult,
    config: &WalkForwardConfig,
    output: &Path,
    summary: Option<&Path>,
) -> Result<()> {
    let results = &result.out_of_sample_results;
    qbt_engine::report::HtmlReport::generate(results, output)?;
    if let Some(path) = summary {
        use serde_json::json;
        let windows: Vec<_> = results
            .iter()
            .zip(&result.bounds)
            .map(|(run, bounds)| {
                json!({
                    "bounds": bounds, "strategy": run.strategy_name,
                    "initial_capital": run.initial_capital, "final_equity": run.final_value,
                    "total_return": run.total_return, "max_drawdown": run.max_drawdown,
                    "sharpe_ratio": run.sharpe_ratio, "completed_trades": run.num_trades
                })
            })
            .collect();
        let data = json!({
            "version": 1, "config": config, "windows": windows,
            "mean_return": result.total_oos_return, "positive_fraction": result.consistency
        });
        std::fs::write(path, serde_json::to_vec(&data)?)?;
    }
    println!(
        "Walk-forward complete: {} OOS periods, avg return {:.2}%, consistency {:.0}%",
        results.len(),
        result.total_oos_return * 100.0,
        result.consistency * 100.0
    );
    Ok(())
}

impl ReportArgs {
    pub fn execute(&self) -> Result<()> {
        use qbt_engine::backtest::BacktestResult;
        use qbt_engine::report::HtmlReport;

        let json = std::fs::read_to_string(&self.input)?;
        let results: Vec<BacktestResult> = serde_json::from_str(&json)?;
        HtmlReport::generate(&results, &self.output)?;
        println!("Report written to {}", self.output.display());
        Ok(())
    }
}

impl StrategiesArgs {
    pub fn execute(&self) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(&strategy_catalog())?);
            return Ok(());
        }
        for strategy in &STRATEGIES {
            println!("• {} - {}", strategy.id, strategy.description);
            if self.verbose {
                if !strategy.aliases.is_empty() {
                    println!("    Aliases: {}", strategy.aliases.join(", "));
                }
                let flags = strategy
                    .parameter_ids
                    .iter()
                    .map(|id| format!("--{}", id.replace('_', "-")))
                    .collect::<Vec<_>>();
                println!("    Parameters: {}", flags.join(", "));
                for (id, value) in strategy.fixed_settings() {
                    println!("    {id} = {value} (fixed)");
                }
            }
        }
        Ok(())
    }
}
