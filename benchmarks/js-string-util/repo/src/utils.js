// 字符串工具：把首字母转成大写。
// 缺陷：未处理空输入。空串时 text[0] 是 undefined，toUpperCase 抛 TypeError。
export function capitalize(text) {
  return text[0].toUpperCase() + text.slice(1);
}

export function trimAll(text) {
  return text.trim();
}
