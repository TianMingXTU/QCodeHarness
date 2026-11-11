//! 常驻服务的文本与全屏终端前端，共用命令语法和逐次审批边界。

use std::collections::{BTreeMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::style::Print;
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::application::service::{EventSubscription, TaskService};
use crate::domain::evidence::AuditEvent;
use crate::domain::service::{
    PROTOCOL_VERSION, PendingApproval, ServiceCommand, ServiceError, ServiceEvent,
    ServiceEventPayload, ServiceReply, TaskStatus, TaskView,
};
use crate::domain::task::TaskKind;
use crate::domain::tool::ToolCall;

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_PENDING_COMMANDS: usize = 64;
const HISTORY_LINES: usize = 500;
const PREVIEW_CHARS: usize = 4096;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const EXIT_DRAIN: Duration = Duration::from_secs(2);
const EXIT_WARNING: &str = "未完成命令停止等待；命令可能已生效，请在下次启动后查询任务状态。";

const HELP: &str = "命令（普通文字：已选任务时追加输入，否则创建任务）：
/create [--kind bug-fix|feature|review|refactor] <目标>
/list                         列出任务
/get [任务]                   查看任务（缺省为已选任务）
/select <任务|none>           切换任务或取消选择
/input <任务> <文字>          向指定任务追加输入
/approvals [任务|all]         查看当前待审批项（缺省为全部）
/approve <审批ID> [任务 执行]  仅批准这一次审批；省略归属时查询实时待审批列表
/deny <审批ID> [任务 执行]     仅拒绝这一次审批
/cancel [任务]                取消执行（缺省为已选任务）
/continue [任务]              继续任务并创建新执行
/fork [任务] [新目标]         从任务分叉（不填任务时使用已选任务）
/events [游标] [条数]         只读回放，缺省 0 / 100，上限 1000
/merge <任务>                 显式合并工作区
/cleanup <任务>               显式删除任务工作区
/help /quit                   帮助 / 关闭服务并退出
以 // 开头可输入字面 /。Ctrl-C 取消已选的运行任务，再按一次退出。
TUI：Tab / Shift-Tab 切换任务，PageUp / PageDown 滚动，左右/Home/End 编辑。";

/// 解析后的终端动作。选择和帮助由前端处理，业务命令交给同一个服务端口。
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalCommand {
    Empty,
    Help,
    Quit,
    Select(Option<String>),
    Input {
        task_id: String,
        text: String,
    },
    Decide {
        approval_id: String,
        owner: Option<(String, String)>,
        allow: bool,
    },
    Service(ServiceCommand),
}

/// 解析一行输入，不访问服务或修改选择。`selected` 只用于补全省略的任务标识。
///
/// 任务描述与补充输入保留原始空白；命令参数以空白分隔，不解释 shell 引号或转义。
pub fn parse_command(line: &str, selected: Option<&str>) -> Result<TerminalCommand, ServiceError> {
    if line.len() > MAX_INPUT_BYTES {
        return Err(ServiceError::new("input_too_large", "输入超过 64 KiB"));
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(TerminalCommand::Empty);
    }
    if !trimmed.starts_with('/') || trimmed.starts_with("//") {
        let text = if trimmed.starts_with("//") {
            trimmed[1..].to_owned()
        } else {
            line.to_owned()
        };
        return Ok(match selected {
            Some(task_id) => TerminalCommand::Input {
                task_id: task_id.into(),
                text,
            },
            None => TerminalCommand::Service(ServiceCommand::Create {
                goal: text,
                kind: TaskKind::BugFix,
            }),
        });
    }
    let (name, rest) = word(trimmed);
    let args: Vec<_> = rest.split_whitespace().collect();
    let service = match name {
        "/help" | "/?" if args.is_empty() => return Ok(TerminalCommand::Help),
        "/quit" | "/exit" if args.is_empty() => return Ok(TerminalCommand::Quit),
        "/create" | "/new" => {
            let (first, tail) = word(rest);
            let (kind, goal) = if first == "--kind" {
                let (kind, goal) = word(tail);
                (
                    TaskKind::parse_name(kind).ok_or_else(|| syntax("未知任务模板"))?,
                    goal,
                )
            } else {
                (TaskKind::BugFix, rest)
            };
            if goal.trim().is_empty() {
                return Err(syntax("/create 需要任务目标"));
            }
            ServiceCommand::Create {
                goal: goal.into(),
                kind,
            }
        }
        "/list" if args.is_empty() => ServiceCommand::List,
        "/get" => ServiceCommand::Get {
            task_id: target(&args, selected)?,
        },
        "/select" if args.len() == 1 => {
            return Ok(TerminalCommand::Select(
                (args[0] != "none").then(|| args[0].to_owned()),
            ));
        }
        "/input" => {
            let (task_id, text) = word(rest);
            if task_id.is_empty() || text.is_empty() {
                return Err(syntax("/input 需要任务与文字"));
            }
            return Ok(TerminalCommand::Input {
                task_id: task_id.into(),
                text: text.into(),
            });
        }
        "/approvals" if args.len() <= 1 => ServiceCommand::Approvals {
            task_id: args
                .first()
                .filter(|id| **id != "all")
                .map(|id| (*id).to_owned()),
        },
        "/approve" | "/deny" if args.len() == 1 || args.len() == 3 => {
            return Ok(TerminalCommand::Decide {
                approval_id: args[0].into(),
                owner: (args.len() == 3).then(|| (args[1].into(), args[2].into())),
                allow: name == "/approve",
            });
        }
        "/cancel" => ServiceCommand::Cancel {
            task_id: target(&args, selected)?,
        },
        "/continue" => ServiceCommand::Continue {
            task_id: target(&args, selected)?,
        },
        "/fork" => {
            let (id, goal) = word(rest);
            let task_id = if id.is_empty() {
                target(&[], selected)?
            } else {
                id.into()
            };
            ServiceCommand::Fork {
                task_id,
                goal: (!goal.is_empty()).then(|| goal.into()),
            }
        }
        "/merge" if args.len() == 1 => ServiceCommand::Merge {
            task_id: args[0].into(),
        },
        "/cleanup" if args.len() == 1 => ServiceCommand::Cleanup {
            task_id: args[0].into(),
        },
        "/events" if args.len() <= 2 => {
            let after = args
                .first()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|_| syntax("事件游标必须是非负整数"))?
                .unwrap_or(0);
            let limit = args
                .get(1)
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|_| syntax("事件条数必须是整数"))?
                .unwrap_or(100);
            if !(1..=1000).contains(&limit) {
                return Err(syntax("事件条数必须在 1 到 1000 之间"));
            }
            ServiceCommand::Events { after, limit }
        }
        _ => return Err(syntax("未知命令或参数不完整，请用 /help 查看用法")),
    };
    Ok(TerminalCommand::Service(service))
}

