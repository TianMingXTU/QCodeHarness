//! 声明式扩展：授权状态、编译期接口与受限执行适配器。
//!
//! Skill 和 Hook 只向调度器提交数据，不获得原始执行器。编译期 Plugin 与宿主
//! 运行在同一个进程，作者的 Rust 代码仍属于受信代码；这些接口不能提供操作
//! 系统隔离。需要隔离的第三方实现应通过已有的子进程边界接入。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use crate::application::agent_roster::AgentRoster;
use crate::application::agent_run::AgentDependencies;
use crate::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Clock, EvidenceSink, ModelReply, ModelRequest,
    RunHooks, TimeLimiter, ToolInvoker,
};
use crate::application::tool_guard::{
    BoundaryEscalationGuard, GuardContext, GuardVerdict, PolicyGuard, ToolGuard, run_guard_chain,
};
use crate::application::tool_registry::{ToolEntry, ToolRegistry, ToolSource};
use crate::domain::agent::AgentRole;
use crate::domain::evidence::AuditEvent;
use crate::domain::extension::{
    ExtensionDescriptor, ExtensionState, InstalledExtension, validate_extension_id,
};
use crate::domain::ids::CallId;
use crate::domain::session::ContextSlice;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use serde_json::json;

/// 提供提示词的 Skill；返回的声明必须在安装后保持稳定。
pub trait Skill: Send + Sync {
    fn descriptor(&self) -> ExtensionDescriptor;
    fn instructions(&self) -> &str;
}

/// Hook 可见的借用数据；没有执行器、文件系统或审批句柄。
#[derive(Clone, Copy)]
pub enum HookEvent<'a> {
    BeforeToolCall {
        role: AgentRole,
        call: &'a ToolCall,
    },
    AfterToolResult {
        role: AgentRole,
        call: &'a ToolCall,
        outcome: &'a ToolOutcome,
    },
}

/// 规划工具动作的 Hook。动作逐条经过声明权限、用户授权和宿主策略。
///
/// `plan` 必须快速返回；编译期实现中的任意 Rust 代码属于宿主的信任边界。
pub trait Hook: Send + Sync {
    fn descriptor(&self) -> ExtensionDescriptor;
    fn plan(&self, event: HookEvent<'_>) -> Vec<ToolCall>;
}

/// 编译期扩展包，只组合 Skill 和 Hook，不加载动态库或运行脚本。
///
/// 包 ID 用于安装查重；各条目按自身 ID 单独批准和启用。
pub trait Plugin: Send + Sync {
    fn id(&self) -> &str;
    fn skills(&self) -> Vec<Arc<dyn Skill>>;
    fn hooks(&self) -> Vec<Arc<dyn Hook>>;
}

/// 扩展安装、选择或授权失败。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ExtensionError(pub String);

impl From<String> for ExtensionError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

#[derive(Clone)]
struct Registration {
    descriptor: ExtensionDescriptor,
    skill: Option<Arc<dyn Skill>>,
    hook: Option<Arc<dyn Hook>>,
    enabled: bool,
    approved: BTreeSet<String>,
    revision: u64,
}

#[derive(Default)]
struct RegistryState {
    entries: BTreeMap<String, Registration>,
    plugins: BTreeSet<String>,
}

/// 共享注册表。工具和 Hook 保存这份注册表的 Arc，每次动作都重读授权。
///
/// 禁用与撤权阻止后续调度，不撤销已经发起的操作。
pub struct ExtensionRegistry {
    backing_names: BTreeSet<String>,
    state: RwLock<RegistryState>,
}

/// 运行选择只缩小已安装条目的范围，不改变启用状态或批准权限。
#[derive(Clone, Default)]
pub struct ExtensionSelection {
    skills: Option<BTreeSet<String>>,
    hooks: Option<BTreeSet<String>>,
}

impl ExtensionSelection {
    /// 选择所有已显式启用的条目；未启用条目仍不可调用。
    pub fn all() -> Self {
        Self::default()
    }

    fn includes_skill(&self, id: &str) -> bool {
        self.skills.as_ref().is_none_or(|ids| ids.contains(id))
    }

