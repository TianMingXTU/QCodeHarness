"""从 127.0.0.1 上的临时 HTTP 服务拉取 JSON 配置。

只用标准库：服务用 ``http.server`` 起在环回地址，客户端用
``urllib.request`` 发请求，全程不碰外部网络。
"""
import json
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class _ConfigHandler(BaseHTTPRequestHandler):
    """把固定配置挂在 /config 上，让测试完全离线可复现。"""

    def do_GET(self):  # noqa: N802 - 基类约定方法名
        body = json.dumps({"items": [{"id": 1}, {"id": 2}]}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        # 静默 HTTP 访问日志，测试输出干净。
        pass


def start_server():
    """在随机端口起一个环回服务，返回 (server, base_url)。

    端口填 0 让系统分配空闲端口，避免固定端口撞车；用
    ``server_address[1]`` 读回实际端口。
    """
    server = ThreadingHTTPServer(("127.0.0.1", 0), _ConfigHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, "http://127.0.0.1:%d" % server.server_address[1]


def fetch_config(base_url):
    """拉取 /config 并解析，返回 items 列表。

    缺陷：把整个 JSON 字典返回了，没取 items 键，调用方拿到的是 dict。
    """
    with urllib.request.urlopen(base_url + "/config") as response:
        payload = json.loads(response.read())
    return payload