fn word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    (&text[..end], text[end..].trim_start())
}

fn target(args: &[&str], selected: Option<&str>) -> Result<String, ServiceError> {
    match args {
        [task_id] => Ok((*task_id).into()),
        [] => selected
            .map(str::to_owned)
            .ok_or_else(|| syntax("请先 /select 一个任务，或显式指定任务")),
        _ => Err(syntax("该命令只接受一个任务标识")),
    }
}

fn syntax(message: &str) -> ServiceError {
    ServiceError::new("invalid_command", message)
}
fn io_error(error: std::io::Error) -> ServiceError {
    ServiceError::new("terminal_io", error.to_string())
}

/// Unicode 字符编辑器。游标始终位于 UTF-8 边界，粘贴中的换行转换为空格。
#[derive(Debug, Default)]
pub struct InputEditor {
    text: String,
    cursor: usize,
}

impl InputEditor {
    /// 当前文字；不包含界面中的提示符或光标标记。
    pub fn text(&self) -> &str {
        &self.text
    }
    /// 游标的字节偏移，保证位于字符边界。
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    /// 插入文字，不执行粘贴内容中的命令；超出单行上限的部分不进入编辑器。
    pub fn insert(&mut self, text: &str) {
        for ch in text.chars() {
            let ch = match ch {
                '\r' | '\n' | '\t' => ' ',
                ch if ch.is_control() => continue,
                ch => ch,
            };
            if self.text.len() + ch.len_utf8() > MAX_INPUT_BYTES {
                break;
            }
            self.text.insert(self.cursor, ch);
            self.cursor += ch.len_utf8();
        }
    }
    /// 应用常见编辑键，未识别的键不改变输入。
    pub fn apply(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(ch) => self.insert(&ch.to_string()),
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Backspace => {
                let previous = self.previous();
                self.text.drain(previous..self.cursor);
                self.cursor = previous;
            }
            KeyCode::Delete => {
                let next = self.next();
                self.text.drain(self.cursor..next);
            }
            _ => {}
        }
    }
    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }
    fn next(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |ch| self.cursor + ch.len_utf8())
    }
    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

struct Outcome {
    reply: ServiceReply,
    select: bool,
    details: Option<String>,
}

struct Completion {
    urgent: bool,
    outcome: Result<Outcome, ServiceError>,
}

struct Session {
    service: Arc<dyn TaskService>,
    selected: Option<String>,
    tasks: BTreeMap<String, TaskView>,
    queue: VecDeque<String>,
    urgent_queue: VecDeque<TerminalCommand>,
    running: JoinSet<Completion>,
    busy: bool,
    urgent_busy: bool,
    closed: bool,
    deadline: Option<Instant>,
    interrupted: bool,
    prefix: String,
    serial: u64,
    output: Vec<String>,
}

