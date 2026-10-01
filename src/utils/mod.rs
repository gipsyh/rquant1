use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::{
    Date, OffsetDateTime, UtcOffset,
    macros::{format_description, offset},
};

/// 日期闭区间
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DateRange {
    start: Date,
    end: Date,
}

impl DateRange {
    pub fn new(start: Date, end: Date) -> Self {
        assert!(start <= end, "开始日期不能晚于结束日期");
        Self { start, end }
    }

    pub fn start(&self) -> Date {
        self.start
    }

    pub fn end(&self) -> Date {
        self.end
    }

    pub fn set_start(&mut self, start: Date) {
        self.set(start, self.end);
    }

    pub fn set_end(&mut self, end: Date) {
        self.set(self.start, end);
    }

    /// 同时更新两端；区间无效时 panic，原值保持不变。
    pub fn set(&mut self, start: Date, end: Date) {
        *self = Self::new(start, end);
    }

    pub fn contains(&self, date: Date) -> bool {
        (self.start..=self.end).contains(&date)
    }
}

impl<'de> Deserialize<'de> for DateRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 反序列化也校验区间，避免从缓存文件绕过构造函数的不变量。
        #[derive(Deserialize)]
        #[serde(rename = "DateRange")]
        struct Fields {
            start: Date,
            end: Date,
        }

        let Fields { start, end } = Fields::deserialize(deserializer)?;
        if start > end {
            return Err(serde::de::Error::custom("开始日期不能晚于结束日期"));
        }
        Ok(Self::new(start, end))
    }
}

/// 返回当前可取到行情的最新日期。
/// 按北京时间判断：19:00 及以后返回今天，在此之前返回昨天，不跳过非交易日。
pub fn latest_rqdate() -> Date {
    const DATA_READY_HOUR: u8 = 19;
    const MARKET_TZ: UtcOffset = offset!(+8);

    let now = OffsetDateTime::now_utc().to_offset(MARKET_TZ);
    if now.hour() >= DATA_READY_HOUR {
        now.date()
    } else {
        now.date().previous_day().unwrap()
    }
}

/// 解析 YYYYMMDD 或 YYYY-MM-DD 日期。
pub fn parse_date(value: &str) -> Result<Date> {
    let format = if value.contains('-') {
        format_description!("[year]-[month]-[day]")
    } else {
        format_description!("[year][month][day]")
    };
    Date::parse(value, format).with_context(|| format!("日期 {value:?} 无效"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn date_range_allows_equal_bounds_and_updates() {
        let first = date!(2024 - 01 - 01);
        let last = date!(2024 - 01 - 03);
        let mut range = DateRange::new(first, first);
        assert!(range.contains(first));
        assert!(!range.contains(last));
        range.set_end(last);
        assert!(range.contains(first));
        assert!(range.contains(last));
        range.set_start(last);
        assert_eq!((range.start(), range.end()), (last, last));
        // 两端一起移动，无须经过不合法的中间状态。
        let later = date!(2024 - 02 - 01);
        range.set(later, later);
        assert_eq!((range.start(), range.end()), (later, later));
    }

    #[test]
    fn parses_both_date_formats() {
        let expected = time::macros::date!(2024 - 02 - 29);
        assert_eq!(parse_date("20240229").unwrap(), expected);
        assert_eq!(parse_date("2024-02-29").unwrap(), expected);
    }

    #[test]
    fn invalid_dates_preserve_input_and_parse_error() {
        for input in ["20230229", "2024-13-01", "not-a-date", ""] {
            let err = parse_date(input).unwrap_err();
            assert!(err.to_string().contains(&format!("{input:?}")));
            assert!(err.downcast_ref::<time::error::Parse>().is_some());
        }
    }
}
