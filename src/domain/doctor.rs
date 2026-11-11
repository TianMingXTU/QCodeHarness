//! 只读诊断的纯数据契约：状态、单项结论、完整报告与采集到的事实。
//!
//! 本模块零 IO，且不依赖 application 或 infrastructure：只定义诊断报告的
//! 形状、采集事实的结构，以及快照版本的纯分类函数。真实读取文件、探测
//! 租约锁的操作在 infrastructure/doctor，把事实判定为各项结论的纯逻辑在
//! application/doctor。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::sandbox::PlatformCapabilities;

/// 诊断项状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorStatus {
    /// 检查通过。
    Ok,
    /// 可用性受限，但不阻断运行。
    Warn,
    /// 当前配置会导致运行失败。
    Error,
    /// 现有数据不足以形成结论。
    Unknown,
}

/// 一项诊断结果。`code` 是稳定码，调用方不解析 `message` 来分支。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// 稳定诊断码。
    pub code: String,
    /// 状态。
    pub status: DoctorStatus,
    /// 人读说明，不含凭据值。
    pub message: String,
    /// 相关路径；没有对应文件时为 `None`。
    #[serde(default)]
    pub path: Option<String>,
    /// 可执行建议。
    #[serde(default)]
    pub recommendation: Option<String>,
}

/// 完整的只读诊断报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// 报告格式版本；诊断字段扩展时加 1。
    pub schema_version: u32,
    /// 被诊断的工作区。
    pub workspace: String,
    /// 逐项检查。
    pub checks: Vec<DoctorCheck>,
}

/// 当前诊断报告格式版本。
pub const DOCTOR_FORMAT: u32 = 1;

/// 配置加载事实：判定用的路径与失败原因。
///
/// `error` 有值表示配置加载失败（无论该文件是否必需，损坏都要报错）；
/// `error` 为 `None` 而 `path` 也有值表示配置已成功加载。`path` 为 `None`
/// 表示没有任何配置来源。`required` 记录是否存在必须存在的来源（显式
/// `QCH_CONFIG`）：它决定"完全没配置"是正常缺省还是错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFacts {
    /// 参与判定的配置文件路径：显式指定优先，其次项目级，最后用户级。
    pub path: Option<PathBuf>,
    /// 配置加载失败的原因文本；成功或缺省时为 `None`。
    pub error: Option<String>,
    /// 是否存在必须存在的配置来源（显式 `QCH_CONFIG` 指向的文件）。
    pub required: bool,
}

/// 模型连接事实：环境与配置合并后的结果。
///
/// `api_key` 只表示变量是否存在，事实里绝不保存密钥值本身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFacts {
    /// 模型服务根地址；环境与配置都无法提供时为 `None`。
    pub base_url: Option<String>,
    /// 模型名称；环境与配置都无法提供时为 `None`。
    pub model: Option<String>,
    /// 进程环境中是否存在 `LLM_API_KEY`。
    pub api_key: bool,
}

/// 快照版本事实：按目录分别统计 `format_version`，以及无法解析的文件。
///
/// 运行快照与任务快照存放目录不同、版本独立演进，各自与自己的当前格式
/// 比较。字段名与 [`SnapshotFacts::run_class`]/[`SnapshotFacts::task_class`]
/// 对应：采集方按目录分别填写，不混在一起判断。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotFacts {
    /// 运行快照（`.qcodeharness/snapshots/`）版本到该版本的文件数。
    pub run_versions: BTreeMap<u64, usize>,
    /// 任务快照（`.qcodeharness/tasks/`）版本到该版本的文件数。
    pub task_versions: BTreeMap<u64, usize>,
    /// 无法解析出 `format_version` 的文件路径。
    pub corrupt: Vec<String>,
}

/// 一份目录内快照版本的判定结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionClass {
    /// 该目录没有快照，无法形成结论（不是损坏）。
    Unknown,
    /// 全部快照都是当前格式。
    Ok,
    /// 出现了不是当前格式的版本，恢复时会被拒绝。
    Incompatible,
}

