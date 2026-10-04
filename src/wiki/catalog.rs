use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use calamine::{Reader, open_workbook_auto};
use csv::{ReaderBuilder, Trim};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FactorDefinition {
    pub sequence: String,
    pub category: String,
    pub english_name: String,
    pub chinese_name: String,
    pub description: String,
    pub formula: String,
    pub scenario: String,
    pub evaluation: String,
    pub required_inputs: String,
    pub polars_expression: String,
}

pub struct FactorCatalogReader;

impl FactorCatalogReader {
    /// Reads a Python-compatible factor catalog from CSV or XLSX.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported formats, workbook/CSV failures, or a
    /// workbook without a readable first worksheet.
    pub fn read(path: &Path) -> Result<Vec<FactorDefinition>> {
        match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "csv" => read_csv(path),
            "xlsx" | "xls" | "xlsb" | "ods" => read_workbook(path),
            extension => bail!("unsupported factor catalog: .{extension}"),
        }
    }
}

fn read_csv(path: &Path) -> Result<Vec<FactorDefinition>> {
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .trim(Trim::All)
        .from_path(path)
        .with_context(|| format!("failed to open factor catalog {}", path.display()))?;
    let headers = reader
        .headers()
        .with_context(|| format!("failed to read headers from {}", path.display()))?
        .clone();
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.with_context(|| format!("invalid CSV row in {}", path.display()))?;
        if record.iter().any(|value| !value.trim().is_empty()) {
            rows.push(definition(headers.iter().zip(record.iter()).map(
                |(header, value)| {
                    (
                        header.trim_start_matches('\u{feff}').to_string(),
                        value.to_string(),
                    )
                },
            )));
        }
    }
    Ok(rows)
}

fn read_workbook(path: &Path) -> Result<Vec<FactorDefinition>> {
    let mut workbook = open_workbook_auto(path)
        .with_context(|| format!("failed to open factor workbook {}", path.display()))?;
    let sheet = workbook
        .sheet_names()
        .first()
        .cloned()
        .context("factor workbook has no worksheets")?;
    let range = workbook
        .worksheet_range(&sheet)
        .with_context(|| format!("failed to read worksheet {sheet:?}"))?;
    let mut rows = range.rows();
    let headers = rows
        .next()
        .unwrap_or_default()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    Ok(rows
        .filter(|row| row.iter().any(|value| !value.to_string().trim().is_empty()))
        .map(|row| {
            definition(
                headers
                    .iter()
                    .cloned()
                    .zip(row.iter().map(ToString::to_string)),
            )
        })
        .collect())
}

fn definition(values: impl Iterator<Item = (String, String)>) -> FactorDefinition {
    let mapped = values
        .filter_map(|(header, value)| {
            header_field(&normalize_header(&header)).map(|field| (field, value.trim().to_string()))
        })
        .collect::<HashMap<_, _>>();
    FactorDefinition {
        sequence: field(&mapped, "sequence"),
        category: field(&mapped, "category"),
        english_name: field(&mapped, "english_name"),
        chinese_name: field(&mapped, "chinese_name"),
        description: field(&mapped, "description"),
        formula: field(&mapped, "formula"),
        scenario: field(&mapped, "scenario"),
        evaluation: field(&mapped, "evaluation"),
        required_inputs: field(&mapped, "required_inputs"),
        polars_expression: field(&mapped, "polars_expression"),
    }
}

fn field(values: &HashMap<&'static str, String>, name: &'static str) -> String {
    values.get(name).cloned().unwrap_or_default()
}

fn normalize_header(value: &str) -> String {
    value
        .trim_start_matches('\u{feff}')
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_lowercase()
}

fn header_field(header: &str) -> Option<&'static str> {
    match header {
        "序号" => Some("sequence"),
        "因子类别" => Some("category"),
        "因子名称(英文)" | "因子名称" => Some("english_name"),
        "因子名称(中文)" => Some("chinese_name"),
        "因子说明" => Some("description"),
        "因子公式" => Some("formula"),
        "因子适用场景" => Some("scenario"),
        "因子评价" => Some("evaluation"),
        "所需输入数据" => Some("required_inputs"),
        "polars表达式" => Some("polars_expression"),
        _ => None,
    }
}