impl Session {
    fn new(service: Arc<dyn TaskService>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            service,
            selected: None,
            tasks: BTreeMap::new(),
            queue: VecDeque::new(),
            urgent_queue: VecDeque::new(),
            running: JoinSet::new(),
            busy: false,
            urgent_busy: false,
            closed: false,
            deadline: None,
            interrupted: false,
            prefix: format!("terminal-{}-{nanos}", std::process::id()),
            serial: 0,
            output: Vec::new(),
        }
    }
    fn id(&mut self) -> String {
        self.serial += 1;
        format!("{}-{}", self.prefix, self.serial)
    }
    fn close(&mut self) {
        self.closed = true;
        self.deadline
            .get_or_insert_with(|| Instant::now() + EXIT_DRAIN);
    }
    fn idle(&self) -> bool {
        self.queue.is_empty() && self.urgent_queue.is_empty() && self.running.is_empty()
    }
    fn report_error(&mut self, error: &ServiceError) {
        self.output.push(format!("[错误] {error}"));
    }
    fn enqueue(&mut self, line: String) {
        if self.closed {
            return;
        }
        let (name, _) = word(line.trim());
        // 审批与取消不排在慢速合并后面；每条通道仍串行，保持输入顺序。
        if matches!(name, "/approve" | "/deny")
            || name == "/cancel"
                && (self.selected.is_some() || word(line.trim()).1.split_whitespace().count() == 1)
        {
            match parse_command(&line, self.selected.as_deref()) {
                Ok(command) if self.urgent_queue.len() < MAX_PENDING_COMMANDS => {
                    self.urgent_queue.push_back(command)
                }
                Ok(_) => self.report_error(&ServiceError::new(
                    "command_queue_full",
                    "审批/取消队列已满，请稍后重试",
                )),
                Err(error) => self.report_error(&error),
            }
        } else if matches!(
            parse_command(&line, self.selected.as_deref()),
            Ok(TerminalCommand::Quit)
        ) {
            self.close();
        } else if self.queue.len() < MAX_PENDING_COMMANDS {
            self.queue.push_back(line);
        } else {
            self.report_error(&ServiceError::new(
                "command_queue_full",
                "命令队列已满，请稍后重试",
            ));
        }
    }
    fn pump(&mut self) {
        if !self.urgent_busy
            && let Some(command) = self.urgent_queue.pop_front()
        {
            self.start(command, true);
        }
        while !self.busy {
            let Some(line) = self.queue.pop_front() else {
                break;
            };
            match parse_command(&line, self.selected.as_deref()) {
                Ok(TerminalCommand::Empty) => {}
                Ok(TerminalCommand::Help) => self.output.push(HELP.into()),
                Ok(TerminalCommand::Quit) => self.close(),
                Ok(TerminalCommand::Select(None)) => {
                    self.selected = None;
                    self.interrupted = false;
                    self.output
                        .push("[选择] 已取消任务选择，普通输入将创建新任务".into());
                }
                Ok(command) => self.start(command, false),
                Err(error) => self.report_error(&error),
            }
        }
    }
    fn start(&mut self, command: TerminalCommand, urgent: bool) {
        let service = self.service.clone();
        let id = self.id();
        if urgent {
            self.urgent_busy = true;
        } else {
            self.busy = true;
        }
        self.running.spawn(async move {
            Completion {
                urgent,
                outcome: execute_command(service, &id, command).await,
            }
        });
    }
    fn complete(
        &mut self,
        result: Result<Completion, tokio::task::JoinError>,
    ) -> Result<(), ServiceError> {
        let completion =
            result.map_err(|error| ServiceError::new("terminal_worker", error.to_string()))?;
        if completion.urgent {
            self.urgent_busy = false;
        } else {
            self.busy = false;
        }
        match completion.outcome {
            Ok(outcome) => {
                match &outcome.reply {
                    ServiceReply::Task { task } => {
                        self.tasks.insert(task.task_id.clone(), (**task).clone());
                        if outcome.select {
                            self.selected = Some(task.task_id.clone());
                            self.interrupted = false;
                            self.output.push(format!("[选择] {}", task.task_id));
                        }
                    }
                    ServiceReply::Tasks { tasks } => {
                        self.tasks = tasks
                            .iter()
                            .map(|task| (task.task_id.clone(), task.clone()))
                            .collect()
                    }
                    _ => {}
                }
                if let Some(details) = outcome.details {
                    self.output.push(details);
                }
                self.output.extend(render_reply(&outcome.reply));
            }
            Err(error) => self.report_error(&error),
        }
        Ok(())
    }
    fn observe(&mut self, event: &ServiceEvent) {
        if let ServiceEventPayload::TaskCreated { task } = &event.payload {
            self.tasks
                .entry(task.task_id.clone())
                .or_insert_with(|| task.clone());
        }
        if let Some(task) = event.task_id.as_ref().and_then(|id| self.tasks.get_mut(id))
            && event.execution_id.as_deref() == Some(task.execution_id.as_str())
        {
            let active = matches!(task.status, TaskStatus::Running | TaskStatus::Stopping);
            match &event.payload {
                ServiceEventPayload::CancelRequested if active => {
                    task.status = TaskStatus::Stopping
                }
                ServiceEventPayload::Stopped { status, error } if active => {
                    task.status = *status;
                    task.error = error.clone();
                }
                ServiceEventPayload::Completed { answer } if active => {
                    task.status = TaskStatus::Completed;
                    task.answer = Some(answer.clone());
                }
                ServiceEventPayload::WorkspaceMerged { verification }
                    if task.status != TaskStatus::Cleaned =>
                {
                    task.status = TaskStatus::Merged;
                    task.verification = verification.clone();
                }
                ServiceEventPayload::WorkspaceRemoved => task.status = TaskStatus::Cleaned,
                _ => {}
            }
        }
        self.output.push(render_event(event));
    }
    fn interrupt(&mut self) {
        let running = self
            .selected
            .as_ref()
            .and_then(|id| self.tasks.get(id))
            .is_some_and(|task| matches!(task.status, TaskStatus::Running | TaskStatus::Stopping));
        if running && !self.interrupted {
            self.interrupted = true;
            let task_id = self
                .selected
                .clone()
                .expect("running 只在已选任务存在时为 true");
            self.urgent_queue
                .push_back(TerminalCommand::Service(ServiceCommand::Cancel { task_id }));
            self.output
                .push("[取消] 已请求取消，再按 Ctrl-C 退出".into());
        } else {
            self.close();
        }
    }
    async fn shutdown(&mut self) -> Result<(), ServiceError> {
        self.running.abort_all();
        while self.running.join_next().await.is_some() {}
        let id = self.id();
        match self.service.call(&id, ServiceCommand::Shutdown).await? {
            ServiceReply::Shutdown => Ok(()),
            _ => Err(ServiceError::new("invalid_reply", "服务没有确认关闭")),
        }
    }
}

