import sys
import unittest

sys.path.insert(0, "src")

from date_parser import is_leap_year, parse_iso_date


class TestParseIsoDate(unittest.TestCase):
    def test_single_digit_day(self):
        # 破损 fixture 上这条会失败：day 段 '05' 只读到 '0'，解析成 0。
        self.assertEqual(parse_iso_date("2026-01-05"), (2026, 1, 5))

    def test_two_digit_day(self):
        self.assertEqual(parse_iso_date("2026-12-31"), (2026, 12, 31))

    def test_january_first(self):
        self.assertEqual(parse_iso_date("2000-01-01"), (2000, 1, 1))


class TestLeapYear(unittest.TestCase):
    def test_common_rules(self):
        self.assertTrue(is_leap_year(2000))
        self.assertFalse(is_leap_year(1900))
        self.assertTrue(is_leap_year(2024))
        self.assertFalse(is_leap_year(2026))


if __name__ == "__main__":
    unittest.main()
