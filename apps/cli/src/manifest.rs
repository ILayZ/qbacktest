use anyhow::{Context, Result, ensure};
use qbt_engine::data::PriceSeries;
use qbt_engine::{report::json::DatasetMetadata, run::RunSpec};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::Xxh3;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunManifest {
    version: u32,
    executable_xxh3_128: String,
    bars_xxh3_128: String,
    pub spec: RunSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<qbt_engine::backtest::WalkForwardConfig>,
    #[serde(default, skip_serializing_if = "DatasetMetadata::is_empty")]
    pub dataset_metadata: DatasetMetadata,
}

fn executable_hash() -> Result<String> {
    let mut file = File::open(std::env::current_exe()?)?;
    let mut hash = Xxh3::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:032x}", hash.digest128()))
}

fn bars_hash(series: &PriceSeries) -> String {
    let mut hash = Xxh3::new();
    for bar in &series.bars {
        hash.update(&bar.timestamp.timestamp().to_le_bytes());
        hash.update(&bar.timestamp.timestamp_subsec_nanos().to_le_bytes());
        for value in [bar.open, bar.high, bar.low, bar.close, bar.volume] {
            hash.update(&value.to_le_bytes());
        }
    }
    format!("{:032x}", hash.digest128())
}

impl RunManifest {
    pub fn new(
        series: &PriceSeries,
        spec: RunSpec,
        dataset_metadata: DatasetMetadata,
    ) -> Result<Self> {
        Ok(Self {
            version: 1,
            executable_xxh3_128: executable_hash()?,
            bars_xxh3_128: bars_hash(series),
            spec,
            windows: None,
            dataset_metadata,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_reader(File::open(path)?)?)
    }

    pub fn verify(&self, series: &PriceSeries) -> Result<()> {
        ensure!(self.version == 1, "Unsupported manifest version");
        ensure!(
            self.spec.version == RunSpec::VERSION,
            "Unsupported run spec"
        );
        ensure!(self.bars_xxh3_128 == bars_hash(series), "Dataset mismatch");
        ensure!(
            self.executable_xxh3_128 == executable_hash()?,
            "QBT build mismatch"
        );
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        let mut file = File::create_new(path).context("Choose a new manifest path")?;
        file.write_all(&json)?;
        Ok(())
    }
}

fn resolved(path: &Path) -> Result<PathBuf> {
    if path.try_exists()? {
        return Ok(path.canonicalize()?);
    }
    ensure!(
        fs::symlink_metadata(path).is_err(),
        "Dangling output symlink"
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent
        .canonicalize()?
        .join(path.file_name().context("Expected a file path")?))
}

pub fn distinct_paths(paths: &[Option<&Path>]) -> Result<()> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for path in paths.iter().flatten() {
        let path = resolved(path)?;
        for prior in &seen {
            ensure!(prior != &path, "Choose separate input and output paths");
            if prior.exists() && path.exists() {
                ensure!(
                    !same_file::is_same_file(prior, &path)?,
                    "Paths refer to the same file"
                );
            }
        }
        seen.push(path);
    }
    Ok(())
}
