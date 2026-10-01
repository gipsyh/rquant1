//! `data` 模块的测试。
//!
//! ```text
//! cargo test data::test
//! ```

use super::*;

/// 六位代码 + 期望的板块；用来同时覆盖号段表与 Display。
const CASES: &[(&str, InstrType)] = &[
    ("600000", InstrType::ShMain),    // 浦发银行
    ("601398", InstrType::ShMain),    // 工商银行
    ("603259", InstrType::ShMain),    // 药明康德
    ("605499", InstrType::ShMain),    // 东鹏饮料
    ("688981", InstrType::ShStar),    // 中芯国际
    ("689009", InstrType::ShStar),    // 九号公司（CDR）
    ("000001", InstrType::SzMain),    // 平安银行
    ("002594", InstrType::SzMain),    // 比亚迪（原中小板）
    ("003816", InstrType::SzMain),    // 中国广核
    ("300750", InstrType::SzChiNext), // 宁德时代
    ("301029", InstrType::SzChiNext), // 怡合达
];

#[test]
fn 裸代码按号段推断板块() {
    for (code, expected) in CASES {
        let symbol = InstrSymbol::from(*code);
        assert_eq!(symbol.tp, *expected, "{code} 的板块");
        assert_eq!(symbol.id, code.parse::<u32>().unwrap(), "{code} 的 id");
    }
}

#[test]
fn 与_display_互为逆运算() {
    for (code, _) in CASES {
        let symbol = InstrSymbol::from(*code);
        // Display 产出 "000001.XSHE" 形式，再解析须回到原值。
        assert_eq!(
            InstrSymbol::from(symbol.to_string().as_str()),
            symbol,
            "{code}"
        );
    }
}

#[test]
fn 各种写法都归一到同一形式() {
    // 裸代码、两位缩写（SH/SZ）、四位 RQAlpha 后缀（XSHG/XSHE），
    // 外加大小写与两端空白，全部归一。
    let cases = [
        ("000001", "000001.XSHE"),
        ("000001.XSHE", "000001.XSHE"),
        ("000001.xshe", "000001.XSHE"),
        ("000001.SZ", "000001.XSHE"),
        ("000001.sz", "000001.XSHE"),
        ("  000001.XSHE  ", "000001.XSHE"),
        ("  000001.SZ  ", "000001.XSHE"),
        ("600000", "600000.XSHG"),
        ("600000.XSHG", "600000.XSHG"),
        ("600000.SH", "600000.XSHG"),
        ("688981.sh", "688981.XSHG"),
        ("300750.SZ", "300750.XSHE"),
        ("301029.SZ", "301029.XSHE"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            InstrSymbol::from(input).to_string(),
            expected,
            "输入 {input:?}"
        );
    }
}

#[test]
fn 两位缩写与四位后缀解析结果相同() {
    for (short, long) in [
        ("000001.SZ", "000001.XSHE"),
        ("600000.SH", "600000.XSHG"),
        ("688981.SH", "688981.XSHG"),
        ("300750.SZ", "300750.XSHE"),
    ] {
        assert_eq!(InstrSymbol::from(short), InstrSymbol::from(long), "{short} vs {long}");
    }
}

#[test]
fn 六位以外的位数被拒绝() {
    for bad in ["00001", "0000001", "", "0000012"] {
        assert!(
            std::panic::catch_unwind(|| InstrSymbol::from(bad)).is_err(),
            "{bad:?} 应当 panic"
        );
    }
}

#[test]
fn 非数字被拒绝() {
    for bad in ["00000A", "ABCDEF", "000-01"] {
        assert!(
            std::panic::catch_unwind(|| InstrSymbol::from(bad)).is_err(),
            "{bad:?} 应当 panic"
        );
    }
}

#[test]
fn 未建模的号段被拒绝() {
    // 北交所 4xxxxx / 920xxx、沪市 B 股 900xxx、深市 B 股 200xxx：
    // Python 侧 _STOCK_PREFIXES 认这些号段，但 InstrType 还没有对应变体。
    for bad in ["430047", "920001", "900901", "200002"] {
        assert!(
            std::panic::catch_unwind(|| InstrSymbol::from(bad)).is_err(),
            "{bad:?} 应当 panic"
        );
    }
}

#[test]
fn 后缀与号段矛盾时被拒绝() {
    for bad in [
        // 000001.XSHG 在 Python 侧是上证综指（指数），不是深市股票。
        "000001.XSHG",
        "000001.SH",
        // 反过来也不行：深市后缀配沪市号段。
        "600000.XSHE",
        "600000.SZ",
        "300750.XSHG",
        "300750.SH",
        // 指数后缀（CSI/INDX）没有对应变体，同样拒绝。
        "000300.INDX",
    ] {
        assert!(
            std::panic::catch_unwind(|| InstrSymbol::from(bad)).is_err(),
            "{bad:?} 应当 panic"
        );
    }
}
