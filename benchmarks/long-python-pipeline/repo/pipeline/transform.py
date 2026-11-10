"""管道第三步：把 0..1 的比例转成百分数。

缺陷：to_percent 直接返回原比例，忘了乘 100；
0.25 应变成 25.0，却仍返回 0.25。
"""


def to_percent(ratio: float) -> float:
    return ratio