async fn execute_command(
    service: Arc<dyn TaskService>,
    id: &str,
    command: TerminalCommand,
) -> Result<Outcome, ServiceError> {
    let (command, select, details) = match command {
        TerminalCommand::Service(command) => {
            let select = matches!(
                command,
                ServiceCommand::Create { .. }
                    | ServiceCommand::Continue { .. }
                    | ServiceCommand::Fork { .. }
            );
            (command, select, None)
        }
        TerminalCommand::Input { task_id, text } => (
            ServiceCommand::Input {
                task_id,
                message_id: id.into(),
                text,
            },
            false,
            None,
        ),
        TerminalCommand::Select(Some(task_id)) => (ServiceCommand::Get { task_id }, true, None),
        TerminalCommand::Decide {
            approval_id,
            owner,
            allow,
        } => {
            // 回放事件只能用于展示，授权归属每次都从实时待审批列表获取。
            let pending_id = format!("{id}-pending");
            let ServiceReply::PendingApprovals { requests } = service
                .call(&pending_id, ServiceCommand::Approvals { task_id: None })
                .await?
            else {
                return Err(ServiceError::new("invalid_reply", "服务没有返回待审批列表"));
            };
            let mut matches = requests
                .iter()
                .filter(|request| request.approval_id == approval_id);
            let request = matches.next().ok_or_else(|| {
                ServiceError::new("approval_not_pending", "该审批当前不在待审批列表中")
            })?;
            if matches.next().is_some() {
                return Err(ServiceError::new(
                    "ambiguous_approval",
                    "审批标识不唯一，无法决定归属",
                ));
            }
            if let Some((task, execution)) = owner
                && (task != request.task_id || execution != request.execution_id)
            {
                return Err(ServiceError::new(
                    "approval_owner_mismatch",
                    "审批不属于给定任务和执行",
                ));
            }
            (
                ServiceCommand::Decide {
                    approval_id,
                    task_id: request.task_id.clone(),
                    execution_id: request.execution_id.clone(),
                    allow,
                },
                false,
                Some(render_approval(request)),
            )
        }
        _ => return Err(syntax("该动作不能发送给服务")),
    };
    let reply = service.call(id, command).await?;
    if select && !matches!(reply, ServiceReply::Task { .. }) {
        return Err(ServiceError::new(
            "invalid_reply",
            "服务没有返回可选择的任务",
        ));
    }
    Ok(Outcome {
        reply,
        select,
        details,
    })
}

struct LiveEvents {
    subscription: Box<dyn EventSubscription>,
    cursor: u64,
    retries: u8,
}
impl LiveEvents {
    fn new(service: &dyn TaskService) -> Result<Self, ServiceError> {
        Ok(Self {
            subscription: service.subscribe(0)?,
            cursor: 0,
            retries: 0,
        })
    }
    fn accept(
        &mut self,
        service: &dyn TaskService,
        result: Result<ServiceEvent, ServiceError>,
    ) -> Result<Option<ServiceEvent>, ServiceError> {
        match result {
            Err(error) if error.code == "lagged" && self.retries < 3 => {
                self.subscription = service.subscribe(self.cursor)?;
                self.retries += 1;
                Ok(None)
            }
            Err(error) => Err(error),
            Ok(event) => {
                if event.version != PROTOCOL_VERSION {
                    return Err(ServiceError::new(
                        "unsupported_version",
                        "无法显示未知版本的服务事件",
                    ));
                }
                if event.sequence <= self.cursor {
                    return Err(ServiceError::new("event_sequence", "服务事件没有按序递增"));
                }
                self.cursor = event.sequence;
                self.retries = 0;
                Ok(Some(event))
            }
        }
    }
}

/// 运行普通逐行文本界面，不切换终端模式；适用于管道与屏幕阅读器。
///
/// EOF 和 `/quit` 最多等待已读命令两秒，并排空原事件订阅中已就绪的结果，再关闭服务。
/// 读写、事件或关闭失败会返回错误；用户命令错误会显示并继续接受输入。
/// 不会留下独立的输入读取任务。
pub async fn run_text<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    service: Arc<dyn TaskService>,
    reader: R,
    mut writer: W,
) -> Result<(), ServiceError> {
    let mut session = Session::new(service);
    let result = text_loop(&mut session, reader, &mut writer).await;
    let shutdown = session.shutdown().await;
    if result.is_ok() && shutdown.is_ok() {
        write_text(&mut writer, "[退出] 服务已关闭").await?;
    }
    result.and(shutdown)
}

