//! 运行平台：当前进程跑在哪个系统上，各系统的默认处理方式不同。
//!
//! 平台事实由编译目标决定，不是运行时状态。编译出的二进制只运行在它被
//! 编译时的目标平台上，`cfg!` 的取值就是当前运行的系统，运行时探测
//! （读 uname、查注册表）没有第二种可能的结果。
//!
//! 平台差异集中在本模块：调用方按本模块给出的能力分支，不在各自的业务
//! 逻辑里散落平台判断。

/// 当前运行的平台。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows。
    Windows,
    /// Linux。
    Linux,
    /// macOS。
    MacOs,
    /// 其他系统（BSD 等）。
    Other,
}

impl Platform {
    /// 当前运行的平台。
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    /// 平台的人读名称。写进证据与模型可见文本。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
            Self::Other => "其他系统",
        }
    }

    /// 跑一条命令用的 shell：程序名与它的固定参数。
    ///
    /// Windows 用 `cmd /C`，其余平台用 POSIX `sh -c`。macOS 自带的 `sh`
    /// 是 POSIX shell，与用户登录时用的 `zsh` 无关，脚本行为一致。
    pub fn shell(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Windows => ("cmd", &["/C"]),
            Self::Linux | Self::MacOs | Self::Other => ("sh", &["-c"]),
        }
    }

    /// 文件系统路径是否默认不区分大小写。
    ///
    /// Windows 与 macOS 的默认文件系统（NTFS、APFS）比较路径时不区分
    /// 大小写，`/Users/foo` 与 `/users/foo` 指向同一位置。Linux 的 ext4
    /// 等区分大小写。
    ///
    /// 判断路径是否落在某个根下时要按这个口径归一化，否则 macOS 上
    /// `cat /users/me/repo/x.txt` 会被当成越界路径误拒。
    pub fn has_case_insensitive_paths(&self) -> bool {
        matches!(self, Self::Windows | Self::MacOs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 检测到的平台必须与编译目标一致。
    ///
    /// 平台由编译目标决定，测试跑在当前机器上，因此当前机器的目标平台
    /// 就是要断言的值。判据用 `cfg!` 独立写一遍，不复用 `Platform::current`，
    /// 否则实现写错时测试会跟着错。
    #[test]
    fn the_detected_platform_matches_the_compilation_target() {
        let expected = if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Other
        };
        assert_eq!(Platform::current(), expected);
    }

    /// shell 的选择按平台固定，且参数顺序正确。
    ///
    /// `cmd /C` 与 `sh -c` 的参数位置不同，写反了命令会执行成空串。
    ///
    /// 断言的是当前平台的映射，不是别的平台：非 Windows 分支若断言
    /// `Platform::Linux`，在 macOS 上测的是 Linux 的映射，当前平台自己的
    /// 映射反而没被断言到。
    #[test]
    fn each_platform_gets_its_own_shell() {
        let platform = Platform::current();
        let (expected_program, expected_args) = match platform {
            Platform::Windows => ("cmd", &["/C"][..]),
            Platform::Linux | Platform::MacOs | Platform::Other => ("sh", &["-c"][..]),
        };
        assert_eq!(platform.shell(), (expected_program, expected_args));
    }

    /// 平台标签互不相同，且非空。
    ///
    /// 标签写进证据，模型据此判断当前系统的处理方式。
    #[test]
    fn platform_labels_are_distinct() {
        let labels = [
            Platform::Windows.label(),
            Platform::Linux.label(),
            Platform::MacOs.label(),
            Platform::Other.label(),
        ];
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "平台标签必须能区分平台");
        for label in labels {
            assert!(!label.is_empty(), "平台标签不能为空");
        }
    }
}
