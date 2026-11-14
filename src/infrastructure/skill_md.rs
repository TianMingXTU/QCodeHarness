//! SKILL.md 安装链路与本地技能市场。
//!
//! 安装走既有 `LocalExtensionStore::install`：SKILL.md 先转成清单 TOML
//! 写入调用方提供的 scratch 目录，再复用标准安装（默认禁用、create_new、
//! 链接检查）。市场支持目录索引与自定义 TOML 索引两种形态。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::application::extension::ExtensionRegistry;
use crate::application::skill_md::{parse_skill_markdown, SkillManifest};
use crate::domain::extension::ExtensionDescriptor;
use crate::infrastructure::extensions::{ExtensionStoreError, LocalExtensionStore};

/// SKILL.md 与市场索引的大小上限，与扩展清单的 64 KiB 同限。
const MAX_SOURCE_BYTES: u64 = 64 * 1024;

/// 安装结果：清单描述与需要向用户显式报告的未识别 frontmatter 键。
#[derive(Debug, Clone)]
pub struct SkillInstallReport {
    pub descriptor: ExtensionDescriptor,
    pub unrecognized_keys: Vec<String>,
}

/// 市场索引里的一条技能：检索 id 与指向 SKILL.md 的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceEntry {
    pub id: String,
    pub source: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceIndexFile {
    skills: Vec<MarketplaceIndexEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketplaceIndexEntry {
    id: String,
    path: PathBuf,
}

/// 把一份 SKILL.md 转成清单后经 `store.install` 安装。
///
/// `scratch` 是转换产物的落盘目录，由调用方提供并保证存在；产物文件名
/// 为 `<id>.manifest.toml`，同名旧文件会被覆盖——它只是中间产物，真正的
/// 安装目标由 store 在工作区清单目录内以 create_new 创建。
///
/// 未识别的 frontmatter 键不阻断安装，但通过返回值显式报告；
/// 无法映射的 allowed-tools 条目则直接拒绝：装出来的清单权限会少于
/// 作者声明，静默降级等于替作者放宽权限边界。
pub fn install_skill_md(
    store: &LocalExtensionStore,
    registry: &ExtensionRegistry,
    skill_md: &Path,
    scratch: &Path,
) -> Result<SkillInstallReport, ExtensionStoreError> {
    let manifest = convert_skill_md(skill_md)?;
    let toml_path = write_scratch_manifest(scratch, &manifest)?;
    let descriptor = store.install(registry, &toml_path)?;
    Ok(SkillInstallReport {
        descriptor,
        unrecognized_keys: manifest.unrecognized_keys,
    })
}

/// 从市场安装指定 id 的技能。
///
/// 条目指向 `.toml` 清单时直接走 `store.install`；指向 SKILL.md 时先转换
/// 再安装，并要求索引 id 与 frontmatter `name` 一致——按 id 检索、按 name
/// 安装的错位会让"装了什么"取决于文件内容而非用户选择。一致性检查发生在
/// 写入之前，错配的条目不会产生任何安装痕迹。
pub fn install_from_marketplace(
    store: &LocalExtensionStore,
    registry: &ExtensionRegistry,
    index: &Path,
    skill_id: &str,
    scratch: &Path,
) -> Result<SkillInstallReport, ExtensionStoreError> {
    let entries = load_marketplace_index(index)?;
    let Some(found) = entries.iter().find(|entry| entry.id == skill_id) else {
        let available = if entries.is_empty() {
            "（无）".to_owned()
        } else {
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(invalid(
            index,
            format!(
                "市场索引 {} 中没有技能 `{skill_id}`；可用：{available}",
                index.display()
            ),
        ));
    };
    if found.source.extension().and_then(|value| value.to_str()) == Some("toml") {
        // TOML 清单自带权威 id，索引 id 只用于检索，不做一致性检查。
        let descriptor = store.install(registry, &found.source)?;
        return Ok(SkillInstallReport {
            descriptor,
            unrecognized_keys: Vec::new(),
        });
    }
    let manifest = convert_skill_md(&found.source)?;
    if manifest.id != skill_id {
        return Err(invalid(
            &found.source,
            format!(
                "索引 id `{skill_id}` 与 SKILL.md name `{}` 不一致",
                manifest.id
            ),
        ));
    }
    let toml_path = write_scratch_manifest(scratch, &manifest)?;
    let descriptor = store.install(registry, &toml_path)?;
    Ok(SkillInstallReport {
        descriptor,
        unrecognized_keys: manifest.unrecognized_keys,
    })
}

/// 扫描目录式市场：根 SKILL.md 与各子目录里的 SKILL.md 各成一条。
///
/// 没有 SKILL.md 的子目录视为非技能目录，直接跳过；任何一条解析失败都让
/// 整体失败——市场列表宁可报错，不能悄悄少列。
pub fn scan_marketplace_directory(
    dir: &Path,
) -> Result<Vec<MarketplaceEntry>, ExtensionStoreError> {
    let mut entries = Vec::new();
    let root_skill = dir.join("SKILL.md");
    if root_skill.is_file() {
        let manifest = convert_skill_md(&root_skill)?;
        entries.push(MarketplaceEntry {
            id: manifest.id,
            source: root_skill,
        });
    }
    let mut children: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|source| ExtensionStoreError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .map(|entry| {
            entry
                .map(|item| item.path())
                .map_err(|source| ExtensionStoreError::Io {
                    path: dir.to_path_buf(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    children.sort();
    for child in children {
        if !child.is_dir() {
            continue;
        }
        let skill_path = child.join("SKILL.md");
        if skill_path.is_file() {
            let manifest = convert_skill_md(&skill_path)?;
            entries.push(MarketplaceEntry {
                id: manifest.id,
                source: skill_path,
            });
        }
    }
    check_duplicates(&entries, dir)?;
    Ok(entries)
}

/// 加载市场索引：目录按扫描处理，TOML 文件按 `[[skills]]` 条目解析，
/// 相对路径按索引文件所在目录解析。
pub fn load_marketplace_index(path: &Path) -> Result<Vec<MarketplaceEntry>, ExtensionStoreError> {
    if path.is_dir() {
        return scan_marketplace_directory(path);
    }
    let text = read_bounded_text(path, "市场索引")?;
    let file: MarketplaceIndexFile =
        toml::from_str(&text).map_err(|error| invalid(path, error.to_string()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let entries = file
        .skills
        .into_iter()
        .map(|item| {
            let source = if item.path.is_absolute() {
                item.path
            } else {
                base.join(&item.path)
            };
            MarketplaceEntry {
                id: item.id,
                source,
            }
        })
        .collect::<Vec<_>>();
    check_duplicates(&entries, path)?;
    Ok(entries)
}

/// 解析并校验一份 SKILL.md：普通文件、不超过 64 KiB、frontmatter 合法、
/// allowed-tools 全部可映射。
fn convert_skill_md(skill_md: &Path) -> Result<SkillManifest, ExtensionStoreError> {
    let text = read_bounded_text(skill_md, "SKILL.md")?;
    let manifest = parse_skill_markdown(&text, &skill_md.display().to_string())
        .map_err(|message| invalid(skill_md, message))?;
    if !manifest.unmapped_permissions.is_empty() {
        return Err(invalid(
            skill_md,
            format!(
                "allowed-tools 中有无法映射为本仓权限的条目，拒绝静默降级：{}",
                manifest.unmapped_permissions.join(", ")
            ),
        ));
    }
    Ok(manifest)
}

/// 把转换后的清单写入 scratch；文件名带 `<id>`，只作中间产物。
fn write_scratch_manifest(
    scratch: &Path,
    manifest: &SkillManifest,
) -> Result<PathBuf, ExtensionStoreError> {
    let toml_path = scratch.join(format!("{}.manifest.toml", manifest.id));
    fs::write(&toml_path, manifest.to_manifest_toml()).map_err(|source| {
        ExtensionStoreError::Io {
            path: toml_path.clone(),
            source,
        }
    })?;
    Ok(toml_path)
}

/// 读取 SKILL.md 或市场索引：本身不是链接、是普通文件、不超过 64 KiB。
///
/// 读侧链接检查与清单同规：链接目标可能在仓库之外。转换产物最终会进入
/// 工作区清单目录，源头文件不落盘，这里的边界就是内容来源边界。
fn read_bounded_text(path: &Path, label: &str) -> Result<String, ExtensionStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| ExtensionStoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || is_link(&metadata) {
        return Err(invalid(path, format!("{label} 必须是普通文件")));
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(invalid(path, format!("只允许不超过 64 KiB 的{label}")));
    }
    fs::read_to_string(path).map_err(|source| ExtensionStoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// 与 `extensions::is_link` 同一规则；该函数私有，跨文件复用需要提升
/// 可见性，超出本任务边界，两处实现需同步维护。
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

/// 索引内 ID 重复与 `load_into` 的清单重复同规：宁可拒绝也不给出歧义列表。
fn check_duplicates(
    entries: &[MarketplaceEntry],
    source: &Path,
) -> Result<(), ExtensionStoreError> {
    let mut seen = BTreeSet::new();
    for entry in entries {
        if !seen.insert(entry.id.as_str()) {
            return Err(invalid(
                source,
                format!("市场索引中扩展 ID `{}` 重复", entry.id),
            ));
        }
    }
    Ok(())
}

fn invalid(path: &Path, message: String) -> ExtensionStoreError {
    ExtensionStoreError::Invalid {
        path: path.to_path_buf(),
        message,
    }
}
