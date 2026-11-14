// ACP 会话最小 e2e：在内存里搭一个说 versioned stdio 协议的假服务，让 TypeScript
// 客户端完成"创建 → 审批 → 完成"与"创建 → 拒绝 → 停止"两条路径。
//
// 编译后用 node 直接运行：node dist-test/test/e2e.js。所有断言失败都会以非零
// 退出码结束，因此 npm test 的结果可以直接作为验收证据。

import { AcpSession, PendingApproval, ServiceEventPayload, TaskView } from "../src/index.js";
import { Transport } from "../src/transport.js";

/** 简单异步队列：跨发送/接收两端传递字符串行。 */
class AsyncQueue {
  private items: string[] = [];
  private waiters: Array<(value: string) => void> = [];
  private closed = false;

  push(value: string): void {
    const waiter = this.waiters.shift();
    if (waiter) waiter(value);
    else this.items.push(value);
  }

  next(): Promise<string> {
    const item = this.items.shift();
    if (item !== undefined) return Promise.resolve(item);
    if (this.closed) return Promise.reject(new Error("queue closed"));
    return new Promise((resolve, reject) => {
      this.waiters.push(resolve);
      void reject;
    });
  }

  close(): void {
    this.closed = true;
  }
}

class MemoryTransport implements Transport {
  constructor(
    private readonly toServer: AsyncQueue,
    private readonly toClient: AsyncQueue,
  ) {}

  writeLine(line: string): void {
    this.toServer.push(line);
  }

  readLine(): Promise<string> {
    return this.toClient.next();
  }

  close(): void {
    this.toClient.close();
  }
}

function taskView(): TaskView {
  return {
    task_id: "task-1",
    session_id: "session-1",
    execution_id: "exec-1",
    attempt: 1,
    status: "running",
    answer: null,
    error: null,
  };
}

function approvalWaiting(): ServiceEventPayload {
  const request: PendingApproval = {
    approval_id: "ap1",
    task_id: "task-1",
    session_id: "session-1",
    execution_id: "exec-1",
    turn: 1,
    call: { id: "call-1", name: "write_file", arguments: {} },
    reason: null,
  };
  return { type: "approval_waiting", request };
}

/** 内存假服务：按 create → 审批等待 → decide 的顺序应答。 */
class FakeServer {
  constructor(
    private readonly toServer: AsyncQueue,
    private readonly toClient: AsyncQueue,
  ) {}

  start(): void {
    this.toClient.push(JSON.stringify({ type: "hello", version: 1 }));
    void this.loop();
  }

  private async loop(): Promise<void> {
    for (;;) {
      let line: string;
      try {
        line = await this.toServer.next();
      } catch {
        return;
      }
      const message = JSON.parse(line) as {
        id: string;
        command: { op: string; allow?: boolean };
      };
      const command = message.command;
      if (command.op === "create") {
        this.toClient.push(
          JSON.stringify({
            type: "reply",
            version: 1,
            id: message.id,
            result: { result: "task", task: taskView() },
          }),
        );
        this.toClient.push(
          JSON.stringify({ type: "event", version: 1, event: fullEvent(approvalWaiting()) }),
        );
      } else if (command.op === "decide") {
        this.toClient.push(
          JSON.stringify({
            type: "reply",
            version: 1,
            id: message.id,
            result: { result: "accepted", task_id: "task-1", execution_id: "exec-1" },
          }),
        );
        const payload: ServiceEventPayload =
          command.allow === true
            ? { type: "completed", answer: "已修复" }
            : { type: "stopped", status: "cancelled", error: null };
        this.toClient.push(JSON.stringify({ type: "event", version: 1, event: fullEvent(payload) }));
      } else if (command.op === "shutdown") {
        this.toClient.push(
          JSON.stringify({
            type: "reply",
            version: 1,
            id: message.id,
            result: { result: "shutdown" },
          }),
        );
        return;
      }
    }
  }
}

// 把事件载荷包成完整 ServiceEvent；测试假服务不需要真实时间戳/序号。
function fullEvent(payload: ServiceEventPayload) {
  return {
    version: 1,
    sequence: 0,
    time_unix_ms: 0,
    task_id: "task-1",
    session_id: "session-1",
    execution_id: "exec-1",
    payload,
  };
}

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(`断言失败: ${message}`);
}

async function runApprovePath(): Promise<void> {
  const toServer = new AsyncQueue();
  const toClient = new AsyncQueue();
  new FakeServer(toServer, toClient).start();
  const session = new AcpSession(new MemoryTransport(toServer, toClient));

  const first = await session.runToCompletion("评审这次改动");
  assert(first.kind === "needs_approval", `应停在审批，实际 ${first.kind}`);
  if (first.kind === "needs_approval") {
    assert(first.prompt.request.approval_id === "ap1", "审批 ID 应为 ap1");
    assert(first.prompt.request.call.name === "write_file", "应携带工具名");
  }

  const outcome = await session.decide("ap1", "task-1", "exec-1", true);
  assert(outcome.kind === "completed", `审批通过后应完成，实际 ${outcome.kind}`);
  if (outcome.kind === "completed") {
    assert(outcome.answer === "已修复", `答案应为"已修复"，实际 ${outcome.answer}`);
  }
}

async function runRejectPath(): Promise<void> {
  const toServer = new AsyncQueue();
  const toClient = new AsyncQueue();
  new FakeServer(toServer, toClient).start();
  const session = new AcpSession(new MemoryTransport(toServer, toClient));

  const first = await session.runToCompletion("评审这次改动");
  assert(first.kind === "needs_approval", `应停在审批，实际 ${first.kind}`);

  const outcome = await session.decide("ap1", "task-1", "exec-1", false);
  assert(outcome.kind === "stopped", `拒绝后应停止，实际 ${outcome.kind}`);
  if (outcome.kind === "stopped") {
    assert(outcome.status === "cancelled", `状态应为 cancelled，实际 ${outcome.status}`);
  }
}

async function main(): Promise<void> {
  await runApprovePath();
  await runRejectPath();
  console.log("ACP TypeScript SDK e2e: 2/2 通过");
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
});