/// 事件日志回放事实：由 infrastructure 逐行读取后填写的结论。
///
/// `contiguous` 为 true 表示序号从 1 连续到 `count` 且协议版本匹配；
/// `corrupt` 记录首个破坏连续性的原因。`read_error` 优先于其余字段：
/// 文件读不出来时回放结论不可知，不应据此报损坏。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JournalFacts {
    /// 事件日志文件是否存在。不存在是常态：从未启动过常驻服务。
    pub exists: bool,
    /// 读取文件失败的原因；能读出来时为 `None`。
    pub read_error: Option<String>,
    /// 成功解析且序号连续的事件条数。
    pub count: u64,
    /// 序号是否从 1 连续无缺口、版本是否匹配。
    pub contiguous: bool,
    /// 首个破坏回放的原因（坏行、版本不符或序号断档）。
    pub corrupt: Option<String>,
}

/// 服务租约探测事实。
///
/// 租约文件存在不代表锁被持有：可能是崩溃残留。真正的持有状态靠
/// `try_lock` 探测，探测失败时持有状态不可知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeaseFacts {
    /// 租约文件是否存在。
    pub lock_present: bool,
    /// `Some(true)` 锁被占用（服务很可能在运行）；`Some(false)` 锁空闲
    /// （崩溃残留，确认无进程后可删）；`None` 探测本身出错，结论不可知。
    pub lock_held: Option<bool>,
}

/// 服务状态文件事实：版本与可读性。
///
/// `schema_version` 缺字段与显式 `0` 都按 v0 处理，与状态文件的迁移
/// 语义一致；`read_error` 表示文件存在但读不出或不是合法 JSON。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateFacts {
    /// 状态文件是否存在。不存在是常态：从未启动过常驻服务。
    pub exists: bool,
    /// 读取或解析失败的原因；成功时为 `None`。
    pub read_error: Option<String>,
    /// 文件声明的 `schema_version`；文件不存在、缺字段或读失败时为 `None`。
    pub schema_version: Option<u64>,
}

/// 诊断一次采集到的全部事实。
///
/// 纯数据载体：字段由 infrastructure 填写，application 只读取并判定。
/// 刻意不实现 `Default`，避免平台能力这类事实在测试里被静默替换成假的
/// 默认值。
#[derive(Debug, Clone)]
pub struct DoctorFacts {
    /// 被诊断的工作区根目录。
    pub workspace: PathBuf,
    /// 配置加载事实。
    pub config: ConfigFacts,
    /// 模型连接事实。
    pub model: ModelFacts,
    /// 解析出的策略文件路径；未指定外部策略时为 `None`。
    pub policy_path: Option<PathBuf>,
    /// 策略文件解析失败的原因；未指定外部策略或解析成功时为 `None`。
    ///
    /// 路径存在但内容非法必须显式报错，否则 doctor 会把一个用不了的策略
    /// 文件报成正常。
    pub policy_parse_error: Option<String>,
    /// 解析出的 MCP 配置路径；未配置 MCP 时为 `None`。
    pub mcp_path: Option<PathBuf>,
    /// MCP 配置解析失败的原因；未配置或解析成功时为 `None`。
    pub mcp_parse_error: Option<String>,
    /// `QCH_AUTO_APPROVE` 的原始值；未设置时为 `None`。
    pub auto_approve_raw: Option<String>,
    /// 当前平台具备的隔离能力。
    pub capabilities: PlatformCapabilities,
    /// 快照版本事实。
    pub snapshots: SnapshotFacts,
    /// 事件日志回放事实。
    pub journal: JournalFacts,
    /// 服务租约事实。
    pub lease: LeaseFacts,
    /// 服务状态文件事实。
    pub state: StateFacts,
    /// 事件日志当前字节数。
    pub journal_bytes: u64,
}