async fn text_loop<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session,
    reader: R,
    writer: &mut W,
) -> Result<(), ServiceError> {
    let mut lines = BufReader::new(reader).lines();
    let mut events = LiveEvents::new(session.service.as_ref())?;
    write_text(
        writer,
        "QCodeHarness 文本界面。输入 /help 查看命令，/quit 关闭服务。普通输入将创建任务。",
    )
    .await?;
    loop {
        session.pump();
        for output in std::mem::take(&mut session.output) {
            write_text(writer, &output).await?;
        }
        if session.closed && session.idle() {
            // EOF 与事件可能同时就绪；只排空当前订阅已就绪的结果，保留错误且不重放历史。
            tokio::select! {
                biased;
                _ = exit_deadline(session.deadline) => return Ok(()),
                event = events.subscription.next() => {
                    if let Some(event) = events.accept(session.service.as_ref(), event)? {
                        session.observe(&event);
                    }
                }
                _ = std::future::ready(()) => return Ok(()),
            }
            continue;
        }
        tokio::select! {
            line = lines.next_line(), if !session.closed => match line.map_err(io_error)? {
                Some(line) => session.enqueue(line),
                None => session.close(),
            },
            event = events.subscription.next() => {
                if let Some(event) = events.accept(session.service.as_ref(), event)? {
                    session.observe(&event);
                }
            }
            Some(result) = session.running.join_next(), if !session.running.is_empty() => session.complete(result)?,
            signal = tokio::signal::ctrl_c(), if !session.closed => { signal.map_err(io_error)?; session.interrupt(); }
            _ = exit_deadline(session.deadline) => {
                write_text(writer, &format!("[退出] {EXIT_WARNING}")).await?;
                return Ok(());
            }
        }
    }
}

async fn exit_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn write_text<W: AsyncWrite + Unpin>(writer: &mut W, text: &str) -> Result<(), ServiceError> {
    writer
        .write_all(sanitize(text).as_bytes())
        .await
        .map_err(io_error)?;
    writer.write_all(b"\n").await.map_err(io_error)?;
    writer.flush().await.map_err(io_error)
}

/// 运行全屏终端界面。要求 stdin 与 stdout 都连接到终端；恢复终端后关闭服务。
///
/// 键盘轮询使用至多一个有界阻塞任务，错误或退出时等待它结束；终端模式通过
/// RAII 保护，任务面板来自服务投影，事件区只展示事实，不从输出推断沙箱保护。
/// 退出等待超过两秒时返回 `terminal_drain_timeout`，调用方可在恢复终端后显示诊断。
pub async fn run_tui(service: Arc<dyn TaskService>) -> Result<(), ServiceError> {
    let mut session = Session::new(service);
    let result = match TerminalGuard::enter() {
        Ok(mut guard) => {
            let result = tui_loop(&mut session).await;
            let restored = guard.restore();
            result.and(restored)
        }
        Err(error) => Err(error),
    };
    let shutdown = session.shutdown().await;
    result.and(shutdown)
}

struct TerminalGuard {
    active: bool,
}
impl TerminalGuard {
    fn enter() -> Result<Self, ServiceError> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(ServiceError::new(
                "terminal_required",
                "全屏界面需要交互终端；管道或屏幕阅读器请用 --text",
            ));
        }
        terminal::enable_raw_mode().map_err(io_error)?;
        let guard = Self { active: true };
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            DisableLineWrap,
            Hide
        )
        .map_err(io_error)?;
        Ok(guard)
    }
    fn restore(&mut self) -> Result<(), ServiceError> {
        if !self.active {
            return Ok(());
        }
        let screen = execute!(
            std::io::stdout(),
            Show,
            EnableLineWrap,
            DisableBracketedPaste,
            LeaveAlternateScreen
        )
        .map_err(io_error);
        let raw = terminal::disable_raw_mode().map_err(io_error);
        if screen.is_ok() && raw.is_ok() {
            self.active = false;
        }
        screen.and(raw)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn poll_event() -> JoinHandle<std::io::Result<Option<Event>>> {
    tokio::task::spawn_blocking(|| {
        if event::poll(POLL_INTERVAL)? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    })
}

async fn tui_loop(session: &mut Session) -> Result<(), ServiceError> {
    let mut events = LiveEvents::new(session.service.as_ref())?;
    let mut editor = InputEditor::default();
    let mut feed = Feed::default();
    feed.push("输入 /help 查看命令；Tab 切换任务，PageUp / PageDown 滚动事件。".into());
    session.enqueue("/list".into());
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut poll = poll_event();
    let result = async {
        loop {
            session.pump();
            for output in std::mem::take(&mut session.output) { feed.push(output); }
            draw(session, &feed, &editor)?;
            if session.closed && session.idle() { return Ok(()); }
            tokio::select! {
                result = &mut poll => {
                    // 先接上下一次有界轮询，任何后续错误都由外层统一等待它退出。
                    poll = poll_event();
                    let event = result.map_err(|error| ServiceError::new("terminal_worker", error.to_string()))?.map_err(io_error)?;
                    match event {
                        Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                            if key.modifiers.contains(KeyModifiers::CONTROL) {
                                match key.code {
                                    KeyCode::Char('c') => session.interrupt(),
                                    KeyCode::Char('d') if editor.text().is_empty() => session.close(),
                                    KeyCode::Char('u') => { editor.take(); }
                                    KeyCode::Char('a') => editor.apply(KeyCode::Home),
                                    KeyCode::Char('e') => editor.apply(KeyCode::End),
                                    _ => {}
                                }
                            } else { match key.code {
                                KeyCode::Enter => { session.enqueue(editor.take()); feed.scroll = 0; }
                                KeyCode::Tab => cycle_task(session, false),
                                KeyCode::BackTab => cycle_task(session, true),
                                KeyCode::PageUp => feed.scroll = (feed.scroll + 10).min(HISTORY_LINES - 1),
                                KeyCode::PageDown => feed.scroll = feed.scroll.saturating_sub(10),
                                code if !key.modifiers.contains(KeyModifiers::ALT) => editor.apply(code),
                                _ => {}
                            } }
                        }
                        Some(Event::Paste(text)) => editor.insert(&text),
                        _ => {}
                    }
                }
                event = events.subscription.next() => {
                    if let Some(event) = events.accept(session.service.as_ref(), event)? { session.observe(&event); }
                }
                Some(result) = session.running.join_next(), if !session.running.is_empty() => session.complete(result)?,
                _ = refresh.tick(), if !session.closed => {
                    if session.queue.is_empty() && !session.busy { session.enqueue("/list".into()); }
                }
                _ = exit_deadline(session.deadline) => {
                    // 退出会恢复全屏区，通过返回值保留未完成命令的诊断。
                    return Err(ServiceError::new("terminal_drain_timeout", EXIT_WARNING));
                }
            }
        }
    }.await;
    // 不丢弃仍在读取终端的阻塞任务，恢复终端前让最后一次 poll 完成。
    let last_poll = poll
        .await
        .map_err(|error| ServiceError::new("terminal_worker", error.to_string()))?
        .map_err(io_error);
    result.and(last_poll.map(|_| ()))
}

