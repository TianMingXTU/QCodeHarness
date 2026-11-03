//! 渲染：把聚合结果画成文本。

use crate::aggregate::GroupSummary;
use crate::TITLE;

/// 渲染整份报表：标题一行，每组一行。
pub fn render(summaries: &[GroupSummary]) -> String {
    // 缺陷：标题后忘了换行，第一组数据粘在标题上。
    let mut text = String::new();
    text.push_str(TITLE);
    for summary in summaries {
        text.push_str(&format!(
            "{} ×{} 平均 {:.1} 峰值 {:.1}\n",
            summary.source, summary.count, summary.average, summary.max
        ));
    }
    text
}
