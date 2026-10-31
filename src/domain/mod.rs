//! 领域层：纯业务概念，零 IO、零框架依赖。
//!
//! 这一层的类型只表达业务规则和不变量，不涉及任何具体实现技术。
//! 所有 IO（文件、网络、数据库）都通过 `application/port` 中的 trait 抽象出去。

pub mod ids;
pub mod workspace;
pub mod tool;
pub mod agent;
pub mod session;
pub mod budget;
pub mod evidence;
