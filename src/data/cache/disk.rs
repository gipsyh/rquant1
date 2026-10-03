use super::mem::MemCacheProvider;
use crate::data::{DataProvider, IndexHistComp, RqData, StockBar, StockSymbol};
use anyhow::{Context, Result, ensure};
use std::{io::Write, path::PathBuf};
use time::Date;

/// 组合 `MemCacheProvider` 复用查询和区间补拉逻辑，增加跨回测日线与指数成分持久化。
/// 交易日历仍由底层数据源提供。
///
/// 构造时读取当前工作目录的 `rqdata.ron`，不存在则创建空缓存；
/// 每次 Drop 都重写文件。一个文件只供一个存活的实例使用，不合并并发写入。
/// 数据源及其复权口径变化时，应先删除旧缓存。
pub struct DiskCacheProvider {
    inner: MemCacheProvider,
    path: PathBuf,
}

impl DiskCacheProvider {
    const FILE_NAME: &str = "rqdata.ron";

    /// 首次访问未缓存股票或指数时预取 `start..=end`，请求更宽时自动补拉。
    /// 缓存读取失败、损坏或数据不一致时 panic，保留原文件。
    pub fn new(provider: Box<dyn DataProvider>, start: Date, end: Date) -> Self {
        let path = std::env::current_dir()
            .expect("无法获取磁盘缓存工作目录")
            .join(Self::FILE_NAME);
        Self::with_path(provider, start, end, path)
            .unwrap_or_else(|err| panic!("初始化磁盘缓存失败: {err:#}"))
    }

    fn with_path(
        provider: Box<dyn DataProvider>,
        start: Date,
        end: Date,
        path: PathBuf,
    ) -> Result<Self> {
        let mut inner = MemCacheProvider::new(provider, start, end);
        let exists = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let data: RqData = ron::from_str(&text)
                    .with_context(|| format!("解析 {} 失败", path.display()))?;
                ensure!(
                    data.stock
                        .iter()
                        .all(|(symbol, stock)| stock.symbol == *symbol),
                    "缓存股票信息无效: 股票代码与键不匹配"
                );
                ensure!(
                    data.stock_bar_date.len() == data.stock_bars.len(),
                    "缓存行情无效: bars 与 bar_date 的股票键不一致"
                );
                for (symbol, bars) in &data.stock_bars {
                    let range = data
                        .stock_bar_date
                        .get(symbol)
                        .with_context(|| format!("缓存行情无效: {symbol} 缺少 bar_date"))?;
                    ensure!(
                        bars.iter()
                            .all(|bar| { bar.symbol == *symbol && range.contains(bar.date) })
                            && bars.windows(2).all(|pair| pair[0].date < pair[1].date),
                        "缓存行情无效: {symbol} 的日期范围、股票代码或日期顺序不正确"
                    );
                }
                for (symbol, index) in &data.index {
                    ensure!(
                        !index.name.trim().is_empty(),
                        "缓存指数名称不能为空: {symbol}"
                    );
                    ensure!(
                        index.symbol == *symbol,
                        "缓存指数信息无效: 指数代码与键不匹配"
                    );
                    ensure!(
                        crate::data::index::normalize_index_symbol(symbol)? == *symbol,
                        "缓存指数代码未规范化: {symbol}"
                    );
                    index
                        .comp
                        .validate()
                        .with_context(|| format!("缓存指数成分无效: {symbol}"))?;
                }
                inner.data = data;
                true
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err).with_context(|| format!("读取 {} 失败", path.display())),
        };
        let cache = Self { inner, path };
        if !exists {
            cache.save()?;
        }
        Ok(cache)
    }

    fn save(&self) -> Result<()> {
        let text = ron::ser::to_string_pretty(&self.inner.data, ron::ser::PrettyConfig::default())?;
        // 同目录临时文件，完整写入后再原子替换，避免写入失败截断旧缓存。
        let parent = self.path.parent().context("缓存路径没有父目录")?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        Ok(())
    }
}

impl Drop for DiskCacheProvider {
    fn drop(&mut self) {
        // Drop 无法返回错误；避免在错误展开期间再次 panic。
        if let Err(err) = self.save() {
            log::error!("写入磁盘缓存 {} 失败: {err:#}", self.path.display());
        }
    }
}

#[async_trait::async_trait]
impl DataProvider for DiskCacheProvider {
    async fn trading_days(&mut self, start: Date, end: Date) -> Vec<Date> {
        self.inner.trading_days(start, end).await
    }

    async fn stock_bar(&mut self, symbol: StockSymbol, start: Date, end: Date) -> Vec<StockBar> {
        self.inner.stock_bar(symbol, start, end).await
    }

    async fn index_name(&mut self, symbol: &str) -> String {
        self.inner.index_name(symbol).await
    }

    async fn index_comp(&mut self, symbol: &str, start: Date, end: Date) -> IndexHistComp {
        self.inner.index_comp(symbol, start, end).await
    }
}

#[cfg(test)]
mod test;