fn cycle_task(session: &mut Session, backwards: bool) {
    let ids: Vec<_> = session.tasks.keys().cloned().collect();
    if ids.is_empty() {
        return;
    }
    let selected = session
        .selected
        .as_ref()
        .and_then(|selected| ids.iter().position(|id| id == selected));
    let index = match selected {
        None => {
            if backwards {
                ids.len() - 1
            } else {
                0
            }
        }
        Some(index) => {
            if backwards {
                (index + ids.len() - 1) % ids.len()
            } else {
                (index + 1) % ids.len()
            }
        }
    };
    session.selected = Some(ids[index].clone());
    session.interrupted = false;
}

#[derive(Default)]
struct Feed {
    lines: VecDeque<String>,
    scroll: usize,
}
impl Feed {
    fn push(&mut self, text: String) {
        for line in sanitize(&text).split('\n') {
            self.lines.push_back(preview(line));
            if self.lines.len() > HISTORY_LINES {
                self.lines.pop_front();
            }
        }
    }
    fn visible(&self, width: usize, height: usize) -> Vec<String> {
        let mut wrapped = VecDeque::new();
        for line in &self.lines {
            let mut current = String::new();
            let mut used = 0;
            for ch in line.chars() {
                let cells = ch.width().unwrap_or(0);
                if used + cells > width && !current.is_empty() {
                    bounded_line(&mut wrapped, std::mem::take(&mut current));
                    used = 0;
                }
                if cells <= width {
                    current.push(ch);
                    used += cells;
                }
            }
            bounded_line(&mut wrapped, current);
        }
        let end = wrapped
            .len()
            .saturating_sub(self.scroll.min(wrapped.len().saturating_sub(height)));
        let start = end.saturating_sub(height);
        wrapped.into_iter().skip(start).take(end - start).collect()
    }
}
fn bounded_line(lines: &mut VecDeque<String>, line: String) {
    lines.push_back(line);
    if lines.len() > HISTORY_LINES {
        lines.pop_front();
    }
}

fn draw(session: &Session, feed: &Feed, editor: &InputEditor) -> Result<(), ServiceError> {
    let (width, height) = terminal::size().map_err(io_error)?;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    queue!(out, MoveTo(0, 0), Clear(ClearType::All)).map_err(io_error)?;
    let mut rows = vec!["QCodeHarness | Tab 切换任务 | /help | /quit".into()];
    if height >= 7 {
        let count = (usize::from(height) / 3).clamp(1, 6);
        rows.push(format!("任务（{}） | > 为当前任务", session.tasks.len()));
        let tasks: Vec<_> = session.tasks.values().collect();
        let selected = tasks
            .iter()
            .position(|task| Some(task.task_id.as_str()) == session.selected.as_deref())
            .unwrap_or(0);
        let start = selected
            .saturating_sub(count / 2)
            .min(tasks.len().saturating_sub(count));
        for task in tasks.into_iter().skip(start).take(count) {
            rows.push(format!(
                "{} {} [{}] 执行 {}",
                if Some(task.task_id.as_str()) == session.selected.as_deref() {
                    ">"
                } else {
                    " "
                },
                task.task_id,
                status(task.status),
                task.execution_id
            ));
        }
        if session.tasks.is_empty() {
            rows.push("尚无任务。输入文字创建一个任务。".into());
        }
    }
    rows.push(format!(
        "事件 | 已选 {}{}",
        session.selected.as_deref().unwrap_or("无"),
        if session.closed {
            " | 正在关闭"
        } else {
            ""
        }
    ));
    let available = usize::from(height).saturating_sub(rows.len() + 2);
    rows.extend(feed.visible(usize::from(width), available));
    while rows.len() < usize::from(height).saturating_sub(2) {
        rows.push(String::new());
    }
    rows.push(format!(
        "{} | 待处理命令 {}",
        if session.closed {
            "正在关闭服务"
        } else {
            "输入 Enter 提交；Ctrl-C 取消/退出"
        },
        session.queue.len() + session.urgent_queue.len() + session.running.len()
    ));
    rows.push(input_view(editor, usize::from(width)));
    for (index, row) in rows.into_iter().take(usize::from(height)).enumerate() {
        queue!(
            out,
            MoveTo(0, index as u16),
            Print(clip_cells(&sanitize(&row), usize::from(width)))
        )
        .map_err(io_error)?;
    }
    out.flush().map_err(io_error)
}

