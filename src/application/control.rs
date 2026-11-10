//! 运行期间接收补充输入并检查外部执行状态。

use serde::{Deserialize, Serialize};

/// 一条可重试的用户补充输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInput {
    pub id: String,
    pub text: String,
}

/// 由宿主提供输入队列与运行状态检查。
///
/// `take_inputs` 可以保留尚未确认的输入；runner 在快照保存后调用
/// `input_applied`，并按 ID 对重复投递去重。
pub trait ExecutionControl: Send + Sync {
    fn take_inputs(&self) -> Vec<UserInput>;
    fn input_applied(&self, input: &UserInput) -> Result<(), String>;
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}
