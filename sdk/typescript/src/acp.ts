// ACP 会话客户端：把 versioned stdio 帧收发封装成"创建任务 → 等待审批 → 决定 →
// 完成"的会话方法，供 IDE / CI 这类外部宿主复用同一套线格式接入。
//
// 会话层不决定模型或审批的最终结论：审批结论由调用方传入的 allow 传回服务，
// 证据与审批链仍落在 QCodeHarness 服务侧。本文件是纯 TypeScript，不依赖任何
// 运行时 IO 库——传输由调用方注入。

import {
  PROTOCOL_VERSION,
  ServiceCommand,
  ServiceError,
  ServiceEvent,
  ServiceEventPayload,
  ServiceReply,
  StdioResponse,
} from "./protocol.js";
import { Transport } from "./transport.js";

export interface AcpApprovalPrompt {
  request: {
    approval_id: string;
    task_id: string;
    execution_id: string;
    call: { id: string; name: string };
  };
}

export type AcpRunOutcome =
  | { kind: "completed"; answer: string }
  | { kind: "stopped"; status: string; error: string | null }
  | { kind: "needs_approval"; prompt: AcpApprovalPrompt }
  | { kind: "error"; message: string };

export class AcpSession {
  private nextId = 0;
  private pending = new Map<
    string,
    { resolve: (reply: ServiceReply) => void; reject: (err: ServiceError) => void }
  >();
  private eventWaiters: Array<(event: ServiceEvent) => void> = [];
  private eventQueue: ServiceEvent[] = [];
  private failure: ServiceError | null = null;
  private closed = false;
  // 逐行读取的缓冲：跨多行累积直到凑齐完整 JSON。
  private buffer = "";
  private readLoopStarted = false;

  constructor(private transport: Transport) {}

  async start(): Promise<void> {
    if (this.readLoopStarted) return;
    this.readLoopStarted = true;
    this.startReadLoop();
  }

  private startReadLoop(): void {
    const loop = async () => {
      for (;;) {
        if (this.closed) return;
        let line: string;
        try {
          line = await this.transport.readLine();
        } catch {
          this.failAll({ code: "transport_closed", message: "传输已关闭" });
          return;
        }
        this.handleLine(line);
      }
    };
    void loop();
  }

  private handleLine(raw: string): void {
    const line = raw.trim();
    if (line.length === 0) return;
    let frame: StdioResponse;
    try {
      frame = JSON.parse(line) as StdioResponse;
    } catch {
      return; // 非 JSON 行直接忽略，与 Rust 侧宽松解析一致。
    }
    if (frame.version !== PROTOCOL_VERSION) {
      this.failAll({ code: "unsupported_version", message: `不支持协议版本 ${frame.version}` });
      return;
    }
    switch (frame.type) {
      case "hello":
        return;
      case "reply": {
        const waiter = this.pending.get(frame.id);
        if (waiter) {
          this.pending.delete(frame.id);
          waiter.resolve(frame.result);
        }
        return;
      }
      case "error": {
        const waiter = this.pending.get(frame.id);
        if (waiter) {
          this.pending.delete(frame.id);
          waiter.reject(frame.error);
        } else {
          this.failAll(frame.error);
        }
        return;
      }
      case "event": {
        // 一条事件只交付一次：有等待者就直接交付，否则入队缓存。
        // 若既入队又交付，等待者消费后事件仍留在队列里，会被重复消费。
        const waiter = this.eventWaiters.shift();
        if (waiter) waiter(frame.event);
        else this.eventQueue.push(frame.event);
        return;
      }
      default:
        return;
    }
  }

  private failAll(error: ServiceError): void {
    this.failure = error;
    for (const waiter of this.pending.values()) waiter.reject(error);
    this.pending.clear();
    while (this.eventWaiters.length > 0) {
      const waiter = this.eventWaiters.shift()!;
      waiter({
        version: PROTOCOL_VERSION,
        sequence: 0,
        time_unix_ms: 0,
        task_id: null,
        session_id: null,
        execution_id: null,
        payload: { type: "stopped", status: "failed", error: error.message },
      });
    }
  }

  private call(command: ServiceCommand): Promise<ServiceReply> {
    if (this.failure) return Promise.reject(this.failure);
    const id = `c${this.nextId++}`;
    const request = { version: PROTOCOL_VERSION, id, command };
    return new Promise<ServiceReply>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.transport.writeLine(JSON.stringify(request));
    });
  }

  private nextEvent(): Promise<ServiceEvent> {
    if (this.eventQueue.length > 0) {
      return Promise.resolve(this.eventQueue.shift()!);
    }
    if (this.failure) return Promise.reject(this.failure);
    return new Promise<ServiceEvent>((resolve) => {
      this.eventWaiters.push(resolve);
    });
  }

  private static payloadOf(event: ServiceEvent): ServiceEventPayload {
    return event.payload;
  }

  /** 启动一个任务并驱动到完成、停止或首次需要审批。 */
  async runToCompletion(goal: string): Promise<AcpRunOutcome> {
    await this.start();
    let reply: ServiceReply;
    try {
      reply = await this.call({ op: "create", goal, kind: "review" });
    } catch (err) {
      return { kind: "error", message: (err as ServiceError).message };
    }
    if (reply.result !== "task" && reply.result !== "accepted") {
      return { kind: "error", message: `create 未返回任务视图，得到 ${reply.result}` };
    }
    return this.drive();
  }

  /** 决定一条待审批后继续驱动。 */
  async decide(
    approvalId: string,
    taskId: string,
    executionId: string,
    allow: boolean,
  ): Promise<AcpRunOutcome> {
    try {
      await this.call({ op: "decide", approval_id: approvalId, task_id: taskId, execution_id: executionId, allow });
    } catch (err) {
      return { kind: "error", message: (err as ServiceError).message };
    }
    return this.drive();
  }

  private async drive(): Promise<AcpRunOutcome> {
    for (;;) {
      let event: ServiceEvent;
      try {
        event = await this.nextEvent();
      } catch (err) {
        return { kind: "error", message: (err as ServiceError).message };
      }
      const payload = AcpSession.payloadOf(event);
      switch (payload.type) {
        case "completed":
          return { kind: "completed", answer: payload.answer };
        case "stopped":
          return { kind: "stopped", status: payload.status, error: payload.error };
        case "approval_waiting":
          return {
            kind: "needs_approval",
            prompt: {
              request: {
                approval_id: payload.request.approval_id,
                task_id: payload.request.task_id,
                execution_id: payload.request.execution_id,
                call: { id: payload.request.call.id, name: payload.request.call.name },
              },
            },
          };
        default:
          continue;
      }
    }
  }
}
