//! 观测记录。

use std::collections::HashMap;

/// 一条观测：来源与它记录的值。
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// 观测来源（如传感器或服务名）。
    pub source: String,
    /// 观测值。
    pub value: f64,
}

/// 从文本行解析观测："来源: 值"。
///
/// # Errors
/// 缺冒号或值不是数字时返回错误文本。
pub fn parse_line(line: &str) -> Result<Observation, String> {
    let (source, value) = line
        .split_once(':')
        .ok_or_else(|| format!("缺少 `:`: {line}"))?;
    let value = value
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("`{value}` 不是数字: {error}"))?;
    Ok(Observation {
        source: source.trim().to_owned(),
        value,
    })
}

/// 按来源把观测分组，保留出现顺序。
pub fn group_by_source(observations: &[Observation]) -> HashMap<String, Vec<f64>> {
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();
    for observation in observations {
        groups
            .entry(observation.source.clone())
            .or_default()
            .push(observation.value);
    }
    groups
}
