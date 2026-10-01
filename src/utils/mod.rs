use anyhow::{Context, Result};
use time::{Date, macros::format_description};

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
