"""管道四环节各自的失败测试。每个测试只盯一个模块，互不牵连。"""
import sys
import unittest

sys.path.insert(0, ".")

from pipeline import clean, load, output, transform  # noqa: E402


class TestLoad(unittest.TestCase):
    def test_last_line_is_kept(self):
        self.assertEqual(load.load_lines("a\nb\nc"), ["a", "b", "c"])


class TestClean(unittest.TestCase):
    def test_both_sides_are_stripped(self):
        self.assertEqual(clean.clean("  x  "), "x")


class TestTransform(unittest.TestCase):
    def test_ratio_becomes_percent(self):
        self.assertEqual(transform.to_percent(0.25), 25.0)


class TestOutput(unittest.TestCase):
    def test_rank_is_descending(self):
        self.assertEqual(output.rank([3, 1, 2]), [3, 2, 1])


if __name__ == "__main__":
    unittest.main()
