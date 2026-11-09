//! 本地扩展清单与授权状态存储。
//!
//! manifest 只包含提示词和已声明的工具动作，不解释脚本、不加载动态库。
//! 作者清单与用户授权分别保存，新增清单默认禁用。

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::application::extension::{
    ExtensionError, ExtensionRegistry, Hook, HookEvent, Skill, StateChange,
};
use crate::application::tool_registry::ToolRegistry;
use crate::domain::extension::{ExtensionDescriptor, ExtensionState};
use crate::domain::ids::CallId;
use crate::domain::tool::ToolCall;

const MAX_FILE_BYTES: u64 = 64 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// 一份清单可能同时提供 Skill 与 Hook；缺哪项就为 `None`。
type ManifestEntries = (Option<Arc<dyn Skill>>, Option<Arc<dyn Hook>>);

/// 本地存储失败。保留路径和底层原因，不能以空扩展列表掩盖损坏文件。
#[derive(Debug, thiserror::Error)]
pub enum ExtensionStoreError {
    #[error("扩展文件 {path} 的 IO 操作失败: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("扩展文件 {path} 无效: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(transparent)]
    Extension(#[from] ExtensionError),
}

/// 项目内的扩展存储。
///
/// 作者清单放在 `<workspace>/.qcodeharness/extensions/manifests/`，随仓库
/// 一起分发；用户授权单独放在用户配置目录，克隆来的仓库不能自带"已批准"。
/// 清单目录内的路径检查拒绝符号链接与 Windows reparse point，但不检查
/// 工作区自身及其祖先：工作区可能本来就在链接路径下（macOS 的 `/var`）。
#[derive(Clone)]
pub struct LocalExtensionStore {
    /// 工作区根；链接检查只覆盖它之下的部分。
    workspace: PathBuf,
    /// 清单目录，`<workspace>/.qcodeharness/extensions`。
    root: PathBuf,
    /// 用户级授权文件；系统给不出配置目录时为 `None`。
    state: Option<PathBuf>,
}

impl LocalExtensionStore {
    /// 授权状态写入平台用户配置目录；清单仍从工作区读取。
    pub fn new(workspace: &Path) -> Self {
        let directory = crate::infrastructure::config::user_directory()
            .map(|dir| dir.join("extension-state"));
        Self::with_state_directory(workspace, directory)
    }

    /// 指定授权状态目录，供测试与嵌入方隔离状态。
    ///
    /// `None` 表示没有可用的用户配置目录：此时清单照常加载，但启用与撤权
    /// 无法保存，相关操作会显式报错，而不是假装成功。
    pub fn with_state_directory(workspace: &Path, directory: Option<PathBuf>) -> Self {
        let root = workspace.join(".qcodeharness/extensions");
        let state = directory.map(|dir| dir.join(format!("{}.toml", workspace_key(workspace))));
        Self {
            workspace: workspace.to_path_buf(),
            root,
            state,
        }
    }

    /// 授权状态文件位置；没有可用用户目录时为 `None`。
    pub fn state_path(&self) -> Option<&Path> {
        self.state.as_deref()
    }

    /// 加载所有安装项与保存的授权，任一清单或授权无效时整体返回错误。
    ///
    /// 编译期 Plugin 若需要恢复授权，应先安装到注册表，再调用 load_into。
    pub fn load(
        &self,
        backing: &ToolRegistry,
    ) -> Result<Arc<ExtensionRegistry>, ExtensionStoreError> {
        let registry = Arc::new(ExtensionRegistry::new(backing));
        self.load_into(&registry)?;
        Ok(registry)
    }

