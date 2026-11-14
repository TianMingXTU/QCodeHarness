// QCodeHarness local Web UI 前端：原生 JavaScript，无框架、CDN 或远程资源。
// 页面只消费常驻服务的 NDJSON 事件与命令回执；审批按钮转发 decide 命令，
// 证据字段按服务事件原样展示，避免前端自行推断结论。

const state = {
  cursor: 0,
  taskId: '',
};

const $ = (id) => document.getElementById(id);

function showReply(value) {
  $('reply').textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

async function postCommand(id, command) {
  const response = await fetch('/command', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id, command }),
  });
  const text = await response.text();
  let value;
  try {
    value = JSON.parse(text);
  } catch (_) {
    value = text;
  }
  if (!response.ok) {
    throw new Error(`服务返回 ${response.status}: ${text}`);
  }
  showReply(value);
  return value;
}

async function getJson(url) {
  const response = await fetch(url);
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`服务返回 ${response.status}: ${text}`);
  }
  return JSON.parse(text);
}

function eventTitle(event) {
  const payload = event.payload || {};
  const type = payload.type || 'unknown';
  const audit = payload.event ? Object.keys(payload.event)[0] : '';
  return `#${event.sequence} ${type}${audit ? ` / ${audit}` : ''}`;
}

function appendEvent(event) {
  const row = document.createElement('div');
  row.className = 'event';
  const title = document.createElement('div');
  title.textContent = eventTitle(event);
  const meta = document.createElement('div');
  meta.className = 'meta';
  meta.textContent = `task=${event.task_id || ''} session=${event.session_id || ''} execution=${event.execution_id || ''} origin=${JSON.stringify(event.origin || {})}`;
  const payload = document.createElement('pre');
  payload.textContent = JSON.stringify(event.payload, null, 2);
  row.append(title, meta, payload);
  $('events').appendChild(row);
  state.cursor = Math.max(state.cursor, Number(event.sequence) || 0);
}

async function refreshEvents() {
  try {
    const response = await fetch(`/events?after=${state.cursor}&limit=1000`);
    if (!response.ok) {
      throw new Error(`事件流返回 ${response.status}`);
    }
    const text = await response.text();
    for (const line of text.split('\n')) {
      if (!line.trim()) continue;
      appendEvent(JSON.parse(line));
    }
  } catch (error) {
    $('events').appendChild(document.createTextNode(`刷新事件失败: ${error.message}`));
  }
}

function renderApproval(request) {
  const box = document.createElement('div');
  box.className = 'event';
  const title = document.createElement('div');
  title.textContent = `待审批 ${request.approval_id} / ${request.call.name}`;
  const meta = document.createElement('div');
  meta.className = 'meta';
  meta.textContent = `task=${request.task_id} session=${request.session_id} execution=${request.execution_id} turn=${request.turn} resumed=${request.resumed}`;
  const reason = document.createElement('div');
  reason.textContent = request.reason || '服务未给出额外原因';
  const evidence = document.createElement('pre');
  evidence.textContent = JSON.stringify(
    { call: request.call, annotations: request.annotations, origin: request.origin },
    null,
    2,
  );
  const approve = document.createElement('button');
  approve.textContent = '批准';
  approve.addEventListener('click', () => decide(request, true));
  const deny = document.createElement('button');
  deny.textContent = '拒绝';
  deny.addEventListener('click', () => decide(request, false));
  box.append(title, meta, reason, evidence, approve, deny);
  return box;
}

async function decide(request, allow) {
  try {
    await postCommand(`web-decide-${request.approval_id}`, {
      op: 'decide',
      approval_id: request.approval_id,
      task_id: request.task_id,
      execution_id: request.execution_id,
      allow,
    });
    await Promise.all([refreshEvents(), refreshApprovals()]);
  } catch (error) {
    showReply(`审批命令失败: ${error.message}`);
  }
}

async function refreshApprovals() {
  try {
    const reply = await getJson('/command?op=approvals');
    const container = $('approvals');
    container.textContent = '';
    const requests = reply.requests || [];
    if (requests.length === 0) {
      container.textContent = '当前没有待审批项';
      return;
    }
    for (const request of requests) {
      container.appendChild(renderApproval(request));
    }
  } catch (error) {
    $('approvals').textContent = `查询待审批失败: ${error.message}`;
  }
}

async function createTask() {
  try {
    const reply = await postCommand(`web-create-${Date.now()}`, {
      op: 'create',
      goal: $('goal').value,
      kind: $('kind').value,
    });
    if (reply && reply.task && reply.task.task_id) {
      state.taskId = reply.task.task_id;
      $('task-id').value = state.taskId;
    }
    await Promise.all([refreshEvents(), refreshApprovals()]);
  } catch (error) {
    showReply(`创建任务失败: ${error.message}`);
  }
}

async function submitInput() {
  try {
    await postCommand(`web-input-${Date.now()}`, {
      op: 'input',
      task_id: $('task-id').value,
      message_id: `web-${Date.now()}`,
      text: $('message').value,
    });
    await refreshEvents();
  } catch (error) {
    showReply(`补充输入失败: ${error.message}`);
  }
}

$('create').addEventListener('click', createTask);
$('input').addEventListener('click', submitInput);
$('refresh-approvals').addEventListener('click', refreshApprovals);
$('refresh-events').addEventListener('click', refreshEvents);

refreshEvents();
refreshApprovals();
setInterval(refreshEvents, 2000);
