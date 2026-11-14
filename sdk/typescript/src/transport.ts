// 双向文本传输抽象：服务端把响应逐行写回，客户端逐行读取。
//
// ACP 在 versioned stdio 之上复用同一套 NDJSON 帧，因此传输只关心"逐行收发
// UTF-8 文本"。真实部署里这对接子进程的 stdin/stdout；测试里用内存队列代替。

export interface Transport {
  writeLine(line: string): void;
  readLine(): Promise<string>;
  close(): void;
}