    pub fn load_into(&self, registry: &ExtensionRegistry) -> Result<(), ExtensionStoreError> {
        self.reject_links(&self.root)?;
        let manifests = self.root.join("manifests");
        self.reject_links(&manifests)?;
        // 授权文件先解析：格式错误时不能先装好一批清单再报错，否则调用方
        // 修正后重试会撞上"扩展已安装"。
        let saved = match &self.state {
            Some(path) if path.try_exists().map_err(|source| io_error(path, source))? => {
                // 授权文件由使用者自己维护，但链接仍要拒绝：写到别处会让
                // "已批准"悄悄落到另一个位置。
                reject_link_itself(path)?;
                let text = read_bounded(path)?;
                Some(
                    toml::from_str::<SavedState>(&text)
                        .map_err(|error| invalid(path, error.to_string()))?,
                )
            }
            _ => None,
        };
        if manifests
            .try_exists()
            .map_err(|source| io_error(&manifests, source))?
        {
            let entries =
                fs::read_dir(&manifests).map_err(|source| io_error(&manifests, source))?;
            let mut paths = Vec::new();
            for entry in entries {
                paths.push(entry.map_err(|source| io_error(&manifests, source))?.path());
            }
            paths.sort();
            // 清单全部解析后才开始登记。单个坏文件不能使前面的授权先行生效。
            let mut parsed = Vec::new();
            for path in paths {
                if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                    return Err(invalid(&path, "清单目录只允许普通 TOML 文件"));
                }
                let manifest = read_manifest(&path)?;
                if path.file_name().and_then(|value| value.to_str())
                    != Some(&format!("{}.toml", manifest.id))
                {
                    return Err(invalid(&path, "文件名与清单 ID 不一致"));
                }
                parsed.push(manifest);
            }
            // 先把能预见的冲突查完再登记：装到一半失败会让调用方修正文件后
            // 重试时撞上"已安装"，只能换新注册表。
            let mut ids = BTreeSet::new();
            let existing: BTreeSet<String> = registry
                .list()
                .map_err(ExtensionStoreError::from)?
                .into_iter()
                .map(|item| item.descriptor.id)
                .collect();
            for manifest in &parsed {
                if !ids.insert(manifest.id.clone()) || existing.contains(&manifest.id) {
                    return Err(invalid(
                        &manifests.join(format!("{}.toml", manifest.id)),
                        format!("扩展 ID `{}` 重复", manifest.id),
                    ));
                }
            }
            for manifest in parsed {
                let (skill, hook) = manifest.entries();
                // 列清单时不要求所有 backing 工具都在场（例如未连接的 MCP
                // 工具）；任务装配阶段的 adapters() 会按实际工具表复核。
                registry.install_loaded::<ExtensionStoreError>(skill, hook, || Ok(()))?;
            }
        }
        if let Some(saved) = saved {
            registry.restore(&saved.extensions)?;
        }
        Ok(())
    }

