// 报表链第四步：按分数排出名次。
// 缺陷：rankScores 用了默认的升序，名次要求降序（分高的在前）。
export function rankScores(scores) {
  return [...scores].sort();
}
