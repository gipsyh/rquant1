//! 报告后端只消费回测结果，不读取行情或运行策略。

use crate::engine::BacktestResult;
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
};

pub trait RqReporter {
    /// 在回测前检查依赖；不需要外部环境的后端无需实现。
    fn check_available(&self) -> Result<()> {
        Ok(())
    }

    /// 在输出目录中生成报告；目录不存在时自动创建。
    fn render(&self, result: &BacktestResult, output_dir: &Path) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
pub enum ReporterKind {
    #[default]
    #[value(name = "quantstats")]
    QuantStats,
}

impl ReporterKind {
    pub fn build(self) -> Box<dyn RqReporter> {
        match self {
            Self::QuantStats => Box::new(QuantStatsReporter::default()),
        }
    }
}

/// 每次回测独立保存；先写 JSON，后端生成失败时仍保留完整回测结果。
pub fn save_report(result: &BacktestResult, reporter: &dyn RqReporter) -> Result<PathBuf> {
    let now = time::OffsetDateTime::now_utc().to_offset(time::macros::offset!(+8));
    let directory = create_run_directory(&result.config.report_output, &result.strategy, now)?;
    let json = directory.join("result.json");
    let mut writer = BufWriter::new(std::fs::File::create(&json)?);
    serde_json::to_writer_pretty(&mut writer, result)?;
    writer.flush()?;
    reporter
        .render(result, &directory)
        .with_context(|| format!("生成报告失败，完整回测结果已保存至 {}", json.display()))?;
    Ok(directory)
}

fn create_run_directory(
    root: &Path,
    strategy: &str,
    timestamp: time::OffsetDateTime,
) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    // 策略名只用于单层目录名，避免自定义名称中的路径分隔符改变输出位置。
    let name: String = strategy
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = if name.is_empty() { "strategy" } else { &name };
    let timestamp = timestamp.format(time::macros::format_description!(
        "[year][month][day]-[hour][minute][second]"
    ))?;
    let prefix = format!("{name}-{timestamp}");
    // 原子创建目录；同秒启动的任务以序号区分，绝不复用已有结果目录。
    for sequence in 0u64.. {
        let name = if sequence == 0 {
            prefix.clone()
        } else {
            format!("{prefix}-{sequence}")
        };
        let path = root.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("创建报告目录 {} 失败", path.display()));
            }
        }
    }
    unreachable!()
}

/// 优先使用当前项目通过 uv 管理的环境，否则使用 PATH 中的 python3。
fn default_python() -> PathBuf {
    let local = PathBuf::from(if cfg!(windows) {
        ".venv/Scripts/python.exe"
    } else {
        ".venv/bin/python"
    });
    if local.is_file() {
        local
    } else {
        "python3".into()
    }
}

pub struct QuantStatsReporter {
    python: PathBuf,
}

impl Default for QuantStatsReporter {
    fn default() -> Self {
        Self {
            python: default_python(),
        }
    }
}

impl QuantStatsReporter {
    const SCRIPT: &str = include_str!("quantstats.py");

    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<()> {
        let output = Command::new(&self.python)
            .arg("-c")
            .arg(Self::SCRIPT)
            .args(args)
            .env("MPLBACKEND", "Agg")
            .output()
            .with_context(|| format!("启动 Python {} 失败", self.python.display()))?;
        ensure!(
            output.status.success(),
            "QuantStats 报告失败 ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }
}

impl RqReporter for QuantStatsReporter {
    fn check_available(&self) -> Result<()> {
        self.run(&[std::ffi::OsStr::new("--check")])
            .context("请先运行 uv sync，在项目 .venv 中安装 QuantStats")
    }

    fn render(&self, result: &BacktestResult, output_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("创建报告目录 {} 失败", output_dir.display()))?;
        let output = output_dir.join("report.html");
        let mut input = tempfile::NamedTempFile::new()?;
        {
            let mut writer = BufWriter::new(input.as_file_mut());
            serde_json::to_writer(&mut writer, result)?;
            writer.flush()?;
        }
        // Python 失败时保留已有报告；成功后才替换目标文件。
        let html = tempfile::NamedTempFile::new_in(output_dir)?;
        self.run(&[input.path().as_os_str(), html.path().as_os_str()])?;
        ensure!(
            html.as_file().metadata()?.len() > 0,
            "QuantStats 未生成 HTML"
        );
        html.persist(&output)
            .with_context(|| format!("保存报告 {} 失败", output.display()))?;
        Ok(())
    }
}
