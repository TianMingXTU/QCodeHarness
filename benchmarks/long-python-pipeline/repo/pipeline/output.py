"""管道第四步：按分数排出名次。

缺陷：rank 用默认的升序排列，但名次要求降序（分高的在前）；
[3, 1, 2] 应得到 [3, 2, 1]，却返回 [1, 2, 3]。
"""


def rank(scores: list[float]) -> list[float]:
    return sorted(scores)
