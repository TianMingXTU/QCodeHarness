import sys
import unittest

sys.path.insert(0, "src")

from duration import format_clock, humanize


class TestHumanize(unittest.TestCase):
    def test_plain_seconds(self):
        self.assertEqual(humanize(5.0), "5s")

    def test_minutes_with_remainder(self):
        self.assertEqual(humanize(65.5), "1m 5.5s")


class TestFormatClock(unittest.TestCase):
    # 这些测试引用缺失的 format_clock：实现之前全部失败。
    def test_zero(self):
        self.assertEqual(format_clock(0), "00:00:00")

    def test_minutes_and_seconds(self):
        self.assertEqual(format_clock(3661), "01:01:01")

    def test_hours_carry_beyond_midnight(self):
        self.assertEqual(format_clock(90061), "25:01:01")


if __name__ == "__main__":
    unittest.main()
