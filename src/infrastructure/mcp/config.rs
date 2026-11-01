//! MCP Server 的配置：从 TOML 文件读要连哪些 Server。
//!
//! 独立于策略配置文件：策略是"允许什么"，MCP 是"接什么"，两者的
//! 变更频率与评审视角都不同，混在一个文件里会互相牵制。
//!
//! ```toml
//! [[mcp_servers]]
//! name = "fetch"
//! command = "uvx"
//! args = ["mcp-server-fetch"]
//! ```

use std::path::Path;

use serde::Deserialize;

/// MCP 配置文件的整体结构。
///
/// 拒绝未知字段：手误（`[[mcp_server]]` 少个 s）或多写的字段若被
/// 静默忽略，MCP 会无声关闭或配置悄悄失效——配置错误必须显式报错。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// 要连接的 MCP Server 列表。
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// 一个 MCP Server 的连接配置（P5 只支持 stdio 传输）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Server 名字：进工具名前缀（`mcp__<name>__<tool>`）、证据与错误信息。
    pub name: String,
    /// 启动命令（本地子进程）。
    pub command: String,
    /// 启动参数。
    #[serde(default)]
    pub args: Vec<String>,
}

impl McpConfig {
    /// 从 TOML 文件加载。文件不存在返回 `Ok(None)`——不配 MCP 是常态，
    /// 不该为缺文件报错；文件存在但内容非法才报错。
    ///
    /// # Errors
    ///
    /// 文件存在但读取失败或 TOML 非法时返回可读错误。
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("读取 MCP 配置失败: {error}")),
        };
        let config: Self =
            toml::from_str(&text).map_err(|error| format!("MCP 配置解析失败: {error}"))?;
        config.validate()?;
        Ok(Some(config))
    }

    /// 结构校验：Server 名字会进工具名前缀，字符集必须受控，否则
    /// `mcp__<name>__<tool>` 可能出现歧义或不可读的名字。
    /// 重名更要拒绝：两个同名 Server 的同款工具会在注册表里
    /// 静默互相覆盖。
    fn validate(&self) -> Result<(), String> {
        for (index, server) in self.mcp_servers.iter().enumerate() {
            if server.name.is_empty() {
                return Err("MCP Server 名字不能为空".to_owned());
            }
            if !server
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "MCP Server 名字 `{}` 只允许字母、数字、下划线与连字符",
                    server.name
                ));
            }
            if server.command.is_empty() {
                return Err(format!("MCP Server `{}` 的启动命令不能为空", server.name));
            }
            if self.mcp_servers[..index]
                .iter()
                .any(|earlier| earlier.name == server.name)
            {
                return Err(format!("MCP Server 名字 `{}` 重复", server.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("qch-mcp-cfg-{}-{name}", std::process::id()))
    }

    #[test]
    fn loads_servers_from_toml() {
        let path = temp_path("ok.toml");
        fs::write(
            &path,
            "[[mcp_servers]]\nname = \"filesystem\"\ncommand = \"npx\"\nargs = [\"-y\", \"@modelcontextprotocol/server-filesystem\", \".\"]\n\n[[mcp_servers]]\nname = \"fetch\"\ncommand = \"uvx\"\nargs = [\"mcp-server-fetch\"]\n",
        )
        .unwrap();

        let config = McpConfig::load(&path).unwrap().expect("文件存在应返回 Some");
        assert_eq!(config.mcp_servers.len(), 2);
        assert_eq!(config.mcp_servers[0].name, "filesystem");
        assert_eq!(config.mcp_servers[0].command, "npx");
        assert_eq!(
            config.mcp_servers[0].args,
            vec!["-y", "@modelcontextprotocol/server-filesystem", "."]
        );
        assert_eq!(config.mcp_servers[1].name, "fetch");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_means_no_mcp() {
        let path = temp_path("absent.toml");
        let _ = fs::remove_file(&path);
        assert_eq!(McpConfig::load(&path).unwrap(), None, "缺文件是常态，不是错误");
    }

    #[test]
    fn invalid_toml_is_an_error_with_reason() {
        let path = temp_path("bad.toml");
        fs::write(&path, "[[mcp_servers]\nname =").unwrap();
        let error = McpConfig::load(&path).unwrap_err();
        assert!(error.contains("解析失败"), "{error}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn server_names_are_constrained_to_safe_characters() {
        let path = temp_path("unsafe.toml");
        // 名字会进工具名前缀：空格等字符会让 `mcp__a b__tool` 变得含混。
        fs::write(&path, "[[mcp_servers]]\nname = \"a b\"\ncommand = \"x\"\n").unwrap();
        let error = McpConfig::load(&path).unwrap_err();
        assert!(error.contains("只允许"), "{error}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn typoed_keys_and_unknown_fields_are_rejected_not_ignored() {
        // serde 默认忽略未知字段：不拦的话，`[[mcp_server]]` 手误会让
        // MCP 无声关闭（解析成 0 个 Server），用户毫无线索。
        let typo = temp_path("typo.toml");
        fs::write(&typo, "[[mcp_server]]\nname = \"fetch\"\ncommand = \"uvx\"\n").unwrap();
        let error = McpConfig::load(&typo).unwrap_err();
        assert!(error.contains("解析失败"), "{error}");
        fs::remove_file(&typo).ok();

        let extra = temp_path("extra.toml");
        fs::write(
            &extra,
            "[[mcp_servers]]\nname = \"fetch\"\ncommand = \"uvx\"\nenv = [\"A=1\"]\n",
        )
        .unwrap();
        let error = McpConfig::load(&extra).unwrap_err();
        assert!(error.contains("解析失败"), "{error}");
        fs::remove_file(&extra).ok();
    }

    #[test]
    fn duplicate_server_names_are_rejected() {
        // 同名 Server 的同款工具会在注册表里静默互相覆盖，加载时就得拦下。
        let path = temp_path("dup.toml");
        fs::write(
            &path,
            "[[mcp_servers]]\nname = \"fs\"\ncommand = \"a\"\n\n[[mcp_servers]]\nname = \"fs\"\ncommand = \"b\"\n",
        )
        .unwrap();
        let error = McpConfig::load(&path).unwrap_err();
        assert!(error.contains("重复"), "{error}");
        fs::remove_file(&path).ok();
    }
}
