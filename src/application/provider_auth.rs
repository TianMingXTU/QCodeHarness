//! 多 provider 认证面的应用层契约。
//!
//! 凭据值只保存在内存里，Debug、序列化与错误文本都不携带它。订阅式登录
//! 只接受环境变量或用户配置目录，绝不回退到项目工作区，避免仓库里的
//! JSON 被当成登录凭据。

use std::any::Any;
use std::fmt::{self, Debug};

use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

/// 认证方式。两种方式的差别只在 HTTP 头形状，不改变模型协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    ApiKey,
    SubscriptionLogin,
}

/// 内存凭据。Debug 与 serde 都只输出认证方式、端点，不输出 secret。
#[derive(Clone, Deserialize)]
pub struct ProviderCredentials {
    #[serde(default)]
    secret: String,
    base_url: String,
    mode: AuthMode,
}

impl ProviderCredentials {
    pub fn new(mode: AuthMode, secret: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self { secret: secret.into(), base_url: base_url.into(), mode }
    }

    pub fn auth_mode(&self) -> AuthMode {
        self.mode
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// API key 走 `x-api-key`；订阅登录走 `authorization`。
    pub fn header_name(&self) -> &'static str {
        match self.mode {
            AuthMode::ApiKey => "x-api-key",
            AuthMode::SubscriptionLogin => "authorization",
        }
    }

    /// 返回 HTTP 头的值。订阅登录按 Bearer token 形状输出。
    pub fn authorization(&self) -> String {
        match self.mode {
            AuthMode::ApiKey => self.secret.clone(),
            AuthMode::SubscriptionLogin => format!("Bearer {}", self.secret),
        }
    }

    /// rig 的 Messages 编码器始终要求 `x-api-key`，两种方式都传 token 本体。
    pub fn api_key_value(&self) -> &str {
        &self.secret
    }

    /// 是否还需叠加 `authorization: Bearer …`。
    pub fn bearer_header(&self) -> Option<String> {
        match self.mode {
            AuthMode::ApiKey => None,
            AuthMode::SubscriptionLogin => Some(self.authorization()),
        }
    }

    pub fn from_subscription(source: &SubscriptionCredentialSource) -> Result<Self, ProviderCredentialError> {
        let token = source.subscription_token()?;
        Ok(Self::new(
            AuthMode::SubscriptionLogin,
            token,
            "https://api.anthropic.com",
        ))
    }
}

impl Serialize for ProviderCredentials {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ProviderCredentials", 3)?;
        state.serialize_field("secret", "<redacted>")?;
        state.serialize_field("base_url", &self.base_url)?;
        state.serialize_field("mode", &self.mode)?;
        state.end()
    }
}

impl Debug for ProviderCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentials")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// 订阅凭据的输入源。文件内容以字节传入，读取发生在基础设施装配层。
#[derive(Debug, Clone, Default)]
pub struct SubscriptionCredentialSource {
    /// 已采集的环境变量，按 (name, value) 给出。
    pub environment: Vec<(String, String)>,
    /// 用户配置目录里的凭据文件（文件名、内容）。
    pub user_config_file: Option<(String, Vec<u8>)>,
    /// 仅用于断言「项目目录不被读取」，实现必须忽略它。
    pub project_config_file: Option<(String, Vec<u8>)>,
}

impl SubscriptionCredentialSource {
    /// 优先级固定为环境变量，再用户配置目录；项目目录永不参与。
    fn subscription_token(&self) -> Result<String, ProviderCredentialError> {
        for name in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_SUBSCRIPTION_TOKEN",
            "QCH_ANTHROPIC_SUBSCRIPTION_TOKEN",
        ] {
            if let Some((_, value)) = self.environment.iter().find(|(key, _)| key == name) {
                if !value.trim().is_empty() {
                    return Ok(value.trim().to_owned());
                }
            }
        }
        if let Some((_, content)) = &self.user_config_file {
            let json: serde_json::Value = serde_json::from_slice(content)
                .map_err(|error| ProviderCredentialError::MalformedSubscriptionCredential(error.to_string()))?;
            for key in ["access_token", "subscription_token", "token"] {
                if let Some(value) = json.get(key).and_then(serde_json::Value::as_str) {
                    if !value.trim().is_empty() {
                        return Ok(value.trim().to_owned());
                    }
                }
            }
        }
        Err(ProviderCredentialError::MissingSubscriptionCredential)
    }
}

/// 凭据错误只说明类型与解析原因，不携带任何候选值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCredentialError {
    MissingSubscriptionCredential,
    MalformedSubscriptionCredential(String),
}

impl fmt::Display for ProviderCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSubscriptionCredential => {
                formatter.write_str("未找到订阅登录凭据（只检查环境变量与用户配置目录）")
            }
            Self::MalformedSubscriptionCredential(reason) => {
                write!(formatter, "订阅登录凭据文件解析失败: {reason}")
            }
        }
    }
}

impl std::error::Error for ProviderCredentialError {}

/// 可注入的传输句柄。application 只保存类型擦除引用，具体发送与请求
/// 记录由 infrastructure 层实现；未知传输显式报错，不猜测协议语义。
pub trait HttpTransport: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_ignores_project_config_bytes() {
        let source = SubscriptionCredentialSource {
            project_config_file: Some((
                "credentials.json".to_owned(),
                b"{\"access_token\":\"project\"}".to_vec(),
            )),
            ..Default::default()
        };
        assert_eq!(
            ProviderCredentials::from_subscription(&source).unwrap_err(),
            ProviderCredentialError::MissingSubscriptionCredential
        );
    }

    #[test]
    fn credentials_debug_and_json_have_no_secret() {
        let credentials = ProviderCredentials::new(AuthMode::ApiKey, "super-secret", "https://example.test");
        assert!(!format!("{credentials:?}").contains("super-secret"));
        assert!(!serde_json::to_string(&credentials).unwrap().contains("super-secret"));
    }
}
