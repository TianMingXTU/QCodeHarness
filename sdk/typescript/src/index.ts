// 包入口：导出 ACP 会话客户端、传输抽象与协议类型。
//
// 本 SDK 只消费 QCodeHarness 的 versioned stdio 协议（Rust SDK 保留为内核路径），
// 作用范围限定在 ACP 会话，不暴露 IDE 自研插件或云调度能力。

export { AcpSession } from "./acp.js";
export type { AcpApprovalPrompt, AcpRunOutcome } from "./acp.js";
export type { Transport } from "./transport.js";
export * from "./protocol.js";
