//! bench-units：长度与面积两个领域各自的格式化入口。
//!
//! 基线：length 与 area 各自内联了同一份「数值一位小数 + 单位」的格式化
//! 逻辑，代码重复。验收契约（tests/units.rs）要求抽出一份共享实现
//! `units_fmt` 供两处调用，但基线还没有这个模块，测试引用它编译不过。

pub mod area;
pub mod length;
