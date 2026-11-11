// 报表链第二步：清洗字段。
// 缺陷：clean 只去了左边空白，右边空白原样保留；
// 正确行为是两侧都去掉。
export function clean(field) {
  return field.trimStart();
}