    /// 先读取并验证源文件，再以 create_new 安装；不覆盖已有安装项。
    ///
    /// 只校验清单自身的声明，不要求 backing 工具此刻在场。清单可能引用
    /// 未连接的 MCP 工具，而安装扩展不该依赖某个 MCP Server 是否已配置；
    /// 真正的可用性由任务装配阶段按本次实际工具表复核。
    pub fn install(
        &self,
        registry: &ExtensionRegistry,
        source: &Path,
    ) -> Result<ExtensionDescriptor, ExtensionStoreError> {
        let manifest = read_manifest(source)?;
        let text = toml::to_string_pretty(&manifest)
            .map_err(|error| invalid(source, error.to_string()))?;
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid(source, "规范化清单超过 64 KiB"));
        }
        let destination = self
            .root
            .join("manifests")
            .join(format!("{}.toml", manifest.id));
        let (skill, hook) = manifest.entries();
        registry.install_loaded(skill, hook, || {
            create_checked_dir(&self.root, &self.workspace)?;
            create_checked_dir(
                destination.parent().expect("清单文件有父目录"),
                &self.workspace,
            )?;
            self.reject_links(&destination)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|source| io_error(&destination, source))?;
            if let Err(source) = file
                .write_all(text.as_bytes())
                .and_then(|_| file.sync_all())
            {
                drop(file);
                // 安装尚未发布到注册表。清理失败也带出错误，不能伪称成功。
                if let Err(cleanup) = fs::remove_file(&destination) {
                    return Err(invalid(
                        &destination,
                        format!("写入失败: {source}；清理未完成: {cleanup}"),
                    ));
                }
                return Err(io_error(&destination, source));
            }
            Ok(())
        })
    }

    /// 批准声明子集并保存；持久化失败时不会发布新增授权。
    pub fn enable(
        &self,
        registry: &ExtensionRegistry,
        id: &str,
        permissions: &[String],
    ) -> Result<(), ExtensionStoreError> {
        registry.change_state(id, StateChange::Enable(permissions), |saved| {
            self.save_state(saved)
        })
    }

    pub fn disable(
        &self,
        registry: &ExtensionRegistry,
        id: &str,
    ) -> Result<(), ExtensionStoreError> {
        registry.change_state(id, StateChange::Disable, |saved| self.save_state(saved))
    }

    pub fn revoke(
        &self,
        registry: &ExtensionRegistry,
        id: &str,
        permission: &str,
    ) -> Result<(), ExtensionStoreError> {
        registry.change_state(id, StateChange::Revoke(permission), |saved| {
            self.save_state(saved)
        })
    }

    /// 保存授权状态。
    ///
    /// 授权文件在用户配置目录，不在工作区；同一工作区的两个会话可能同时
    /// 写它，因此按"读回最新、只改本条目"合并，避免用旧快照覆盖对方的
    /// 撤销或新授权。
    fn save_state(&self, saved: &[ExtensionState]) -> Result<(), ExtensionStoreError> {
        let Some(destination) = &self.state else {
            return Err(invalid(
                Path::new("<用户配置目录>"),
                "系统没有提供用户配置目录，无法保存扩展授权",
            ));
        };
        let directory = destination.parent().expect("授权文件有父目录");
        fs::create_dir_all(directory).map_err(|source| io_error(directory, source))?;
        let mut merged = match destination.try_exists() {
            Ok(true) => {
                let text = read_bounded(destination)?;
                toml::from_str::<SavedState>(&text)
                    .map_err(|error| invalid(destination, error.to_string()))?
                    .extensions
            }
            Ok(false) => Vec::new(),
            Err(source) => return Err(io_error(destination, source)),
        };
        for item in saved {
            match merged.iter_mut().find(|existing| existing.id == item.id) {
                Some(existing) => *existing = item.clone(),
                None => merged.push(item.clone()),
            }
        }
        let text = toml::to_string_pretty(&SavedState { extensions: merged })
            .map_err(|error| invalid(destination, error.to_string()))?;
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid(destination, "授权状态超过 64 KiB"));
        }
        let temp = directory.join(format!(
            ".state-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|source| io_error(&temp, source))?;
        let result = file
            .write_all(text.as_bytes())
            .and_then(|_| file.sync_all());
        drop(file);
        let result = result
            .map_err(|source| io_error(&temp, source))
            .and_then(|_| {
                fs::rename(&temp, destination)
                    .map_err(|source| io_error(destination, source))
            });
        if let Err(error) = result {
            if let Err(cleanup) = fs::remove_file(&temp) {
                return Err(invalid(
                    &temp,
                    format!("保存授权失败: {error}；清理未完成: {cleanup}"),
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    /// 只检查工作区之下的路径分量。
    ///
    /// 工作区自身可能位于符号链接路径下（macOS 的 `/var` 就是链接），把
    /// 祖先链一并纳入会把所有正常项目判成不可用。这里只拦工作区内部由
    /// 仓库引入的链接。
    fn reject_links(&self, path: &Path) -> Result<(), ExtensionStoreError> {
        reject_links_from(path, &self.workspace)
    }
}

/// 每个工作区一份授权文件的稳定键。
///
/// 用固定算法的 FNV-1a 而不是 `DefaultHasher`：后者不保证跨 Rust 版本稳定，
/// 升级工具链后文件名一变，用户已批准的权限就会被静默丢弃。键取绝对路径，
/// 不同工作区互不覆盖。
fn workspace_key(workspace: &Path) -> String {
    let path = std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    id: String,
    description: String,
    instructions: String,
    permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hook: Option<ManifestHook>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestHook {
    event: ManifestEvent,
    tool: String,
    args: Value,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManifestEvent {
    BeforeTool,
    AfterTool,
}

impl Manifest {
    fn descriptor(&self) -> ExtensionDescriptor {
        ExtensionDescriptor {
            id: self.id.clone(),
            description: self.description.clone(),
            permissions: self.permissions.clone(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.descriptor().validate()?;
        if self.instructions.trim().is_empty() && self.hook.is_none() {
            return Err("清单必须包含 Skill 指令或 Hook".to_owned());
        }
        if let Some(hook) = &self.hook {
            if !self.permissions.contains(&hook.tool) {
                return Err(format!("Hook 工具 `{}` 未在 permissions 中声明", hook.tool));
            }
            if !hook.args.is_object() {
                return Err("Hook args 必须是参数对象".to_owned());
            }
        }
        Ok(())
    }

    fn entries(self) -> ManifestEntries {
        let has_skill = !self.instructions.trim().is_empty();
        let has_hook = self.hook.is_some();
        let manifest = Arc::new(self);
        (
            has_skill.then_some(manifest.clone() as Arc<dyn Skill>),
            has_hook.then_some(manifest as Arc<dyn Hook>),
        )
    }
}

impl Skill for Manifest {
    fn descriptor(&self) -> ExtensionDescriptor {
        self.descriptor()
    }
    fn instructions(&self) -> &str {
        &self.instructions
    }
}

impl Hook for Manifest {
    fn descriptor(&self) -> ExtensionDescriptor {
        self.descriptor()
    }
    fn plan(&self, event: HookEvent<'_>) -> Vec<ToolCall> {
        let Some(hook) = &self.hook else {
            return Vec::new();
        };
        let matches = matches!(
            (hook.event, event),
            (ManifestEvent::BeforeTool, HookEvent::BeforeToolCall { .. })
                | (ManifestEvent::AfterTool, HookEvent::AfterToolResult { .. })
        );
        if matches {
            vec![ToolCall {
                id: CallId::new(format!("manifest-{}", self.id)),
                name: hook.tool.clone(),
                arguments: hook.args.clone(),
            }]
        } else {
            Vec::new()
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedState {
    extensions: Vec<ExtensionState>,
}

fn read_manifest(path: &Path) -> Result<Manifest, ExtensionStoreError> {
    // 清单自身不得是链接：链接目标可能在仓库之外，扫描仓库看不出这条依赖。
    // 祖先链不查，调用方的临时目录本身可能就是链接（macOS 的 /var）。
    reject_link_itself(path)?;
    let text = read_bounded(path)?;
    let manifest: Manifest =
        toml::from_str(&text).map_err(|error| invalid(path, error.to_string()))?;
    manifest.validate().map_err(|error| invalid(path, error))?;
    Ok(manifest)
}

fn reject_link_itself(path: &Path) -> Result<(), ExtensionStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) => {
            Err(invalid(path, "拒绝符号链接或 reparse point"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn read_bounded(path: &Path) -> Result<String, ExtensionStoreError> {
    let file = File::open(path).map_err(|source| io_error(path, source))?;
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(invalid(path, "只允许不超过 64 KiB 的普通文件"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid(path, "文件读取期间增长到 64 KiB 以上"));
    }
    String::from_utf8(bytes).map_err(|error| invalid(path, error.to_string()))
}

fn create_checked_dir(path: &Path, boundary: &Path) -> Result<(), ExtensionStoreError> {
    fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    // 建完再查：中间目录可能早已存在且是指向别处的链接。
    reject_links_from(path, boundary)
}

/// 从 `boundary` 之下的第一个分量开始查链接。
///
/// `boundary` 自身及其祖先不查：工作区可能本来就在链接路径下，把祖先链
/// 纳入会把正常项目判成不可用。
fn reject_links_from(path: &Path, boundary: &Path) -> Result<(), ExtensionStoreError> {
    let relative = path.strip_prefix(boundary).unwrap_or(path);
    let mut current = boundary.to_path_buf();
    for component in relative.components() {
        if matches!(component, Component::ParentDir) {
            return Err(invalid(path, "路径不允许包含 .."));
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(invalid(&current, "拒绝符号链接或 reparse point"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&current, source)),
        }
    }
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn io_error(path: &Path, source: std::io::Error) -> ExtensionStoreError {
    ExtensionStoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn invalid(path: &Path, message: impl Into<String>) -> ExtensionStoreError {
    ExtensionStoreError::Invalid {
        path: path.to_path_buf(),
        message: message.into(),
    }
}
