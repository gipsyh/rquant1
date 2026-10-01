//! tushare 返回结果的列式容器。

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::error::TushareError;

/// tushare 的返回结果，等价于 Python 侧的
/// `pd.DataFrame(data['items'], columns=data['fields'])`（`client.py:47-50`）。
///
/// 严格照搬线格式：列名 + 行数组，每行是等长的 JSON value 数组。
/// 不做类型推断 —— JSON 数字没有 int/float 之分，具体口径由取值方法决定。
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    fields: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Table {
    /// 组表，并校验每行长度与列数一致。
    ///
    /// tushare 正常总是返回等长行；长度不一致说明响应被截断或字段错位。
    /// 这属于必须暴露的结构异常，不能像 Python 那样让它悄悄变成 NaN 列。
    pub fn new(fields: Vec<String>, rows: Vec<Vec<Value>>) -> Result<Self, TushareError> {
        for (index, row) in rows.iter().enumerate() {
            if row.len() != fields.len() {
                return Err(TushareError::Shape(format!(
                    "第 {index} 行有 {} 个值，但 fields 有 {} 列（{}）",
                    row.len(),
                    fields.len(),
                    fields.join(",")
                )));
            }
        }
        Ok(Self { fields, rows })
    }

    /// 列名，顺序与线格式一致。
    pub fn fields(&self) -> &[String] {
        &self.fields
    }

    /// 行数，对应 `len(df)`。
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 列名 -> 列下标。缺列返回 `None`（对应 Python 的 `KeyError`）。
    pub fn index_of(&self, field: &str) -> Option<usize> {
        self.fields.iter().position(|name| name == field)
    }

    /// 取一列，对应 `df["col"]`。
    pub fn column(&self, field: &str) -> Option<Column<'_>> {
        let index = self.index_of(field)?;
        Some(Column {
            name: &self.fields[index],
            rows: &self.rows,
            index,
        })
    }

    /// 取一行。
    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        let values = self.rows.get(index)?;
        Some(Row {
            fields: &self.fields,
            values,
        })
    }

    /// 取单元格，对应 `df.at[row, col]`。
    pub fn get(&self, row: usize, field: &str) -> Option<&Value> {
        let index = self.index_of(field)?;
        self.rows.get(row)?.get(index)
    }

    /// 逐行转成 JSON 对象，对应 `df.to_dict('records')`。
    pub fn records(&self) -> Vec<Map<String, Value>> {
        self.rows
            .iter()
            .map(|values| zip_record(&self.fields, values))
            .collect()
    }

    /// 逐行反序列化成调用方指定的类型，对应「按行构造 dataclass」的写法。
    ///
    /// 配合 `#[derive(Deserialize)]` 的结构体使用；列名即字段名，
    /// 可空列写成 `Option<T>`。额外的列会被忽略，缺失的列报错。
    ///
    /// # 与 [`Column::as_f64`] 的区别：这里是严格的
    ///
    /// 走 serde 的标准语义，**不做类型转换**：数字字符串 `"3400"` 反序列化成
    /// `f64` 会失败，而不是像 [`Column::as_f64`] 那样解析成功。
    ///
    /// 两者刻意不同：[`Column::as_f64`] 对标 `pd.to_numeric(errors="coerce")`
    /// 的宽松口径，适合逐列取值；`to_typed` 保持严格，避免像 `"000001"` 这样的
    /// 代码被悄悄当成数字 1。类型不符时按行报错并给出行号，不会静默丢数据。
    pub fn to_typed<T: DeserializeOwned>(&self) -> Result<Vec<T>, TushareError> {
        self.rows
            .iter()
            .enumerate()
            .map(|(row, values)| {
                serde_json::from_value(Value::Object(zip_record(&self.fields, values)))
                    .map_err(|source| TushareError::Deserialize { row, source })
            })
            .collect()
    }
}

fn zip_record(fields: &[String], values: &[Value]) -> Map<String, Value> {
    fields.iter().cloned().zip(values.iter().cloned()).collect()
}

/// 一列数据的视图，对应 `df["col"]`。
///
/// 取值方法统一把缺失值映射成 `None`，对齐 Python 里 `pd.isna` 的用法。
#[derive(Debug, Clone, Copy)]
pub struct Column<'a> {
    name: &'a str,
    rows: &'a [Vec<Value>],
    index: usize,
}

impl<'a> Column<'a> {
    pub fn name(&self) -> &'a str {
        self.name
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 单元格原始值；`None` 表示该行缺少这一列。
    pub fn get(&self, row: usize) -> Option<&'a Value> {
        self.rows.get(row)?.get(self.index)
    }

    /// 按行迭代原始值。
    pub fn values(&self) -> impl Iterator<Item = Option<&'a Value>> {
        self.rows.iter().map(move |row| row.get(self.index))
    }

    /// 等价于 `pd.to_numeric(col, errors="coerce")`。
    ///
    /// JSON 数字直接取；数字字符串也解析（tushare 有字段以字符串返回）；
    /// `null` 或无法解析 → `None`。这是**宽松**口径，与严格的
    /// [`Table::to_typed`] 不同，见那里的说明。
    pub fn as_f64(&self) -> Vec<Option<f64>> {
        self.values()
            .map(|value| match value? {
                Value::Number(number) => number.as_f64(),
                Value::String(text) => text.trim().parse::<f64>().ok(),
                _ => None,
            })
            .collect()
    }

    /// 取整数；非整数值返回 `None`。
    pub fn as_i64(&self) -> Vec<Option<i64>> {
        self.values()
            .map(|value| match value? {
                Value::Number(number) => number.as_i64(),
                Value::String(text) => text.trim().parse::<i64>().ok(),
                _ => None,
            })
            .collect()
    }

    /// 只接受 JSON 字符串；数字等其它类型一律 `None`。
    ///
    /// tushare 的 `ts_code` / `trade_date` / `cal_date` 都以字符串返回。
    pub fn as_str(&self) -> Vec<Option<&'a str>> {
        self.values()
            .map(|value| match value? {
                Value::String(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 等价于 `.astype(str)`：数字、布尔也一并转成字符串。
    pub fn as_string(&self) -> Vec<Option<String>> {
        self.values()
            .map(|value| match value? {
                Value::String(text) => Some(text.clone()),
                Value::Bool(flag) => Some(flag.to_string()),
                Value::Number(number) => Some(number.to_string()),
                // null 与嵌套结构没有合理的字符串形式，按缺失处理。
                Value::Null | Value::Array(_) | Value::Object(_) => None,
            })
            .collect()
    }

    pub fn as_bool(&self) -> Vec<Option<bool>> {
        self.values()
            .map(|value| match value? {
                Value::Bool(flag) => Some(*flag),
                // 实测 trade_cal.is_open 以数字 1/0 返回。
                Value::Number(number) => number.as_i64().map(|n| n != 0),
                Value::String(text) => match text.trim() {
                    "1" | "true" | "True" => Some(true),
                    "0" | "false" | "False" => Some(false),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

/// 一行的视图。
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    fields: &'a [String],
    values: &'a [Value],
}

impl<'a> Row<'a> {
    /// 按列名取值；缺列返回 `None`。
    pub fn get(&self, field: &str) -> Option<&'a Value> {
        let index = self.fields.iter().position(|name| name == field)?;
        self.values.get(index)
    }

    /// 按列序取值。
    pub fn at(&self, index: usize) -> Option<&'a Value> {
        self.values.get(index)
    }

    pub fn values(&self) -> &'a [Value] {
        self.values
    }

    pub fn to_map(&self) -> Map<String, Value> {
        zip_record(self.fields, self.values)
    }
}
