//! 长度格式化：把米数读起来友好。

/// 把米数格式化成「数值 单位」文本，数值保留一位小数。
pub fn length_format(meters: f64) -> String {
    format!("{meters:.1} m")
}