    fn includes_hook(&self, id: &str) -> bool {
        self.hooks.as_ref().is_none_or(|ids| ids.contains(id))
    }
}

/// 可直接装入既有 AgentDependencies 的两个接缝。
pub struct ExtensionAdapters {
    pub source: Arc<dyn ToolSource>,
    /// 已转发原依赖中的挂点，无需装配方再包一层。
    pub hooks: Arc<dyn RunHooks>,
}

pub(crate) enum StateChange<'a> {
    Enable(&'a [String]),
    Disable,
    Revoke(&'a str),
}

impl ExtensionRegistry {
    /// 从宿主实际提供的工具建立权限字典；这些工具名同时保留给宿主。
    pub fn new(backing: &ToolRegistry) -> Self {
        Self {
            backing_names: backing.specs().into_iter().map(|spec| spec.name).collect(),
            state: RwLock::new(RegistryState::default()),
        }
    }

    pub fn install_skill(
        &self,
        skill: Arc<dyn Skill>,
    ) -> Result<ExtensionDescriptor, ExtensionError> {
        self.install_pair(Some(skill), None)
    }

    pub fn install_hook(&self, hook: Arc<dyn Hook>) -> Result<ExtensionDescriptor, ExtensionError> {
        self.install_pair(None, Some(hook))
    }

    /// 同一清单可以同时提供 Skill 与 Hook，但必须声明同一个身份和权限上限。
    pub fn install_pair(
        &self,
        skill: Option<Arc<dyn Skill>>,
        hook: Option<Arc<dyn Hook>>,
    ) -> Result<ExtensionDescriptor, ExtensionError> {
        self.install_with(skill, hook, || Ok(()))
    }

    pub(crate) fn install_with<E: From<ExtensionError>>(
        &self,
        skill: Option<Arc<dyn Skill>>,
        hook: Option<Arc<dyn Hook>>,
        persist: impl FnOnce() -> Result<(), E>,
    ) -> Result<ExtensionDescriptor, E> {
        self.install_checked(skill, hook, true, persist)
    }

    /// 加载已落盘清单时只校验声明本身，不要求 backing 工具此刻存在。
    ///
    /// 清单可能声明 MCP 工具，而列出/管理扩展时未连接 MCP。真正的可用性
    /// 由 [`Self::adapters`] 在任务装配时按本次实际工具表检查。
    pub(crate) fn install_loaded<E: From<ExtensionError>>(
        &self,
        skill: Option<Arc<dyn Skill>>,
        hook: Option<Arc<dyn Hook>>,
        persist: impl FnOnce() -> Result<(), E>,
    ) -> Result<ExtensionDescriptor, E> {
        self.install_checked(skill, hook, false, persist)
    }

    fn install_checked<E: From<ExtensionError>>(
        &self,
        skill: Option<Arc<dyn Skill>>,
        hook: Option<Arc<dyn Hook>>,
        validate_backing: bool,
        persist: impl FnOnce() -> Result<(), E>,
    ) -> Result<ExtensionDescriptor, E> {
        let entry = self
            .registration(skill, hook, validate_backing)
            .map_err(E::from)?;
        let mut state = self.state.write().map_err(|_| E::from(poisoned()))?;
        self.check_new(&state, &entry).map_err(E::from)?;
        persist()?;
        let descriptor = entry.descriptor.clone();
        state.entries.insert(descriptor.id.clone(), entry);
        Ok(descriptor)
    }

    /// 整包校验通过后才登记，重复 ID 或工具冲突不会留下半包安装。
    pub fn install_plugin(
        &self,
        plugin: &dyn Plugin,
    ) -> Result<Vec<ExtensionDescriptor>, ExtensionError> {
        validate_extension_id(plugin.id())?;
        if self.backing_names.contains(plugin.id()) {
            return Err(ExtensionError(format!(
                "Plugin `{}` 与宿主工具重名",
                plugin.id()
            )));
        }
        let mut entries = Vec::new();
        for skill in plugin.skills() {
            entries.push(self.registration(Some(skill), None, true)?);
        }
        for hook in plugin.hooks() {
            entries.push(self.registration(None, Some(hook), true)?);
        }
        if entries.is_empty() {
            return Err(ExtensionError("Plugin 没有提供 Skill 或 Hook".to_owned()));
        }
        let mut state = self.state.write().map_err(|_| poisoned())?;
        if state.plugins.contains(plugin.id()) || state.entries.contains_key(plugin.id()) {
            return Err(ExtensionError(format!(
                "Plugin ID `{}` 已安装或已被扩展占用",
                plugin.id()
            )));
        }
        let mut seen = BTreeSet::new();
        for entry in &entries {
            self.check_new(&state, entry)?;
            if entry.descriptor.id == plugin.id() {
                return Err(ExtensionError(format!(
                    "Plugin `{}` 与其扩展条目重名",
                    plugin.id()
                )));
            }
            if !seen.insert(&entry.descriptor.id) {
                return Err(ExtensionError(format!(
                    "Plugin 重复提供扩展 `{}`",
                    entry.descriptor.id
                )));
            }
        }
        let descriptors = entries
            .iter()
            .map(|entry| entry.descriptor.clone())
            .collect();
        for entry in entries {
            state.entries.insert(entry.descriptor.id.clone(), entry);
        }
        state.plugins.insert(plugin.id().to_owned());
        Ok(descriptors)
    }

    fn registration(
        &self,
        skill: Option<Arc<dyn Skill>>,
        hook: Option<Arc<dyn Hook>>,
        validate_backing: bool,
    ) -> Result<Registration, ExtensionError> {
        let descriptor = skill
            .as_ref()
            .map(|item| item.descriptor())
            .or_else(|| hook.as_ref().map(|item| item.descriptor()))
            .ok_or_else(|| ExtensionError("扩展必须提供 Skill 或 Hook".to_owned()))?;
        descriptor.validate()?;
        if hook
            .as_ref()
            .is_some_and(|item| item.descriptor() != descriptor)
        {
            return Err(ExtensionError(
                "同一扩展的 Skill 与 Hook 声明不一致".to_owned(),
            ));
        }
        if validate_backing {
            for permission in &descriptor.permissions {
                if !self.backing_names.contains(permission) {
                    return Err(ExtensionError(format!(
                        "扩展 `{}` 声明了未知工具权限 `{permission}`",
                        descriptor.id
                    )));
                }
            }
        }
        Ok(Registration {
            descriptor,
            skill,
            hook,
            enabled: false,
            approved: BTreeSet::new(),
            revision: 0,
        })
    }

    fn check_new(&self, state: &RegistryState, entry: &Registration) -> Result<(), ExtensionError> {
        let id = &entry.descriptor.id;
        if state.entries.contains_key(id) || state.plugins.contains(id) {
            return Err(ExtensionError(format!(
                "扩展 ID `{id}` 已安装或已被 Plugin 占用"
            )));
        }
        if self.backing_names.contains(id)
            || self
                .backing_names
                .contains(&entry.descriptor.skill_tool_name())
        {
            return Err(ExtensionError(format!("扩展 `{id}` 与宿主工具重名")));
        }
        Ok(())
    }

    /// 按 ID 排序返回状态快照；调用权限不以该快照为依据。
    pub fn list(&self) -> Result<Vec<InstalledExtension>, ExtensionError> {
        let state = self.state.read().map_err(|_| poisoned())?;
        Ok(state
            .entries
            .values()
            .map(|entry| InstalledExtension {
                descriptor: entry.descriptor.clone(),
                has_skill: entry.skill.is_some(),
                has_hook: entry.hook.is_some(),
                enabled: entry.enabled,
                approved_permissions: entry.approved.iter().cloned().collect(),
            })
            .collect())
    }

    /// 显式批准声明的子集。空列表表示启用提示词，但不批准任何工具动作。
    pub fn enable(&self, id: &str, permissions: &[String]) -> Result<(), ExtensionError> {
        self.change_state(id, StateChange::Enable(permissions), |_| Ok(()))
    }

    /// 禁用并清空批准项；重新启用仍需显式提交权限列表。
    pub fn disable(&self, id: &str) -> Result<(), ExtensionError> {
        self.change_state(id, StateChange::Disable, |_| Ok(()))
    }

    pub fn revoke(&self, id: &str, permission: &str) -> Result<(), ExtensionError> {
        self.change_state(id, StateChange::Revoke(permission), |_| Ok(()))
    }

    pub(crate) fn change_state<E: From<ExtensionError>>(
        &self,
        id: &str,
        change: StateChange<'_>,
        persist: impl FnOnce(&[ExtensionState]) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut state = self.state.write().map_err(|_| E::from(poisoned()))?;
        let entry = state.entries.get(id).ok_or_else(|| E::from(unknown(id)))?;
        let (enabled, approved) = match change {
            StateChange::Enable(permissions) => {
                let grants = checked_grants(entry, permissions).map_err(E::from)?;
                (true, grants)
            }
            StateChange::Disable => (false, BTreeSet::new()),
            StateChange::Revoke(permission) => {
                if !entry
                    .descriptor
                    .permissions
                    .iter()
                    .any(|declared| declared == permission)
                {
                    return Err(E::from(ExtensionError(format!(
                        "扩展 `{id}` 未声明权限 `{permission}`"
                    ))));
                }
                let mut grants = entry.approved.clone();
                grants.remove(permission);
                (entry.enabled, grants)
            }
        };
        // 只交回被改的那一条：授权文件可能被另一个会话同时改过，用整份
        // 内存快照覆盖会把对方刚做的撤销或授权写回去。持久化实现按 ID
        // 合并，因此这里给多少条就只影响多少条。
        let saved = vec![ExtensionState {
            id: id.to_owned(),
            enabled,
            permissions: approved.iter().cloned().collect(),
        }];
        // 先持久化再发布到共享状态；写盘失败时不能留下已激活的新增权限。
        persist(&saved)?;
        let entry = state.entries.get_mut(id).expect("写锁期间登记项不变");
        entry.enabled = enabled;
        entry.approved = approved;
        entry.revision += 1;
        Ok(())
    }

    /// 加载已保存的授权。先校验整份数据，未知条目或越权批准使加载整体失败。
    pub fn restore(&self, saved: &[ExtensionState]) -> Result<(), ExtensionError> {
        let mut state = self.state.write().map_err(|_| poisoned())?;
        let mut updates = BTreeMap::new();
        for item in saved {
            let entry = state
                .entries
                .get(&item.id)
                .ok_or_else(|| unknown(&item.id))?;
            let grants = checked_grants(entry, &item.permissions)?;
            if !item.enabled && !grants.is_empty() {
                return Err(ExtensionError(format!(
                    "禁用扩展 `{}` 仍保存了权限",
                    item.id
                )));
            }
            if updates
                .insert(item.id.clone(), (item.enabled, grants))
                .is_some()
            {
                return Err(ExtensionError(format!("授权状态重复包含 `{}`", item.id)));
            }
        }
        for entry in state.entries.values_mut() {
            let (enabled, approved) = updates.remove(&entry.descriptor.id).unwrap_or_default();
            entry.enabled = enabled;
            entry.approved = approved;
            entry.revision += 1;
        }
        Ok(())
    }

    /// 配置仅选择安装项，不能替用户启用或批准权限；类型不符也返回错误。
    pub fn select(
        &self,
        skills: &[String],
        hooks: &[String],
    ) -> Result<ExtensionSelection, ExtensionError> {
        self.select_optional(Some(skills), Some(hooks))
    }

    /// 与 [`Self::select`] 相同，但 `None` 表示该侧不额外限制。
    ///
    /// 配置文件里的空列表通常表示"没写"，无法区分"全选"与"清空"；
    /// 装配层用 `None` 表达不限制，启用/禁用状态仍以安装项自身为准。
    pub fn select_optional(
        &self,
        skills: Option<&[String]>,
        hooks: Option<&[String]>,
    ) -> Result<ExtensionSelection, ExtensionError> {
        let state = self.state.read().map_err(|_| poisoned())?;
        for (ids, skill) in [(skills, true), (hooks, false)] {
            let Some(ids) = ids else { continue };
            let mut seen = BTreeSet::new();
            for id in ids {
                if !seen.insert(id) {
                    return Err(ExtensionError(format!("配置重复选择扩展 `{id}`")));
                }
                let entry = state.entries.get(id).ok_or_else(|| unknown(id))?;
                if (skill && entry.skill.is_none()) || (!skill && entry.hook.is_none()) {
                    return Err(ExtensionError(format!(
                        "扩展 `{id}` 不提供所选的 {}",
                        if skill { "Skill" } else { "Hook" }
                    )));
                }
            }
        }
        Ok(ExtensionSelection {
            skills: skills.map(|ids| ids.iter().cloned().collect()),
            hooks: hooks.map(|ids| ids.iter().cloned().collect()),
        })
    }

    pub fn enabled_instructions(
        &self,
        selection: &ExtensionSelection,
    ) -> Result<String, ExtensionError> {
        let entries = self.enabled_entries(selection, true)?;
        Ok(entries
            .iter()
            .filter_map(|entry| {
                entry.skill.as_ref().map(|skill| {
                    format!(
                        "【Skill: {}】\n{}",
                        entry.descriptor.id,
                        skill.instructions()
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// 已选中 Skill 的工具名，供角色白名单放行。
    ///
    /// Skill 工具是模型调用扩展的唯一入口。装配方把它加进受限角色的
    /// 白名单，真正的 backing 工具仍由角色档案逐条把关。
    pub fn skill_tool_names(
        &self,
        selection: &ExtensionSelection,
    ) -> Result<Vec<String>, ExtensionError> {
        let entries = self.enabled_entries(selection, true)?;
        Ok(entries
            .into_iter()
            .map(|entry| entry.descriptor.skill_tool_name())
            .collect())
    }

    fn enabled_entries(
        &self,
        selection: &ExtensionSelection,
        skills: bool,
    ) -> Result<Vec<Registration>, ExtensionError> {
        let state = self.state.read().map_err(|_| poisoned())?;
        Ok(state
            .entries
            .values()
            .filter(|entry| {
                entry.enabled
                    && if skills {
                        entry.skill.is_some() && selection.includes_skill(&entry.descriptor.id)
                    } else {
                        entry.hook.is_some() && selection.includes_hook(&entry.descriptor.id)
                    }
            })
            .cloned()
            .collect())
    }

    fn authorize(
        &self,
        id: &str,
        permission: Option<&str>,
        revision: Option<u64>,
    ) -> Result<u64, ExtensionError> {
        let state = self.state.read().map_err(|_| poisoned())?;
        let entry = state.entries.get(id).ok_or_else(|| unknown(id))?;
        if !entry.enabled {
            return Err(ExtensionError(format!("扩展 `{id}` 已禁用")));
        }
        if revision.is_some_and(|expected| expected != entry.revision) {
            return Err(ExtensionError(format!(
                "扩展 `{id}` 的授权已变更，请重新发起调用"
            )));
        }
        if let Some(permission) = permission {
            if !entry
                .descriptor
                .permissions
                .iter()
                .any(|declared| declared == permission)
            {
                return Err(ExtensionError(format!(
                    "扩展 `{id}` 未声明工具权限 `{permission}`"
                )));
            }
            if !entry.approved.contains(permission) {
                return Err(ExtensionError(format!(
                    "扩展 `{id}` 的工具权限 `{permission}` 未获用户批准"
                )));
            }
        }
        Ok(entry.revision)
    }

    /// 为一次任务建立适配器，`deps.tools` 必须是尚未加入扩展的宿主工具表。
    ///
    /// 适配器绑定这次任务的证据、时钟、审批和角色花名册。编排器换角色时，
    /// `before_model_call` 只读请求更新角色；首次模型调用前使用 initial_role。
    /// 并行任务应各自构造适配器，不得共享这份运行上下文。
    pub fn adapters(
        self: &Arc<Self>,
        deps: &AgentDependencies,
        initial_role: AgentRole,
        selection: ExtensionSelection,
        limiter: Arc<dyn TimeLimiter>,
    ) -> Result<ExtensionAdapters, ExtensionError> {
        let state = self.state.read().map_err(|_| poisoned())?;
        for entry in state.entries.values() {
            // 只检查本次任务真正会暴露的条目。未启用或未被选中的清单可能
            // 引用尚未连接的 MCP 工具，让它们拖住整个任务没有意义。
            let skill_selected = entry.enabled
                && entry.skill.is_some()
                && selection.includes_skill(&entry.descriptor.id);
            let hook_selected = entry.enabled
                && entry.hook.is_some()
                && selection.includes_hook(&entry.descriptor.id);
            if !skill_selected && !hook_selected {
                continue;
            }
            if deps
                .tools
                .find(&entry.descriptor.skill_tool_name())
                .is_some()
            {
                return Err(ExtensionError(
                    "扩展适配器必须绑定未含 Skill 工具的宿主注册表".to_owned(),
                ));
            }
            for permission in &entry.descriptor.permissions {
                if deps.tools.find(permission).is_none() {
                    return Err(ExtensionError(format!(
                        "本次任务缺少 backing 工具 `{permission}`"
                    )));
                }
            }
        }
        drop(state);
        // 与 Runner 的内置链保持同一顺序；追加守卫只能收紧裁决。
        let mut guards: Vec<Arc<dyn ToolGuard>> = vec![
            Arc::new(PolicyGuard::new(deps.policy.clone())),
            Arc::new(BoundaryEscalationGuard),
        ];
        guards.extend(deps.additional_guards.iter().cloned());
        let dispatcher = Arc::new(CheckedDispatcher {
            registry: self.clone(),
            tools: deps.tools.clone(),
            approvals: deps.approvals.clone(),
            evidence: deps.evidence.clone(),
            clock: deps.clock.clone(),
            guards,
            roster: deps.roster.clone(),
            limiter,
            context: Mutex::new(RuntimeContext {
                role: initial_role,
                turn: 1,
                next_turn: 1,
            }),
        });
        Ok(ExtensionAdapters {
            source: Arc::new(SkillSource {
                registry: self.clone(),
                selection: selection.clone(),
                dispatcher: dispatcher.clone(),
            }),
            hooks: Arc::new(ExtensionHooks {
                registry: self.clone(),
                selection,
                dispatcher,
                observer: deps.hooks.clone(),
            }),
        })
    }
}

fn checked_grants(
    entry: &Registration,
    permissions: &[String],
) -> Result<BTreeSet<String>, ExtensionError> {
    let mut approved = BTreeSet::new();
    for permission in permissions {
        if !entry.descriptor.permissions.contains(permission) {
            return Err(ExtensionError(format!(
                "扩展 `{}` 未声明权限 `{permission}`",
                entry.descriptor.id
            )));
        }
        if !approved.insert(permission.clone()) {
            return Err(ExtensionError(format!("重复批准权限 `{permission}`")));
        }
    }
    Ok(approved)
}

fn poisoned() -> ExtensionError {
    ExtensionError("扩展状态锁已损坏，拒绝继续授权".to_owned())
}
fn unknown(id: &str) -> ExtensionError {
    ExtensionError(format!("未知扩展 `{id}`"))
}

#[derive(Clone, Copy)]
struct RuntimeContext {
    role: AgentRole,
    turn: u32,
    next_turn: u32,
}

struct CheckedDispatcher {
    registry: Arc<ExtensionRegistry>,
    tools: ToolRegistry,
    approvals: Arc<dyn ApprovalGate>,
    evidence: Arc<dyn EvidenceSink>,
    clock: Arc<dyn Clock>,
    guards: Vec<Arc<dyn ToolGuard>>,
    roster: AgentRoster,
    limiter: Arc<dyn TimeLimiter>,
    context: Mutex<RuntimeContext>,
}

impl CheckedDispatcher {
    fn record(&self, event: AuditEvent) {
        self.evidence.record(self.clock.now(), event);
    }

    /// 角色白名单是扩展动作的最后一道边界；审批前后都要查，审批期间换角色
    /// 时旧批准不能沿用。
    fn check_role(&self, role: AgentRole, tool: &str) -> Result<(), String> {
        let profile = self
            .roster
            .find(role)
            .ok_or_else(|| format!("角色 `{}` 没有权限档案", role.name()))?;
        if profile
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.iter().any(|allowed| allowed == tool))
        {
            return Err(format!(
                "角色 `{}` 无权调用 backing 工具 `{tool}`",
                role.name()
            ));
        }
        Ok(())
    }

    async fn dispatch(&self, id: &str, mut call: ToolCall) -> ToolOutcome {
        let context = match self.context.lock() {
            Ok(context) => *context,
            Err(_) => return ToolOutcome::refused("扩展运行上下文已损坏"),
        };
        // Hook 作者给出的 ID 不能覆盖外层调用 ID；证据明确记录动作的扩展来源。
        call.id = CallId::new(format!("extension__{id}__{}", call.id.as_str()));
        self.record(AuditEvent::ToolRequested {
            turn: context.turn,
            call: call.clone(),
        });
        let outcome = self.execute(id, &call, context).await;
        self.observe(context.turn, &call, outcome)
    }

    async fn execute(&self, id: &str, call: &ToolCall, context: RuntimeContext) -> ToolOutcome {
        let revision = match self.registry.authorize(id, Some(&call.name), None) {
            Ok(revision) => revision,
            Err(error) => return ToolOutcome::refused(error.to_string()),
        };
        if let Err(reason) = self.check_role(context.role, &call.name) {
            return ToolOutcome::refused(reason);
        }
        let Some(entry) = self.tools.find(&call.name) else {
            return ToolOutcome::refused(format!("backing 工具 `{}` 未注册", call.name));
        };
        let decision = self
            .guards
            .iter()
            .find_map(|guard| guard.as_policy_guard())
            .expect("调度器始终装配策略守卫")
            .evaluate(call, &entry.spec);
        self.record(AuditEvent::PolicyEvaluated {
            turn: context.turn,
            tool: call.name.clone(),
            verdict: decision.verdict,
            rule_index: decision.rule_index,
            reason: decision.reason,
        });
        let (verdict, index) = run_guard_chain(&self.guards, call, &GuardContext::new(&entry.spec));
        match verdict {
            GuardVerdict::Deny(reason) => return ToolOutcome::refused(reason),
            GuardVerdict::RequireApproval(reason) => {
                let from_policy =
                    index.is_some_and(|index| self.guards[index].as_policy_guard().is_some());
                let request = ApprovalRequest {
                    turn: context.turn,
                    call: call.clone(),
                    annotations: entry.spec.annotations,
                    reason: (!from_policy).then_some(reason),
                    resumed: false,
                };
                // 审批不套时限：人思考多久由人决定。交互闸门在 future 的
                // poll 内同步读 stdin，包上计时器也拿不到轮询机会，只会
                // 给出"有时限"的假象。
                let decision = self.approvals.request(&request).await;
                self.record(AuditEvent::ApprovalDecided {
                    turn: context.turn,
                    tool: call.name.clone(),
                    allowed: decision.is_ok(),
                    reason: decision.as_ref().err().cloned(),
                });
                if let Err(reason) = decision {
                    return ToolOutcome::refused(reason);
                }
            }
            GuardVerdict::Pass => {}
        }
        // 审批可能等待很久。禁用后再启用同样改变版本，旧批准不能复用。
        if let Err(error) = self
            .registry
            .authorize(id, Some(&call.name), Some(revision))
        {
            return ToolOutcome::refused(error.to_string());
        }
        // 等待审批期间编排器可能已换角色，批准是按旧角色的白名单拿到的。
        // 角色只增不减地变化时重放旧决定等于越权，必须让调用方重新发起。
        let current = match self.context.lock() {
            Ok(current) => *current,
            Err(_) => return ToolOutcome::refused("扩展运行上下文已损坏"),
        };
        if current.role != context.role {
            return ToolOutcome::refused(format!(
                "审批期间角色从 `{}` 变为 `{}`，本次批准不再适用",
                context.role.name(),
                current.role.name()
            ));
        }
        if let Err(reason) = self.check_role(context.role, &call.name) {
            return ToolOutcome::refused(reason);
        }
        match self.limiter.limit(entry.invoker.invoke(call)).await {
            Ok(outcome) => outcome,
            Err(_) => ToolOutcome::failed(format!(
                "扩展 `{id}` 调用 `{}` 超时；已发起动作的副作用与结果无法确认",
                call.name
            )),
        }
    }

    fn observe(&self, turn: u32, call: &ToolCall, mut outcome: ToolOutcome) -> ToolOutcome {
        let boundary = outcome.execution.take();
        let failure = outcome.mcp_failure.take();
        self.record(AuditEvent::ToolObserved {
            turn,
            call: call.clone(),
            outcome: outcome.clone(),
        });
        if let Some(failure) = failure {
            self.record(AuditEvent::McpCallFailed {
                turn,
                call_id: call.id.clone(),
                tool: call.name.clone(),
                failure,
            });
        }
        if let Some(boundary) = boundary {
            self.record(AuditEvent::SandboxApplied {
                turn,
                call: call.clone(),
                platform: boundary.platform.to_owned(),
                active: boundary.active,
                boundary_checked: boundary.boundary_checked,
            });
        }
        outcome
    }
}

struct SkillSource {
    registry: Arc<ExtensionRegistry>,
    selection: ExtensionSelection,
    dispatcher: Arc<CheckedDispatcher>,
}

impl ToolSource for SkillSource {
    fn name(&self) -> &str {
        "extensions"
    }
    fn tools(&self) -> Vec<ToolEntry> {
        let entries = match self.registry.enabled_entries(&self.selection, true) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        entries
            .into_iter()
            .map(|entry| ToolEntry {
                spec: ToolSpec {
                    name: entry.descriptor.skill_tool_name(),
                    description: entry.descriptor.description,
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "tool": {
                                "type": "string",
                                "description": "可选的已声明 backing 工具名；省略时返回 Skill 指令"
                            },
                            "arguments": {"type": "object"}
                        },
                        "additionalProperties": false
                    }),
                    annotations: ToolAnnotations::new(false, true, false, true),
                },
                invoker: Arc::new(SkillInvoker {
                    id: entry.descriptor.id,
                    registry: self.registry.clone(),
                    dispatcher: self.dispatcher.clone(),
                }),
            })
            .collect()
    }
}

struct SkillInvoker {
    id: String,
    registry: Arc<ExtensionRegistry>,
    dispatcher: Arc<CheckedDispatcher>,
}

impl ToolInvoker for SkillInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            if let Err(error) = self.registry.authorize(&self.id, None, None) {
                return ToolOutcome::refused(error.to_string());
            }
            let Some(args) = call.arguments.as_object() else {
                return ToolOutcome::failed("Skill 参数必须是对象");
            };
            if args.keys().any(|key| key != "tool" && key != "arguments") {
                return ToolOutcome::failed("Skill 参数仅支持 tool 和 arguments");
            }
            match args.get("tool") {
                None if args.is_empty() => {
                    let state = match self.registry.state.read() {
                        Ok(state) => state,
                        Err(_) => return ToolOutcome::refused(poisoned().to_string()),
                    };
                    match state
                        .entries
                        .get(&self.id)
                        .filter(|entry| entry.enabled)
                        .and_then(|entry| entry.skill.as_ref())
                    {
                        Some(skill) => ToolOutcome::success(skill.instructions()),
                        None => ToolOutcome::refused("Skill 已禁用或不存在"),
                    }
                }
                Some(serde_json::Value::String(tool)) => {
                    let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    if !arguments.is_object() {
                        return ToolOutcome::failed("backing 工具 arguments 必须是对象");
                    }
                    self.dispatcher
                        .dispatch(
                            &self.id,
                            ToolCall {
                                id: call.id.clone(),
                                name: tool.clone(),
                                arguments,
                            },
                        )
                        .await
                }
                _ => ToolOutcome::failed("tool 必须是字符串；读取指令请传空对象"),
            }
        })
    }
}

struct ExtensionHooks {
    registry: Arc<ExtensionRegistry>,
    selection: ExtensionSelection,
    dispatcher: Arc<CheckedDispatcher>,
    observer: Arc<dyn RunHooks>,
}

impl ExtensionHooks {
    async fn run(&self, event: HookEvent<'_>) {
        let entries = match self.registry.enabled_entries(&self.selection, false) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries {
            let id = &entry.descriptor.id;
            if self.registry.authorize(id, None, None).is_err() {
                continue;
            }
            if let Some(hook) = entry.hook {
                for action in hook.plan(event) {
                    // 直接调用宿主注册表，不再分发 RunHooks，避免 Hook 自己触发自己。
                    self.dispatcher.dispatch(id, action).await;
                }
            }
        }
    }
}

impl RunHooks for ExtensionHooks {
    fn before_model_call<'a>(&'a self, request: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(mut context) = self.dispatcher.context.lock() {
                let latest = request
                    .history
                    .iter()
                    .filter_map(|slice| match slice {
                        ContextSlice::Round { number, .. } => Some(*number),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                context.role = request.spec.role;
                context.turn = context.next_turn.max(latest.saturating_add(1));
                context.next_turn = context.turn.saturating_add(1);
            }
            self.observer.before_model_call(request).await;
        })
    }

    fn after_model_reply<'a>(&'a self, reply: &'a ModelReply, retried: bool) -> BoxFuture<'a, ()> {
        self.observer.after_model_reply(reply, retried)
    }

    fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.observer.before_tool_call(call).await;
            let role = self
                .dispatcher
                .context
                .lock()
                .ok()
                .map(|context| context.role);
            if let Some(role) = role {
                self.run(HookEvent::BeforeToolCall { role, call }).await;
            }
        })
    }

    fn after_tool_result<'a>(
        &'a self,
        call: &'a ToolCall,
        outcome: &'a ToolOutcome,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let role = self
                .dispatcher
                .context
                .lock()
                .ok()
                .map(|context| context.role);
            if let Some(role) = role {
                self.run(HookEvent::AfterToolResult {
                    role,
                    call,
                    outcome,
                })
                .await;
            }
            self.observer.after_tool_result(call, outcome).await;
        })
    }
}
