//! bench-units 的重构验收测试。
//!
//! 契约：length 与 area 共用的「数值保留一位小数 + 单位」格式化逻辑，
//! 必须抽到 `units_fmt` 共享模块里；两处调用方改走共享实现，行为不变。
//! 基线没有 `units_fmt`，本文件引用它编译不过，`cargo test` 失败。

use bench_units::area;
use bench_units::length;
use bench_units::units_fmt;

#[test]
fn shared_formatter_exposed_and_correct() {
    assert_eq!(units_fmt::with_unit(5.0, "m"), "5.0 m");
    assert_eq!(units_fmt::with_unit(7.77, "m2"), "7.8 m2");
}

// 行为保持不变：抽走共享逻辑后，两个调用方的输出与之前一致。
#[test]
fn length_format_preserves_behavior() {
    assert_eq!(length::length_format(5.0), "5.0 m");
    assert_eq!(length::length_format(7.77), "7.8 m");
}

#[test]
fn area_format_preserves_behavior() {
    assert_eq!(area::area_format(3.0), "3.0 m2");
}
