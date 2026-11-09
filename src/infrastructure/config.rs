//! 按内置默认、用户级、项目级、显式文件的顺序合并应用配置。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::config::AppConfig;
use crate::domain::ids::validate_safe_identifier;

/// 单个配置文件的字节上限。
///
/// 配置是人写的短文件；设上限是为了不让一个指向 `/dev/zero` 或巨型日志的
/// 项目文件把启动拖进无限读取。
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// 按优先级排列的配置来源；显式文件参与最后一层合并。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    /// 系统无法提供用户配置目录时为 `None`。
    pub user: Option<PathBuf>,
    pub project: PathBuf,
    /// 由调用者明确指定，缺失时必须报错。
    pub explicit: Option<PathBuf>,
}

/// 用户级配置目录；分层指令也使用它。
///
/// 系统无法提供配置目录时返回 `None`，调用方不得自行猜测 `~` 路径。
pub fn user_directory() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("qcodeharness"))
}

/// 按平台配置目录、工作区与 `QCH_CONFIG` 发现配置路径，不读取文件。
pub fn discover(workspace: &Path) -> ConfigPaths {
    ConfigPaths {
        user: user_directory().map(|dir| dir.join("config.toml")),
        project: workspace.join(".qcodeharness/config.toml"),
        explicit: std::env::var_os("QCH_CONFIG").map(PathBuf::from),
    }
}

/// 从工作区发现配置路径并加载。
///
/// # Errors
///
/// 读取、TOML 解析或最终值校验失败时返回带来源路径的错误。
pub fn load(workspace: &Path) -> Result<AppConfig, ConfigError> {
    load_from_paths(&discover(workspace))
}

/// 合并给定文件并校验最终配置；本入口不查询环境变量或真实用户目录。
///
/// 仅忽略用户级与项目级文件的 `NotFound`。相对路径以定义该字段的
/// 配置文件目录为基准；列表替换低层列表，显式空列表会清空继承值。
///
/// # Errors
///
/// 读取、TOML 解析或最终值校验失败时返回带来源路径的错误。
pub fn load_from_paths(paths: &ConfigPaths) -> Result<AppConfig, ConfigError> {
    let mut config = AppConfig::default();
    let mut sources = HashMap::new();
    for (path, required, trust) in [
        (paths.user.as_deref(), false, Trust::User),
        (Some(paths.project.as_path()), false, Trust::Project),
        (paths.explicit.as_deref(), true, Trust::User),
    ] {
        let Some(path) = path else { continue };
        if let Some(layer) = read_layer(path, required)? {
            layer.apply(&mut config, path, trust, &mut sources)?;
        }
    }
    validate(&config, &sources)?;
    Ok(config)
}

fn read_layer(path: &Path, required: bool) -> Result<Option<FileConfig>, ConfigError> {
    let text = match read_bounded(path) {
        Ok(Some(text)) => text,
        Ok(None) if !required => return Ok(None),
        Ok(None) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "显式指定的配置文件不存在",
                ),
            });
        }
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
}