fn input_view(editor: &InputEditor, width: usize) -> String {
    let prefix = "输入> ";
    let room = width.saturating_sub(prefix.width() + 1);
    let before = sanitize(&editor.text[..editor.cursor]);
    let after = sanitize(&editor.text[editor.cursor..]);
    let mut tail = VecDeque::new();
    let mut used = 0;
    for ch in before.chars().rev() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > room {
            break;
        }
        used += cells;
        tail.push_front(ch);
    }
    let before: String = tail.into_iter().collect();
    format!(
        "{prefix}{before}|{}",
        clip_cells(&after, room.saturating_sub(used))
    )
}

fn clip_cells(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > width {
            break;
        }
        result.push(ch);
        used += cells;
    }
    result
}

fn sanitize(text: &str) -> String {
    let mut safe = String::new();
    for ch in text.chars() {
        match ch {
            '\n' => safe.push('\n'),
            '\t' => safe.push_str("    "),
            ch if ch.is_control()
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                safe.extend(ch.escape_default())
            }
            ch => safe.push(ch),
        }
    }
    safe
}

fn preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut head: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        head.push_str(" …（显示已截断，可用 /events 回放）");
    }
    head
}

fn status(value: TaskStatus) -> &'static str {
    match value {
        TaskStatus::Running => "运行中",
        TaskStatus::Stopping => "正在取消",
        TaskStatus::Cancelled => "已取消",
        TaskStatus::BudgetExhausted => "预算耗尽",
        TaskStatus::Failed => "失败",
        TaskStatus::Completed => "已完成",
        TaskStatus::Interrupted => "已中断",
        TaskStatus::Merged => "已合并",
        TaskStatus::Cleaned => "工作区已删除",
    }
}

fn render_task(task: &TaskView) -> String {
    let mut text = format!(
        "任务 {} [{}] 会话 {} 执行 {} 第 {} 次\n工作区 {} 基线 {}",
        task.task_id,
        status(task.status),
        task.session_id,
        task.execution_id,
        task.attempt,
        task.workspace.display(),
        task.base_revision
    );
    if let Some(answer) = &task.answer {
        text.push_str(&format!("\n答复：{answer}"));
    }
    if let Some(error) = &task.error {
        text.push_str(&format!("\n错误：{error}"));
    }
    if !task.verification.is_empty() {
        text.push_str(&format!("\n工作区校验：{}", task.verification));
    }
    text
}

fn render_approval(request: &PendingApproval) -> String {
    format!(
        "[待审批] {} task={} execution={} resumed={}\n工具 {}\n理由：{}\n注解：只读={} 破坏性={} 幂等={} 外部交互={}\n/approve {} {} {} 或 /deny {} {} {}（仅此一次）",
        request.approval_id,
        request.task_id,
        request.execution_id,
        request.resumed,
        render_call(&request.call),
        request.reason.as_deref().unwrap_or("服务未提供升级理由"),
        request.annotations.read_only,
        request.annotations.destructive,
        request.annotations.idempotent,
        request.annotations.open_world,
        request.approval_id,
        request.task_id,
        request.execution_id,
        request.approval_id,
        request.task_id,
        request.execution_id
    )
}

fn render_reply(reply: &ServiceReply) -> Vec<String> {
    match reply {
        ServiceReply::Task { task } => vec![render_task(task)],
        ServiceReply::Tasks { tasks } if tasks.is_empty() => vec!["[任务] 尚无任务".into()],
        ServiceReply::Tasks { tasks } => tasks
            .iter()
            .map(|task| {
                format!(
                    "任务 {} [{}] 执行 {}",
                    task.task_id,
                    status(task.status),
                    task.execution_id
                )
            })
            .collect(),
        ServiceReply::Accepted {
            task_id,
            execution_id,
        } => vec![format!(
            "[受理] 任务 {task_id}{}",
            execution_id
                .as_ref()
                .map(|id| format!(" 执行 {id}"))
                .unwrap_or_default()
        )],
        ServiceReply::PendingApprovals { requests } if requests.is_empty() => {
            vec!["[审批] 当前没有待审批项".into()]
        }
        ServiceReply::PendingApprovals { requests } => {
            requests.iter().map(render_approval).collect()
        }
        ServiceReply::Events { events, next } => {
            let mut lines: Vec<_> = events.iter().map(render_event).collect();
            lines.push(format!("[回放] 下一游标 {next}，只读展示，不执行历史动作"));
            lines
        }
        ServiceReply::Shutdown => vec!["[退出] 服务已关闭".into()],
    }
}

fn render_call(call: &ToolCall) -> String {
    format!("{}({})", call.name, call.arguments)
}

