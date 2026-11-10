"""管道第二步：清洗字段。

缺陷：clean 只去了左边的空白（lstrip），右边空白原样保留；
测试要求字段两侧空白都去掉（strip）。
"""


def clean(field: str) -> str:
    return field.lstrip()