/// 读取配置正文；`Ok(None)` 只表示文件不存在。
///
/// 只接受普通文件并限制长度：项目配置来自仓库，指向 FIFO 或超大日志的
/// 文件不该让启动挂起或耗尽内存。不跟随符号链接，避免把 `~/.aws/credentials`
/// 这类本地文件读进报错信息。
fn read_bounded(path: &Path) -> Result<Option<String>, std::io::Error> {
    use std::io::Read;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "只接受不超过 64 KiB 的普通文件（不跟随符号链接）",
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("配置文件超过 {MAX_CONFIG_BYTES} 字节上限"),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "读取期间配置文件增长到上限以上",
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

// 文件侧每个叶字段均保留缺省状态，避免把显式默认值误当成未配置。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    llm: LlmSection,
    run: RunSection,
    policy_path: Option<PathBuf>,
    mcp_config_path: Option<PathBuf>,
    skills: ExtensionSection,
    hooks: ExtensionSection,
    parallel: ParallelSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmSection {
    base_url: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunSection {
    max_turns: Option<u32>,
    max_wall_clock_seconds: Option<u64>,
    summarize_min_dropped: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionSection {
    enabled: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParallelSection {
    max_subtasks: Option<usize>,
    verify_command: Option<String>,
}

/// 配置来源的可信级别。
///
/// 项目级配置随仓库分发，克隆一个仓库就等于接受作者写的文件；用户级与
/// `QCH_CONFIG` 指定的文件是使用者自己写的。两者能设置的项目不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// 使用者自己写的文件。
    User,
    /// 工作区内的文件，可能来自任意仓库。
    Project,
}

impl FileConfig {
    fn apply(
        self,
        config: &mut AppConfig,
        path: &Path,
        trust: Trust,
        sources: &mut HashMap<&'static str, PathBuf>,
    ) -> Result<(), ConfigError> {
        if trust == Trust::Project {
            for (field, present) in [
                ("llm.base_url", self.llm.base_url.is_some()),
                ("policy_path", self.policy_path.is_some()),
                ("mcp_config_path", self.mcp_config_path.is_some()),
            ] {
                if present {
                    return Err(ConfigError::Invalid {
                        path: path.to_path_buf(),
                        field,
                        message: format!(
                            "项目级配置不能设置 {field}；请写进用户级配置或改用环境变量（{}）",
                            match field {
                                "llm.base_url" => "LLM_BASE_URL",
                                "policy_path" => "QCH_POLICY",
                                _ => "QCH_MCP_CONFIG",
                            }
                        ),
                    });
                }
            }
        }
        // 来源与值同步替换，使最终校验指向真正定义该值的文件。
        macro_rules! overlay {
            ($target:expr, $value:expr, $field:literal) => {
                if let Some(value) = $value {
                    $target = value;
                    sources.insert($field, path.to_path_buf());
                }
            };
        }

        overlay!(
            config.llm.base_url,
            self.llm.base_url.map(Some),
            "llm.base_url"
        );
        overlay!(config.llm.model, self.llm.model.map(Some), "llm.model");
        overlay!(config.run.max_turns, self.run.max_turns, "run.max_turns");
        overlay!(
            config.run.max_wall_clock_seconds,
            self.run.max_wall_clock_seconds.map(Some),
            "run.max_wall_clock_seconds"
        );
        overlay!(
            config.run.summarize_min_dropped,
            self.run.summarize_min_dropped.map(Some),
            "run.summarize_min_dropped"
        );
        overlay!(
            config.policy_path,
            self.policy_path
                .map(|value| resolve_path(value, path, "policy_path"))
                .transpose()?
                .map(Some),
            "policy_path"
        );
        overlay!(
            config.mcp_config_path,
            self.mcp_config_path
                .map(|value| resolve_path(value, path, "mcp_config_path"))
                .transpose()?
                .map(Some),
            "mcp_config_path"
        );
        overlay!(config.skills.enabled, self.skills.enabled.map(Some), "skills.enabled");
        overlay!(config.hooks.enabled, self.hooks.enabled.map(Some), "hooks.enabled");
        overlay!(
            config.parallel.max_subtasks,
            self.parallel.max_subtasks,
            "parallel.max_subtasks"
        );
        overlay!(
            config.parallel.verify_command,
            self.parallel.verify_command.map(Some),
            "parallel.verify_command"
        );
        Ok(())
    }
}

fn resolve_path(
    value: PathBuf,
    source: &Path,
    field: &'static str,
) -> Result<PathBuf, ConfigError> {
    // 空串会被 `join` 变成配置文件所在目录，之后读它只会得到一条
    // "拒绝访问"的误导性 IO 错误。TOML 没有 null，写不出"清除继承值"，
    // 因此空路径一律按笔误处理。
    if value.as_os_str().is_empty() {
        return Err(ConfigError::Invalid {
            path: source.to_path_buf(),
            field,
            message: "必须是非空路径".to_owned(),
        });
    }
    Ok(if value.is_absolute() {
        value
    } else {
        source.parent().unwrap_or(Path::new("")).join(value)
    })
}

fn validate(
    config: &AppConfig,
    sources: &HashMap<&'static str, PathBuf>,
) -> Result<(), ConfigError> {
    let invalid = |field, message: String| ConfigError::Invalid {
        // 默认值均有效，因此无效字段一定来自某个配置文件。
        path: sources[field].clone(),
        field,
        message,
    };
    for (field, valid, message) in [
        (
            "llm.base_url",
            config
                .llm
                .base_url
                .as_ref()
                .is_none_or(|value| !value.trim().is_empty()),
            "必须是非空字符串",
        ),
        (
            "llm.model",
            config
                .llm
                .model
                .as_ref()
                .is_none_or(|value| !value.trim().is_empty()),
            "必须是非空字符串",
        ),
        ("run.max_turns", config.run.max_turns > 0, "必须大于 0"),
        (
            "run.max_wall_clock_seconds",
            config.run.max_wall_clock_seconds != Some(0),
            "必须大于 0",
        ),
        (
            "run.summarize_min_dropped",
            config.run.summarize_min_dropped != Some(0),
            "必须大于 0",
        ),
        (
            "parallel.max_subtasks",
            config.parallel.max_subtasks <= 16,
            "必须在 0..=16 范围内",
        ),
        (
            "parallel.verify_command",
            config
                .parallel
                .verify_command
                .as_ref()
                .is_none_or(|command| !command.trim().is_empty()),
            "必须是非空命令",
        ),
    ] {
        if !valid {
            return Err(invalid(field, message.to_owned()));
        }
    }
    for (field, enabled) in [
        ("skills.enabled", &config.skills.enabled),
        ("hooks.enabled", &config.hooks.enabled),
    ] {
        let Some(enabled) = enabled else { continue };
        let mut seen = HashSet::new();
        for id in enabled {
            // 与扩展安装共用同一套校验：配置能写出来的 ID 必须能装得上，
            // 否则用户会拿到"未知扩展"这种误导性错误。
            validate_safe_identifier(field, id, 56).map_err(|message| invalid(field, message))?;
            if !seen.insert(id) {
                return Err(invalid(field, format!("标识符 {id:?} 重复")));
            }
        }
    }
    Ok(())
}

/// 配置加载错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取配置文件 {path} 失败: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("解析配置文件 {path} 失败: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("配置文件 {path} 的 {field} 无效: {message}")]
    Invalid {
        path: PathBuf,
        field: &'static str,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn link_file(original: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(original, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(original, link)
        }
    }

    struct Fixture {
        root: PathBuf,
        paths: ConfigPaths,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "qch-config-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).expect("创建独立测试目录");
            Self {
                paths: ConfigPaths {
                    user: Some(root.join("user/config.toml")),
                    project: root.join("project/.qcodeharness/config.toml"),
                    explicit: None,
                },
                root,
            }
        }

        fn write(&self, path: &Path, text: &str) {
            std::fs::create_dir_all(path.parent().expect("配置父目录")).expect("创建配置目录");
            std::fs::write(path, text).expect("写入测试配置");
        }

        fn user(&self, text: &str) {
            self.write(self.paths.user.as_ref().expect("用户配置路径"), text);
        }

        fn project(&self, text: &str) {
            self.write(&self.paths.project, text);
        }

        fn explicit(&mut self, text: &str) {
            let path = self.root.join("explicit/config.toml");
            self.write(&path, text);
            self.paths.explicit = Some(path);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).expect("清理测试目录");
        }
    }

    #[test]
    fn absent_implicit_files_use_defaults() {
        let mut fixture = Fixture::new();
        assert_eq!(
            load_from_paths(&fixture.paths).unwrap(),
            AppConfig::default()
        );
        fixture.paths.user = None;
        assert_eq!(
            load_from_paths(&fixture.paths).unwrap(),
            AppConfig::default()
        );
    }

    #[test]
    fn project_lists_replace_user_lists() {
        let fixture = Fixture::new();
        fixture.user("[skills]\nenabled = ['user-skill']\n[hooks]\nenabled = ['user-hook']");
        fixture.project("[skills]\nenabled = ['project-skill']");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.skills.enabled.as_deref(), Some(&["project-skill".to_owned()][..]));
        assert_eq!(config.hooks.enabled.as_deref(), Some(&["user-hook".to_owned()][..]));
    }

    #[test]
    fn explicit_empty_lists_clear_inherited_extensions() {
        let fixture = Fixture::new();
        fixture.user("[skills]\nenabled = ['user-skill']\n[hooks]\nenabled = ['user-hook']");
        fixture.project("[skills]\nenabled = []\n[hooks]\nenabled = []");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.skills.enabled.as_deref(), Some(&[][..]));
        assert_eq!(config.hooks.enabled.as_deref(), Some(&[][..]));
    }

    #[test]
    fn explicit_default_four_overrides_nondefault_parallelism() {
        let fixture = Fixture::new();
        fixture.user("[parallel]\nmax_subtasks = 12");
        fixture.project("[parallel]\nmax_subtasks = 4");
        assert_eq!(
            load_from_paths(&fixture.paths)
                .unwrap()
                .parallel
                .max_subtasks,
            4
        );
    }

    #[test]
    fn empty_sections_preserve_lower_layer_fields() {
        let fixture = Fixture::new();
        fixture.user("[skills]\nenabled = ['user-skill']\n[parallel]\nmax_subtasks = 12");
        fixture.project("[skills]\n[parallel]");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.skills.enabled.as_deref(), Some(&["user-skill".to_owned()][..]));
        assert_eq!(config.parallel.max_subtasks, 12);
    }

    #[test]
    fn explicit_file_is_a_final_overlay() {
        let mut fixture = Fixture::new();
        fixture.user("[skills]\nenabled = ['user-skill']");
        fixture.project("[hooks]\nenabled = ['project-hook']");
        fixture.explicit("[parallel]\nmax_subtasks = 8");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.skills.enabled.as_deref(), Some(&["user-skill".to_owned()][..]));
        assert_eq!(config.hooks.enabled.as_deref(), Some(&["project-hook".to_owned()][..]));
        assert_eq!(config.parallel.max_subtasks, 8);
    }

    #[test]
    fn user_file_supplies_all_settings() {
        let fixture = Fixture::new();
        fixture.user(
            "policy_path = 'policy.toml'\nmcp_config_path = 'mcp.json'\n\
             [llm]\nbase_url = 'https://example.invalid/v1'\nmodel = 'local-model'\n\
             [run]\nmax_turns = 30\nmax_wall_clock_seconds = 90\nsummarize_min_dropped = 6\n\
             [skills]\nenabled = ['a-z_09', '0', 'a_', 'b-']\n\
             [hooks]\nenabled = ['before-run']\n[parallel]\nmax_subtasks = 16",
        );
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(
            config.llm.base_url.as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(config.llm.model.as_deref(), Some("local-model"));
        assert_eq!(config.run.max_turns, 30);
        assert_eq!(config.run.max_wall_clock_seconds, Some(90));
        assert_eq!(config.run.summarize_min_dropped, Some(6));
        assert_eq!(
            config.policy_path,
            Some(fixture.root.join("user/policy.toml"))
        );
        assert_eq!(
            config.mcp_config_path,
            Some(fixture.root.join("user/mcp.json"))
        );
        assert_eq!(
            config.skills.enabled.as_deref(),
            Some(&["a-z_09", "0", "a_", "b-"].map(str::to_owned)[..])
        );
        assert_eq!(
            config.hooks.enabled.as_deref(),
            Some(&["before-run".to_owned()][..])
        );
        assert_eq!(config.parallel.max_subtasks, 16);
    }

    #[test]
    fn partial_sections_merge_per_field_and_twenty_resets_turn_budget() {
        let mut fixture = Fixture::new();
        fixture.user(
            "[llm]\nbase_url = 'https://user.invalid/v1'\nmodel = 'user-model'\n\
             [run]\nmax_turns = 80\nmax_wall_clock_seconds = 60\nsummarize_min_dropped = 3",
        );
        fixture.project("[llm]\nmodel = 'project-model'\n[run]\nmax_turns = 20");
        fixture.explicit("[run]\nsummarize_min_dropped = 9");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(
            config.llm.base_url.as_deref(),
            Some("https://user.invalid/v1")
        );
        assert_eq!(config.llm.model.as_deref(), Some("project-model"));
        assert_eq!(config.run.max_turns, 20);
        assert_eq!(config.run.max_wall_clock_seconds, Some(60));
        assert_eq!(config.run.summarize_min_dropped, Some(9));
    }

    #[test]
    fn empty_file_and_empty_sections_preserve_all_inherited_values() {
        let mut fixture = Fixture::new();
        fixture.user(
            "policy_path = 'policy.toml'\nmcp_config_path = 'mcp.json'\n\
             [llm]\nbase_url = 'https://user.invalid/v1'\nmodel = 'user-model'\n\
             [run]\nmax_turns = 80\nmax_wall_clock_seconds = 60\nsummarize_min_dropped = 3\n\
             [skills]\nenabled = ['user-skill']\n[hooks]\nenabled = ['user-hook']\n\
             [parallel]\nmax_subtasks = 12",
        );
        let inherited = load_from_paths(&fixture.paths).unwrap();
        fixture.project("[llm]\n[run]\n[skills]\n[hooks]\n[parallel]");
        fixture.explicit("");
        assert_eq!(load_from_paths(&fixture.paths).unwrap(), inherited);
    }

    #[test]
    fn explicit_layer_replaces_lists_and_resets_defaults() {
        let mut fixture = Fixture::new();
        fixture.user("[skills]\nenabled = ['user-skill']\n[hooks]\nenabled = ['user-hook']");
        fixture.project("[run]\nmax_turns = 80\n[parallel]\nmax_subtasks = 12");
        fixture.explicit(
            "[skills]\nenabled = []\n[hooks]\nenabled = ['explicit-hook']\n\
             [run]\nmax_turns = 20\n[parallel]\nmax_subtasks = 4",
        );
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.skills.enabled.as_deref(), Some(&[][..]));
        assert_eq!(config.hooks.enabled.as_deref(), Some(&["explicit-hook".to_owned()][..]));
        assert_eq!(config.run.max_turns, 20);
        assert_eq!(config.parallel.max_subtasks, 4);
    }

    #[test]
    fn paths_stay_relative_to_the_file_that_defines_each_value() {
        let mut fixture = Fixture::new();
        fixture.user("policy_path = 'user-policy.toml'\nmcp_config_path = 'user-mcp.json'");
        let user_config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(
            user_config.policy_path,
            Some(fixture.root.join("user/user-policy.toml"))
        );
        assert_eq!(
            user_config.mcp_config_path,
            Some(fixture.root.join("user/user-mcp.json"))
        );

        fixture.explicit("policy_path = 'explicit-policy.toml'\nmcp_config_path = '../explicit-mcp.json'");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(
            config.policy_path,
            Some(fixture.root.join("explicit/explicit-policy.toml"))
        );
        assert_eq!(
            config.mcp_config_path,
            Some(fixture.root.join("explicit/../explicit-mcp.json"))
        );
    }

    #[test]
    fn absolute_reference_paths_are_preserved_without_opening_them() {
        let fixture = Fixture::new();
        let policy = fixture.root.join("absent-policy.toml");
        let mcp = fixture.root.join("absent-mcp.json");
        fixture.user(&format!(
            "policy_path = {}\nmcp_config_path = {}",
            toml::Value::String(policy.to_string_lossy().into_owned()),
            toml::Value::String(mcp.to_string_lossy().into_owned()),
        ));
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.policy_path, Some(policy));
        assert_eq!(config.mcp_config_path, Some(mcp));
    }

    #[test]
    fn explicit_file_is_required_even_when_implicit_files_are_valid() {
        for empty in [false, true] {
            let mut fixture = Fixture::new();
            fixture.user("[skills]\nenabled = ['user-skill']");
            let path = if empty {
                PathBuf::new()
            } else {
                fixture.root.join("missing.toml")
            };
            fixture.paths.explicit = Some(path.clone());
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(
                matches!(error, ConfigError::Read { path: ref actual, ref source }
                if actual == &path && source.kind() == std::io::ErrorKind::NotFound)
            );
        }
    }

    #[test]
    fn non_notfound_io_errors_are_reported_for_every_layer() {
        for layer in 0..3 {
            let mut fixture = Fixture::new();
            fixture.explicit("");
            let path = match layer {
                0 => fixture.paths.user.as_ref().unwrap().clone(),
                1 => fixture.paths.project.clone(),
                _ => fixture.paths.explicit.as_ref().unwrap().clone(),
            };
            if layer == 2 {
                std::fs::remove_file(&path).unwrap();
            }
            std::fs::create_dir_all(&path).unwrap();
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(error.to_string().contains(&path.display().to_string()));
            assert!(matches!(error, ConfigError::Read { path: actual, source }
                if actual == path && source.kind() != std::io::ErrorKind::NotFound));
        }
    }

    #[test]
    fn invalid_utf8_is_an_io_error_with_path_context() {
        let fixture = Fixture::new();
        fixture.project("");
        std::fs::write(&fixture.paths.project, [0xff, 0xfe]).unwrap();
        assert!(matches!(load_from_paths(&fixture.paths),
            Err(ConfigError::Read { path, source })
                if path == fixture.paths.project && source.kind() == std::io::ErrorKind::InvalidData));
    }

    #[test]
    fn invalid_toml_in_any_layer_is_not_hidden_by_later_files() {
        for layer in 0..3 {
            let mut fixture = Fixture::new();
            fixture.user("");
            fixture.project("");
            fixture.explicit("");
            let path = match layer {
                0 => fixture.paths.user.as_ref().unwrap(),
                1 => &fixture.paths.project,
                _ => fixture.paths.explicit.as_ref().unwrap(),
            };
            fixture.write(path, "[broken");
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(error.to_string().contains(&path.display().to_string()));
            assert!(matches!(error, ConfigError::Parse { path: actual, .. } if &actual == path));
        }
    }

    #[test]
    fn unknown_root_and_nested_fields_are_rejected_with_path_context() {
        for text in [
            "unknown = 1",
            "[llm]\nunknown = 1",
            "[run]\nunknown = 1",
            "[skills]\nunknown = 1",
            "[hooks]\nunknown = 1",
            "[parallel]\nunknown = 1",
        ] {
            let fixture = Fixture::new();
            fixture.project(text);
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(error.to_string().contains("unknown"));
            assert!(
                matches!(error, ConfigError::Parse { path, .. } if path == fixture.paths.project)
            );
        }
    }

    #[test]
    fn wrong_types_negative_numbers_and_overflow_are_parse_errors() {
        for text in [
            "[llm]\nmodel = 1",
            "[llm]\nbase_url = false",
            "[run]\nmax_turns = -1",
            "[run]\nmax_turns = 4294967296",
            "[run]\nmax_wall_clock_seconds = -1",
            "[run]\nsummarize_min_dropped = -1",
            "[parallel]\nmax_subtasks = -1",
            "[skills]\nenabled = 'skill'",
            "[hooks]\nenabled = [1]",
            "policy_path = []",
            "mcp_config_path = 1",
            "[run]\nmax_turns = 1\nmax_turns = 2",
        ] {
            let fixture = Fixture::new();
            fixture.project(text);
            assert!(
                matches!(load_from_paths(&fixture.paths),
                Err(ConfigError::Parse { path, .. }) if path == fixture.paths.project),
                "{text}"
            );
        }
    }

    #[test]
    fn invalid_scalar_values_report_the_defining_file_and_field() {
        for (text, field) in [
            ("[llm]\nbase_url = ''", "llm.base_url"),
            ("[llm]\nbase_url = '   '", "llm.base_url"),
            ("[llm]\nmodel = ''", "llm.model"),
            ("[llm]\nmodel = '\t '", "llm.model"),
            ("[run]\nmax_turns = 0", "run.max_turns"),
            (
                "[run]\nmax_wall_clock_seconds = 0",
                "run.max_wall_clock_seconds",
            ),
            (
                "[run]\nsummarize_min_dropped = 0",
                "run.summarize_min_dropped",
            ),
            ("[parallel]\nmax_subtasks = 17", "parallel.max_subtasks"),
        ] {
            let mut fixture = Fixture::new();
            fixture.user(text);
            fixture.project("[llm]\n[run]\n[parallel]");
            fixture.explicit("");
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(error.to_string().contains(field));
            assert!(
                matches!(error, ConfigError::Invalid { path, field: actual, .. }
                if Some(&path) == fixture.paths.user.as_ref() && actual == field),
                "{text}"
            );
        }
    }

    #[test]
    fn verify_command_is_inherited_and_empty_values_are_rejected() {
        let fixture = Fixture::new();
        fixture.user("[parallel]\nverify_command = 'cargo test --quiet'");
        fixture.project("[parallel]\nmax_subtasks = 2");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(
            config.parallel.verify_command.as_deref(),
            Some("cargo test --quiet")
        );
        assert_eq!(config.parallel.max_subtasks, 2);

        for text in [
            "[parallel]\nverify_command = ''",
            "[parallel]\nverify_command = '   '",
        ] {
            let fixture = Fixture::new();
            fixture.project(text);
            assert!(matches!(load_from_paths(&fixture.paths),
                Err(ConfigError::Invalid { field: "parallel.verify_command", .. })));
        }
    }

    #[test]
    fn positive_run_budgets_and_parallel_boundaries_are_accepted() {
        for max_subtasks in [0, 1, 4, 16] {
            let fixture = Fixture::new();
            fixture.project(&format!(
                "[run]\nmax_turns = 1\nmax_wall_clock_seconds = 1\nsummarize_min_dropped = 1\n\
                 [parallel]\nmax_subtasks = {max_subtasks}",
            ));
            let config = load_from_paths(&fixture.paths).unwrap();
            assert_eq!(config.run.max_turns, 1);
            assert_eq!(config.run.max_wall_clock_seconds, Some(1));
            assert_eq!(config.run.summarize_min_dropped, Some(1));
            assert_eq!(config.parallel.max_subtasks, max_subtasks);
        }
        let fixture = Fixture::new();
        fixture
            .project("[run]\nmax_turns = 4294967295\nmax_wall_clock_seconds = 9223372036854775807");
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.run.max_turns, u32::MAX);
        assert_eq!(config.run.max_wall_clock_seconds, Some(i64::MAX as u64));
    }

    #[test]
    fn malformed_and_duplicate_extension_ids_are_rejected_for_both_lists() {
        for section in ["skills", "hooks"] {
            for list in [
                "['']",
                "['Upper']",
                "['a.b']",
                "['a/b']",
                "['é']",
                "['a b']",
                "['a', 'a']",
            ] {
                let fixture = Fixture::new();
                fixture.project(&format!("[{section}]\nenabled = {list}"));
                assert!(
                    matches!(load_from_paths(&fixture.paths),
                    Err(ConfigError::Invalid { path, field, .. })
                        if path == fixture.paths.project && field == format!("{section}.enabled")),
                    "{section}: {list}"
                );
            }
        }
    }

    #[test]
    fn only_final_values_are_validated_after_all_overlays() {
        let mut fixture = Fixture::new();
        fixture.user(
            "[llm]\nbase_url = ''\nmodel = ''\n\
             [run]\nmax_turns = 0\nmax_wall_clock_seconds = 0\nsummarize_min_dropped = 0\n\
             [parallel]\nmax_subtasks = 17\n[skills]\nenabled = ['UPPER']\n[hooks]\nenabled = ['a', 'a']",
        );
        fixture.project("[run]\nmax_turns = 20");
        fixture.explicit(
            "[llm]\nbase_url = 'https://explicit.invalid/v1'\nmodel = 'explicit-model'\n\
             [run]\nmax_wall_clock_seconds = 1\nsummarize_min_dropped = 1\n\
             [parallel]\nmax_subtasks = 4\n[skills]\nenabled = []\n[hooks]\nenabled = []",
        );
        let config = load_from_paths(&fixture.paths).unwrap();
        assert_eq!(config.llm.model.as_deref(), Some("explicit-model"));
        assert_eq!(config.run.max_turns, 20);
        assert_eq!(config.parallel.max_subtasks, 4);
        assert_eq!(config.skills.enabled.as_deref(), Some(&[][..]));
        assert_eq!(config.hooks.enabled.as_deref(), Some(&[][..]));
    }

    /// 项目级配置来自仓库，不能设置能改变执行方或数据去向的字段。
    #[test]
    fn project_config_cannot_redirect_the_model_policy_or_mcp_servers() {
        for (text, field) in [
            ("[llm]\nbase_url = 'https://evil.invalid/v1'", "llm.base_url"),
            ("policy_path = 'allow-all.toml'", "policy_path"),
            ("mcp_config_path = 'mcp.json'", "mcp_config_path"),
        ] {
            let fixture = Fixture::new();
            fixture.project(text);
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(
                matches!(error, ConfigError::Invalid { ref path, field: actual, .. }
                    if path == &fixture.paths.project && actual == field),
                "{text}: {error}"
            );
        }
        // 同一份内容写在用户级或 QCH_CONFIG 里是允许的。
        let fixture = Fixture::new();
        fixture.user("policy_path = 'p.toml'\n[llm]\nbase_url = 'https://mine.invalid/v1'");
        assert_eq!(
            load_from_paths(&fixture.paths).unwrap().llm.base_url.as_deref(),
            Some("https://mine.invalid/v1")
        );
    }

    #[test]
    fn validation_error_source_changes_when_a_higher_layer_redefines_a_field() {
        let mut fixture = Fixture::new();
        fixture.user("[run]\nmax_turns = 0");
        fixture.project("[run]\nmax_turns = 0");
        fixture.explicit("");
        assert!(matches!(load_from_paths(&fixture.paths),
            Err(ConfigError::Invalid { path, field: "run.max_turns", .. }) if path == fixture.paths.project));
        fixture.explicit("[run]\nmax_turns = 0");
        assert!(matches!(load_from_paths(&fixture.paths),
            Err(ConfigError::Invalid { path, field: "run.max_turns", .. }) if Some(&path) == fixture.paths.explicit.as_ref()));
    }

    #[test]
    fn example_loads_through_the_public_file_loader() {
        let fixture = Fixture::new();
        // 示例里的列表项是注释，复制成项目级配置后不改变任何继承值。
        fixture.project(include_str!("../../config.example.toml"));
        assert_eq!(
            load_from_paths(&fixture.paths).unwrap(),
            AppConfig::default()
        );
    }

    #[test]
    fn empty_path_values_are_rejected_instead_of_resolving_to_the_directory() {
        for (text, field) in [
            ("policy_path = ''", "policy_path"),
            ("mcp_config_path = ''", "mcp_config_path"),
        ] {
            let fixture = Fixture::new();
            fixture.user(text);
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(
                matches!(error, ConfigError::Invalid { field: actual, .. } if actual == field),
                "{text}: {error}"
            );
        }
    }

    /// 符号链接、目录与超大文件都在读取阶段被拒，不跟随链接。
    #[test]
    fn config_reads_are_bounded_and_never_follow_links() {
        let fixture = Fixture::new();
        let directory = fixture.root.join("user");
        std::fs::create_dir_all(&directory).unwrap();
        let target = fixture.paths.user.as_ref().unwrap().clone();

        // 目录：不是普通文件。
        std::fs::create_dir(&target).unwrap();
        let error = load_from_paths(&fixture.paths).unwrap_err();
        assert!(error.to_string().contains("普通文件"), "{error}");
        std::fs::remove_dir(&target).unwrap();

        // 超过 64 KiB。
        std::fs::write(&target, "x".repeat(65 * 1024)).unwrap();
        let error = load_from_paths(&fixture.paths).unwrap_err();
        assert!(error.to_string().contains("上限"), "{error}");

        // 符号链接：不跟随。Windows 无权限建链接时跳过这一分支。
        std::fs::remove_file(&target).unwrap();
        let real = fixture.root.join("real.toml");
        std::fs::write(&real, "").unwrap();
        if link_file(&real, &target).is_ok() {
            let error = load_from_paths(&fixture.paths).unwrap_err();
            assert!(error.to_string().contains("符号链接"), "{error}");
        }
    }

    #[test]
    fn discovery_and_external_env_precedence_are_isolated_in_child_processes() {
        const CHILD_WORKSPACE: &str = "QCH_CONFIG_TEST_CHILD_WORKSPACE";
        const EXPECT_EXPLICIT: &str = "QCH_CONFIG_TEST_EXPECT_EXPLICIT";
        if let Some(workspace) = std::env::var_os(CHILD_WORKSPACE) {
            let workspace = PathBuf::from(workspace);
            let paths = discover(&workspace);
            assert_eq!(
                paths.user,
                dirs::config_dir().map(|dir| dir.join("qcodeharness/config.toml"))
            );
            assert_eq!(paths.project, workspace.join(".qcodeharness/config.toml"));
            let expected = (std::env::var(EXPECT_EXPLICIT).unwrap() == "true")
                .then(|| workspace.join("explicit.toml"));
            assert_eq!(paths.explicit, expected);
            // 模型地址与策略路径属于用户可信层，这里把它当作用户级文件读取；
            // 项目级位置只用于验证 discover 的路径拼接。
            let config = load_from_paths(&ConfigPaths {
                user: Some(workspace.join(".qcodeharness/config.toml")),
                project: workspace.join("absent-project.toml"),
                explicit: None,
            })
            .unwrap();
            assert_eq!(
                config.llm.base_url.as_deref(),
                Some("https://file.invalid/v1")
            );
            assert_eq!(config.llm.model.as_deref(), Some("file-model"));
            assert_eq!(
                config.policy_path,
                Some(workspace.join(".qcodeharness/policy.toml"))
            );
            assert_eq!(
                config.mcp_config_path,
                Some(workspace.join(".qcodeharness/mcp.json"))
            );
            return;
        }
        let fixture = Fixture::new();
        fixture.user(
            "policy_path = 'policy.toml'\nmcp_config_path = 'mcp.json'\n\
             [llm]\nbase_url = 'https://file.invalid/v1'\nmodel = 'file-model'",
        );
        let workspace = fixture.root.join("project");
        std::fs::create_dir_all(workspace.join(".qcodeharness")).unwrap();
        std::fs::copy(
            fixture.paths.user.as_ref().unwrap(),
            workspace.join(".qcodeharness/config.toml"),
        )
        .unwrap();
        for explicit in [true, false] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "infrastructure::config::tests::discovery_and_external_env_precedence_are_isolated_in_child_processes"])
                .env(CHILD_WORKSPACE, &workspace)
                .env(EXPECT_EXPLICIT, explicit.to_string())
                .env("LLM_BASE_URL", "https://env.invalid/v1")
                .env("LLM_MODEL", "env-model")
                .env("QCH_POLICY", "env-policy.toml")
                .env("QCH_MCP_CONFIG", "env-mcp.json");
            if explicit {
                child.env("QCH_CONFIG", workspace.join("explicit.toml"));
            } else {
                child.env_remove("QCH_CONFIG");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