impl SnapshotFacts {
    /// 用一份快照事实自带的版本分布判定结论，不需要调用方传当前格式。
    ///
    /// 这里只用领域层已知的"文件里是否只有一个版本"这一结构事实：混合
    /// 版本必然不兼容，单一版本是否兼容交给调用方按目录各自比较当前格式
    /// （见 [`snapshot_version_status`]）。
    pub fn run_class(&self, current_run_format: u32) -> VersionClass {
        version_class(&self.run_versions, current_run_format)
    }

    /// 任务快照目录的版本判定，与运行快照各自独立。
    pub fn task_class(&self, current_task_format: u32) -> VersionClass {
        version_class(&self.task_versions, current_task_format)
    }
}

/// 单个目录的版本分布判定：把版本计数表归类为未知/正常/不兼容。
///
/// 纯函数，便于对"全部当前版本""混入旧版本""只有旧版本""空目录"四种
/// 形态分别做单元测试，不需要真实文件。
fn version_class(versions: &BTreeMap<u64, usize>, current_format: u32) -> VersionClass {
    if versions.is_empty() {
        return VersionClass::Unknown;
    }
    let all_current = versions
        .keys()
        .all(|version| *version == current_format as u64);
    if all_current {
        VersionClass::Ok
    } else {
        VersionClass::Incompatible
    }
}

/// 综合 corruption、运行与任务两个目录的版本判定，给出快照整体结论。
///
/// 任一损坏文件或任一目录不兼容即 Error；两个目录都空（从未保存过快照）
/// 时 Unknown；否则 Ok。运行与任务目录分别和自己的当前格式比较，互不
/// 牵扯。
pub fn snapshot_version_status(
    facts: &SnapshotFacts,
    current_run_format: u32,
    current_task_format: u32,
) -> (DoctorStatus, usize, usize) {
    let run_class = facts.run_class(current_run_format);
    let task_class = facts.task_class(current_task_format);
    let incompatible = matches!(run_class, VersionClass::Incompatible)
        || matches!(task_class, VersionClass::Incompatible);
    let status = if !facts.corrupt.is_empty() || incompatible {
        DoctorStatus::Error
    } else if run_class == VersionClass::Unknown && task_class == VersionClass::Unknown {
        DoctorStatus::Unknown
    } else {
        DoctorStatus::Ok
    };
    (status, facts.run_versions.values().sum(), facts.task_versions.values().sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(version: u64) -> BTreeMap<u64, usize> {
        BTreeMap::from([(version, 1)])
    }

    #[test]
    fn missing_directory_is_unknown_not_error() {
        assert_eq!(version_class(&BTreeMap::new(), 4), VersionClass::Unknown);
    }

    #[test]
    fn all_current_versions_are_ok() {
        let many = BTreeMap::from([(4, 3)]);
        assert_eq!(version_class(&many, 4), VersionClass::Ok);
    }

    #[test]
    fn a_single_old_version_is_incompatible() {
        assert_eq!(version_class(&single(2), 4), VersionClass::Incompatible);
    }

    #[test]
    fn mixed_versions_are_rejected_even_when_one_is_current() {
        // 只要有任何一份不是当前格式，恢复就可能被拒：不能因为"大多没变"
        // 就放过。
        let mixed = BTreeMap::from([(4, 2), (2, 1)]);
        assert_eq!(version_class(&mixed, 4), VersionClass::Incompatible);
    }

    #[test]
    fn run_and_task_directories_are_judged_against_their_own_formats() {
        // 任务快照当前格式是 v1：一份 v1 任务快照正常，同样的 v1 若出现在
        // 运行快照目录（当前 v4）则不兼容。两份目录互不牵扯。
        let facts = SnapshotFacts {
            run_versions: single(1),
            task_versions: single(1),
            corrupt: Vec::new(),
        };
        assert_eq!(facts.run_class(4), VersionClass::Incompatible);
        assert_eq!(facts.task_class(1), VersionClass::Ok);
    }
}