fn render_event(event: &ServiceEvent) -> String {
    let text = match &event.payload {
        ServiceEventPayload::TaskCreated { task } => format!("任务已创建：{}", render_task(task)),
        ServiceEventPayload::ExecutionStarted => "新执行已开始".into(),
        ServiceEventPayload::Audit { event } => render_audit(event),
        ServiceEventPayload::ModelCallStarted { role } => format!("模型调用开始：{}", role.name()),
        ServiceEventPayload::ModelCallFinished {
            text,
            calls,
            retried,
        } => format!(
            "模型回复（重试={retried}）：{text}{}",
            if calls.is_empty() {
                String::new()
            } else {
                format!(
                    "\n工具请求：{}",
                    calls.iter().map(render_call).collect::<Vec<_>>().join("\n")
                )
            }
        ),
        ServiceEventPayload::ApprovalWaiting { request } => render_approval(request),
        ServiceEventPayload::ApprovalResolved {
            approval_id,
            allowed,
            reason,
        } => format!(
            "审批 {approval_id} {}{}",
            if *allowed {
                "已批准本次请求"
            } else {
                "已拒绝"
            },
            reason
                .as_ref()
                .map(|reason| format!("：{reason}"))
                .unwrap_or_default()
        ),
        ServiceEventPayload::InputReceived { message_id, text } => {
            format!("输入 {message_id} 已接收：{text}")
        }
        ServiceEventPayload::InputApplied { message_id } => format!("输入 {message_id} 已应用"),
        ServiceEventPayload::CancelRequested => "已请求取消，等待执行停止".into(),
        ServiceEventPayload::Stopped {
            status: value,
            error,
        } => format!(
            "执行停止 [{}]{}",
            status(*value),
            error
                .as_ref()
                .map(|error| format!("：{error}"))
                .unwrap_or_default()
        ),
        ServiceEventPayload::Completed { answer } => format!("执行完成：{answer}"),
        ServiceEventPayload::Forked { source_task } => format!("已从任务 {source_task} 分叉"),
        ServiceEventPayload::WorkspaceMerged { verification } => {
            format!("工作区已合并，校验：{verification}")
        }
        ServiceEventPayload::WorkspaceRemoved => "任务工作区已删除".into(),
    };
    format!(
        "[事件 #{} task={} execution={}] {text}",
        event.sequence,
        event.task_id.as_deref().unwrap_or("-"),
        event.execution_id.as_deref().unwrap_or("-")
    )
}

fn render_audit(event: &AuditEvent) -> String {
    match event {
        AuditEvent::Thought { text, .. } => format!("思考：{text}"),
        AuditEvent::StepStarted { role, .. } => format!("角色 {} 开始执行", role.name()),
        AuditEvent::ToolRequested { call, .. } => format!("请求工具：{}", render_call(call)),
        AuditEvent::ToolObserved { call, outcome, .. } => format!(
            "工具 {} [{:?}]：{}",
            call.name, outcome.status, outcome.content
        ),
        AuditEvent::ApprovalDecided {
            tool,
            allowed,
            reason,
            ..
        } => format!(
            "工具 {tool} 审批 allowed={allowed}：{}",
            reason.as_deref().unwrap_or("无附加理由")
        ),
        AuditEvent::PolicyEvaluated {
            tool,
            verdict,
            reason,
            ..
        } => format!("策略 {tool} [{verdict:?}]：{reason}"),
        AuditEvent::SandboxApplied {
            call,
            platform,
            active,
            boundary_checked,
            ..
        } => format!(
            "执行边界 {} {platform}：{}；命令字面路径检查={}；不提供文件系统或网络隔离",
            call.name,
            if *active {
                "进程沙箱已生效"
            } else {
                "无进程沙箱保护"
            },
            boundary_checked
        ),
        AuditEvent::ToolCallReplayed {
            tool,
            reason,
            previous_outcome,
            ..
        } => format!(
            "恢复 {tool}：{reason}；{}，未自动重放",
            if previous_outcome.is_none() {
                "结果未知"
            } else {
                "存在先前调用结果供参考"
            }
        ),
        AuditEvent::ApprovalReasked { tool, reason, .. } => {
            format!("恢复后重新审批 {tool}：{reason}")
        }
        AuditEvent::ContextCompacted {
            rounds_before,
            rounds_after,
            ..
        } => format!("上下文裁剪：{rounds_before} → {rounds_after} 轮"),
        AuditEvent::ContextBudgeted {
            tokens_before,
            tokens_after,
            ..
        } => format!("上下文预算：{tokens_before} → {tokens_after} token（估算）"),
        AuditEvent::SummaryGenerated { covers, .. } => {
            format!("摘要已生成：第 {} 到 {} 轮", covers.0, covers.1)
        }
        AuditEvent::SummaryFailed { reason, .. } => format!("摘要失败：{reason}"),
        AuditEvent::McpLifecycle { fact, .. } => format!(
            "MCP {} {:?} 第 {} 代，进程沙箱保护={}{}",
            fact.server,
            fact.kind,
            fact.generation,
            fact.process_isolation,
            fact.fault
                .map(|fault| format!("，故障：{fault}"))
                .unwrap_or_default()
        ),
        AuditEvent::McpCallFailed { tool, failure, .. } => format!(
            "MCP {tool} 调用故障：{}；{}；进程沙箱保护={}；未自动重放",
            failure.kind,
            if failure.result_unknown {
                "远端结果未知"
            } else {
                "服务未标记结果未知"
            },
            failure.process_isolation
        ),
    }
}
