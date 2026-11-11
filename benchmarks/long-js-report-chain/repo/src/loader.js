// 报表链第一步：把原始文本按行加载成列表。
// 缺陷：loadLines 用 slice(0, -1) 丢掉了最后一行。
export function loadLines(text) {
  const lines = text.split('\n').filter((line) => line.length > 0);
  return lines.slice(0, -1);
}
