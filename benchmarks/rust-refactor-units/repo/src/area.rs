//! 面积格式化：把平方米数读起来友好。

/// 把平方米数格式化成「数值 单位」文本，数值保留一位小数。
pub fn area_format(square_meters: f64) -> String {
    format!("{square_meters:.1} m2")
}
