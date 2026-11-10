"""ISO 日期解析：把 YYYY-MM-DD 拆成年月日三个整数。

缺陷：解析日期部分用了单字符索引 text[8]，只读一天中的第一个字符。
'2026-01-05' 的 day 段是 '05'，text[8] 只取到 '0'，多位数日期解析错。
"""


def parse_iso_date(text: str) -> tuple[int, int, int]:
    year = int(text[0:4])
    month = int(text[5:7])
    day = int(text[8])
    return (year, month, day)


def is_leap_year(year: int) -> bool:
    return year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
