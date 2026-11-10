"""管道第一步：把原始文本按行加载成列表。

缺陷：load_lines 用 lines[:-1] 丢掉了最后一条行，输入 'a\nb\nc'
只得到 ['a', 'b']，最后一行 'c' 消失。
"""


def load_lines(text: str) -> list[str]:
    lines = text.splitlines()
    return lines[:-1]
