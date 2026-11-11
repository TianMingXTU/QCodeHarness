// 去重工具：按 key 保留首次出现的元素。
// 缺陷：当前实现用 Map 覆盖写入，保成了"最后一次"，先出现的被挤掉。
export function dedupeByKey(items, keyFn) {
  const last = new Map();
  for (const item of items) {
    last.set(keyFn(item), item);
  }
  return [...last.values()];
}

export function firstBy(items, keyFn) {
  if (items.length === 0) return undefined;
  let best = items[0];
  for (const item of items) {
    if (keyFn(item) < keyFn(best)) best = item;
  }
  return best;
}
