//! 验收：guard 的判定必须与 reference（固定 revision 的口径）一致。

use bench_ident_guard::guard;
use bench_ident_guard::reference;

/// 同一组输入上，guard 与 reference 的合法/非法判定必须完全一致。
fn must_agree(kind: &str, id: &str, max_bytes: usize) {
    let got = guard::is_safe_dir_name(kind, id, max_bytes).is_ok();
    let want = reference::validate_safe_identifier(kind, id, max_bytes).is_ok();
    assert_eq!(got, want, "`{id}` 的判定不一致：guard={got} reference={want}");
}

#[test]
fn regular_names_agree() {
    must_agree("dir", "task-a", 32);
    must_agree("dir", "9", 32);
    must_agree("dir", "x_y-z", 32);
}

#[test]
fn reserved_device_names_are_rejected_like_reference() {
    // 这组是缺陷点：guard 漏掉设备名拦截，"nul"、"com4" 会被放行，
    // 与 reference 的拒绝不一致，基线上这些断言失败。
    must_agree("dir", "nul", 32);
    must_agree("dir", "con", 32);
    must_agree("dir", "com4", 32);
    must_agree("dir", "lpt1", 32);
}

#[test]
fn out_of_class_names_agree() {
    must_agree("dir", "", 32);
    must_agree("dir", "Task", 32);
    must_agree("dir", "x".repeat(33).as_str(), 32);
}
