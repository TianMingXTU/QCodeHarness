//! `--doctor` 第 13 项：provider 连通性诊断的纯判定。
//!
//! 既有 12 项的采集和顺序属于 `application::doctor`；本模块只把凭据存在
//! 与探测结果翻译成一条 `DoctorCheck`，不读取环境、凭据或网络。事实类型
//! 在 `domain::doctor::ProviderConnectivityFacts`：domain 不能反向依赖
//! application，所以纯事实字段下沉到 domain，本模块只保留判定位形。

use crate::domain::doctor::{DoctorCheck, DoctorStatus, ProviderConnectivityFacts, ProviderConnectivityProbe};

/// 纯函数：输出固定 code，凭据缺失或 401 是 Error，超时/不可达是 Warn，
/// 未探测是 Unknown。detail 只抄录非敏感摘要，调用方负责先净化。
pub fn provider_connectivity_check(facts: &ProviderConnectivityFacts) -> DoctorCheck {
    let (status, message, recommendation) = match (facts.credential_present, facts.probe) {
        (false, _) => (
            DoctorStatus::Error,
            "未找到 Anthropic Messages 凭据".to_owned(),
            Some("配置 API key 或完成订阅式登录后重试".to_owned()),
        ),
        (true, ProviderConnectivityProbe::Ok) => (
            DoctorStatus::Ok,
            "Anthropic Messages 凭据存在，连通探测成功".to_owned(),
            None,
        ),
        (true, ProviderConnectivityProbe::Unauthorized) => (
            DoctorStatus::Error,
            "Anthropic Messages 凭据被服务端拒绝".to_owned(),
            Some("检查凭据是否有效或重新登录".to_owned()),
        ),
        (true, ProviderConnectivityProbe::Timeout | ProviderConnectivityProbe::Unreachable) => (
            DoctorStatus::Warn,
            format!(
                "Anthropic Messages 连通探测未完成{}",
                suffix(facts.detail.as_deref())
            ),
            Some("检查网络或稍后重试".to_owned()),
        ),
        (true, ProviderConnectivityProbe::Unknown) => (
            DoctorStatus::Unknown,
            "未提供 Anthropic Messages 连通探测结果".to_owned(),
            Some("启用连通探测后重新运行 --doctor".to_owned()),
        ),
    };
    DoctorCheck {
        code: "provider_connectivity".to_owned(),
        status,
        message,
        path: None,
        recommendation,
    }
}

fn suffix(detail: Option<&str>) -> String {
    match detail {
        Some(text) if !text.trim().is_empty() => format!("（{text}）"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_credential_is_error_even_without_probe() {
        let check = provider_connectivity_check(&ProviderConnectivityFacts {
            credential_present: false,
            probe: ProviderConnectivityProbe::Unknown,
            detail: None,
        });
        assert_eq!(check.status, DoctorStatus::Error);
        assert_eq!(check.code, "provider_connectivity");
    }

    #[test]
    fn ok_probe_is_ok() {
        let check = provider_connectivity_check(&ProviderConnectivityFacts {
            credential_present: true,
            probe: ProviderConnectivityProbe::Ok,
            detail: None,
        });
        assert_eq!(check.status, DoctorStatus::Ok);
        assert_eq!(check.code, "provider_connectivity");
    }

    #[test]
    fn timeout_probe_is_warn() {
        let check = provider_connectivity_check(&ProviderConnectivityFacts {
            credential_present: true,
            probe: ProviderConnectivityProbe::Timeout,
            detail: None,
        });
        assert_eq!(check.status, DoctorStatus::Warn);
        assert_eq!(check.code, "provider_connectivity");
    }
}
