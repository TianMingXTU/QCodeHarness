// versioned stdio 协议（PROTOCOL_VERSION = 1）的 TypeScript 映射。
//
// 线格式与 Rust 侧 src/interface/stdio.rs 完全一致：每行一个 JSON 帧；响应按
// type 标签；命令按 op 标签、回复按 result 标签、事件按 type 标签。这里只描述
// 客户端需要的子集，作用范围限定 ACP 会话，不做 IDE 自研插件。

export const PROTOCOL_VERSION = 1;

export type TaskKind = "review" | "fix" | "feature" | "question";

export interface CreateCommand {
  op: "create";
  goal: string;
  kind: TaskKind;
}
export interface ListCommand {
  op: "list";
}
export interface GetCommand {
  op: "get";
  task_id: string;
}
export interface InputCommand {
  op: "input";
  task_id: string;
  message_id: string;
  text: string;
}
export interface DecideCommand {
  op: "decide";
  approval_id: string;
  task_id: string;
  execution_id: string;
  allow: boolean;
}
export interface CancelCommand {
  op: "cancel";
  task_id: string;
}
export interface EventsCommand {
  op: "events";
  after: number;
  limit: number;
}
export interface ShutdownCommand {
  op: "shutdown";
}
export type ServiceCommand =
  | CreateCommand
  | ListCommand
  | GetCommand
  | InputCommand
  | DecideCommand
  | CancelCommand
  | EventsCommand
  | ShutdownCommand;

export interface ServiceError {
  code: string;
  message: string;
}

export type TaskStatus =
  | "running"
  | "stopping"
  | "cancelled"
  | "budget_exhausted"
  | "failed"
  | "completed"
  | "interrupted"
  | "merged"
  | "cleaned";

export interface ToolCall {
  id: string;
  name: string;
  arguments: unknown;
}

export interface PendingApproval {
  approval_id: string;
  task_id: string;
  session_id: string;
  execution_id: string;
  turn: number;
  call: ToolCall;
  reason: string | null;
}

export interface TaskView {
  task_id: string;
  session_id: string;
  execution_id: string;
  attempt: number;
  status: TaskStatus;
  answer: string | null;
  error: string | null;
}

interface TaskReply {
  result: "task";
  task: TaskView;
}
interface TasksReply {
  result: "tasks";
  tasks: TaskView[];
}
interface AcceptedReply {
  result: "accepted";
  task_id: string;
  execution_id: string | null;
}
interface PendingApprovalsReply {
  result: "pending_approvals";
  requests: PendingApproval[];
}
interface EventsReply {
  result: "events";
  events: ServiceEvent[];
  next: number;
}
interface ShutdownReply {
  result: "shutdown";
}
export type ServiceReply =
  | TaskReply
  | TasksReply
  | AcceptedReply
  | PendingApprovalsReply
  | EventsReply
  | ShutdownReply;

export interface ApprovalWaitingPayload {
  type: "approval_waiting";
  request: PendingApproval;
}
export interface CompletedPayload {
  type: "completed";
  answer: string;
}
export interface StoppedPayload {
  type: "stopped";
  status: TaskStatus;
  error: string | null;
}
export type ServiceEventPayload = ApprovalWaitingPayload | CompletedPayload | StoppedPayload;

export interface ServiceEvent {
  version: number;
  sequence: number;
  time_unix_ms: number;
  task_id: string | null;
  session_id: string | null;
  execution_id: string | null;
  payload: ServiceEventPayload;
}

export interface HelloResponse {
  type: "hello";
  version: number;
}
export interface ReplyResponse {
  type: "reply";
  version: number;
  id: string;
  result: ServiceReply;
}
export interface EventResponse {
  type: "event";
  version: number;
  event: ServiceEvent;
}
export interface ErrorResponse {
  type: "error";
  version: number;
  id: string;
  error: ServiceError;
}
export interface SubscribedResponse {
  type: "subscribed";
  version: number;
  id: string;
  after: number;
}
export type StdioResponse =
  | HelloResponse
  | ReplyResponse
  | EventResponse
  | ErrorResponse
  | SubscribedResponse;
