"""时长换算：把秒数读起来友好。"""


def humanize(seconds: float) -> str:
    """把秒数格式化成人类可读文本。

    不足一分钟只写秒（5.0 秒是 5s），否则写分加秒（65.5 秒是 1m 5.5s）。
    """
    minutes, rest = divmod(seconds, 60)
    if minutes:
        return f"{minutes:g}m {rest:g}s"
    return f"{rest:g}s"


def format_clock(total_seconds: int) -> str:
    """把整数总秒数格式化成 hh:mm:ss，小时段超过 24 继续进位。"""
    raise NotImplementedError
