"""fetch_config 的测试：起一个环回 HTTP 服务，拉取后核对形状。"""
import sys
import unittest

sys.path.insert(0, "src")

from json_poller import fetch_config, start_server  # noqa: E402


class TestFetchConfig(unittest.TestCase):
    def setUp(self):
        self.server, self.base_url = start_server()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def test_returns_items_list(self):
        # 破损 fixture 上这条会失败：fetch_config 返回整个 dict 而不是
        # 取 items 键，调用方拿到 dict 而非 list。
        items = fetch_config(self.base_url)
        self.assertIsInstance(items, list)
        self.assertEqual(items, [{"id": 1}, {"id": 2}])

    def test_item_shape(self):
        items = fetch_config(self.base_url)
        self.assertEqual(items[0]["id"], 1)


if __name__ == "__main__":
    unittest.main()
