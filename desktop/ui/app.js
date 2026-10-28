import { stateLabel, canCancel, printerSetup, printerFromForm, connectionResult, authenticationNotice, discoveryNotice } from './model.mjs';

const $ = (id) => document.getElementById(id);
const invoke = (command, args = {}) => window.__TAURI__.core.invoke(command, args);
let printers = [];
let service = { running: false, address: null };
let busy = false;
let downloadPending = false;
let checkingUpdate = false;
let componentStatus;
let jobSnapshot;
let connectionSnapshot;

function node(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
}
function message(text, error = false) {
  $('message').textContent = text;
  $('message').className = error ? 'error' : '';
  $('message').hidden = false;
}
async function action(work, control) {
  if (busy) return;
  busy = true;
  if (control) {
    control.setAttribute('aria-busy', 'true');
    $('operation-state').hidden = false;
  }
  try { await work(); } catch (error) { message(String(error), true); }
  finally {
    busy = false;
    if (control) {
      control.removeAttribute('aria-busy');
      $('operation-state').hidden = true;
    }
  }
}
function confirmAction(title, description, label) {
  const dialog = $('confirm-dialog');
  $('confirm-title').textContent = title;
  $('confirm-description').textContent = description;
  $('confirm-submit').textContent = label;
  dialog.returnValue = '';
  return new Promise((resolve) => {
    dialog.addEventListener('close', () => resolve(dialog.returnValue === 'confirm'), { once: true });
    dialog.showModal();
  });
}
function button(text, handler, className = '') {
  const el = node('button', text, className);
  el.type = 'button';
  el.addEventListener('click', () => action(handler, el));
  return el;
}
function empty(list, text) { list.replaceChildren(node('div', text, 'empty')); }
function badge(text, warning = false) { return node('span', text, `badge${warning ? ' warning' : ''}`); }
function table(headers) {
  const el = node('table', undefined, 'data-table');
  const head = el.createTHead().insertRow();
  for (const title of headers) { const th = node('th', title); th.scope = 'col'; head.append(th); }
  return { el, body: el.createTBody() };
}
function cell(row, text, className) { const td = row.insertCell(); if (text) td.textContent = text; if (className) td.className = className; return td; }
const time = (timestamp) => new Date(timestamp * 1000).toLocaleString('zh-CN', { hour12: false });
async function refreshStatus() {
  service = await invoke('service_status');
  $('mode').textContent = service.simulation ? '模拟模式 · 不真实打印' : 'Windows 本机';
  $('service-state').textContent = service.running ? 'IPP 服务运行中' : '服务未启动';
  $('service-address').textContent = service.address || '仅在可信局域网使用';
  $('discovery-note').textContent = discoveryNotice(service);
  $('lamp').classList.toggle('running', service.running);
  $('start').disabled = service.running;
  $('stop').disabled = !service.running;
  $('address').disabled = service.running;
  $('authentication-required').checked = service.authentication_required;
  $('authentication-note').textContent = authenticationNotice(service);
  $('authentication-note').classList.toggle('pending', service.authentication_restart_required);
  if (service.address) $('address').value = service.address;
  if (componentStatus) renderSumatra();
}
async function refreshQueues() {
  const available = await invoke('available_printers');
  const selected = $('queue').value;
  $('queue').replaceChildren(new Option('请选择本机队列', ''));
  for (const printer of available) $('queue').add(new Option(printer.name, printer.name));
  $('queue').value = selected;
}
function editPrinter(printer) {
  const form = $('printer-form');
  for (const key of ['id', 'name', 'max_copies', 'default_paper']) form.elements[key].value = printer[key];
  if (![...$('queue').options].some((option) => option.value === printer.queue)) $('queue').add(new Option(`${printer.queue}（当前未枚举到）`, printer.queue));
  form.elements.queue.value = printer.queue;
  form.elements.enabled.checked = printer.enabled;
  form.elements.duplex.checked = printer.duplex;
  for (const input of form.querySelectorAll('[name=paper_sizes]')) input.checked = printer.paper_sizes.includes(input.value);
  form.elements.id.readOnly = true;
  form.elements.name.focus();
}
function renderPrinters() {
  $('printer-count').textContent = printers.length;
  const list = $('printer-list'); list.replaceChildren();
  for (const printer of printers) {
    const card = node('article', undefined, 'printer-card');
    const title = node('div', undefined, 'printer-title');
    const heading = node('h3', printer.name);
    title.append(heading, badge(printer.enabled ? '已启用' : '已停用'), button('编辑', async () => editPrinter(printer), 'small-button'));
    card.append(title, node('p', `${printer.queue}；${printer.paper_sizes.join(' / ')}（默认 ${printer.default_paper}）；${printer.duplex ? '支持双面' : '仅单面'}；最多 ${printer.max_copies} 份`, 'muted'));
    const setup = printerSetup(service.address || $('address').value.trim(), printer.id);
    if (setup) {
      const fields = node('div', undefined, 'setup-fields');
      for (const [key, label] of [['address', 'Address 地址'], ['protocol', 'Protocol 协议'], ['queue', 'Queue 队列'], ['ppd', 'PPD 在 Mac 下载驱动']]) {
        const field = node('div', undefined, 'setup-field');
        const value = node('div', undefined, 'copy-field');
        value.append(node('code', setup[key]), button('复制', async () => {
          await navigator.clipboard.writeText(setup[key]); message(`${label}已复制。`);
        }, 'small-button'));
        field.append(node('span', label, 'field-label'), value); fields.append(field);
      }
      card.append(fields);
    } else card.append(node('p', `Mac 可通过自动发现添加；手动添加时使用 Windows 的实际局域网 IP 和队列 ipp/printers/${encodeURIComponent(printer.id)}。`, 'muted'));
    list.append(card);
  }
  if (!printers.length) empty(list, '还没有共享打印机。先选择一个 Windows 队列。');
}
async function refreshPrinters() {
  printers = await invoke('list_printers');
  const selected = $('filter').value;
  $('filter').replaceChildren(new Option('全部打印机', ''));
  for (const printer of printers) $('filter').add(new Option(printer.name, printer.id));
  $('filter').value = selected;
  renderPrinters();
}
async function refreshJobs() {
  const jobs = await invoke('list_jobs');
  const filtered = jobs.filter((job) => !$('filter').value || job.printer_id === $('filter').value);
  $('job-count').textContent = filtered.length;
  const snapshot = JSON.stringify([filtered, printers.map((p) => [p.id, p.name])]);
  if (snapshot === jobSnapshot) return;
  jobSnapshot = snapshot;
  const list = $('job-list');
  if (!filtered.length) { empty(list, '暂无打印任务。'); return; }
  const { el, body } = table(['任务 / 打印机', '设备', '打印选项', '状态', '创建时间', '详情', '操作']);
  for (const job of filtered) {
    const row = body.insertRow();
    const printer = printers.find((p) => p.id === job.printer_id);
    cell(row, `#${job.id}`, 'identity-cell').append(node('span', printer?.name || job.printer_id, 'secondary'));
    cell(row, job.device_id === 'anonymous' ? '匿名客户端' : job.device_id, 'identity-cell');
    const sides = { 'one-sided': '单面', 'two-sided-long-edge': '双面 · 长边', 'two-sided-short-edge': '双面 · 短边' };
    cell(row, `${job.paper} · ${job.copies} 份`, 'options-cell').append(node('span', sides[job.sides] || job.sides, 'secondary'));
    cell(row, '', 'status-cell').append(badge(stateLabel(job.state), ['uncertain', 'failed'].includes(job.state)));
    cell(row, job.created_at ? time(job.created_at) : '—', 'time-cell');
    cell(row, job.detail || '—', 'details-cell');
    const actions = cell(row, '', 'action-cell');
    if (canCancel(job)) actions.append(button('取消', async () => { await invoke('cancel_job', { id: job.id }); await refreshJobs(); message('等待任务已取消。'); }, 'small-button'));
  }
  list.replaceChildren(el);
}
async function refreshConnections() {
  const events = await invoke('list_connection_events');
  $('connection-count').textContent = events.length;
  const snapshot = JSON.stringify(events);
  if (snapshot === connectionSnapshot) return;
  connectionSnapshot = snapshot;
  const list = $('connection-list');
  if (!events.length) { empty(list, '尚未收到请求。在 Mac 查询或打印后，来源 IP 和结果会显示在这里。'); return; }
  const operations = { 2: ['提交打印任务', 'Print-Job'], 4: ['验证打印参数', 'Validate-Job'], 5: ['创建等待任务', 'Create-Job'], 6: ['上传打印文档', 'Send-Document'], 8: ['取消任务', 'Cancel-Job'], 9: ['查询任务状态', 'Get-Job-Attributes'], 10: ['查询任务列表', 'Get-Jobs'], 11: ['查询打印机', 'Get-Printer-Attributes'] };
  const { el, body } = table(['时间', '客户端', '操作 / 路径', '结果', '协议状态', '详情']);
  for (const event of events) {
    const row = body.insertRow();
    const result = connectionResult(event);
    const operation = operations[event.operation] || ['HTTP 请求', event.operation == null ? '' : `IPP ${event.operation}`];
    cell(row, time(event.timestamp), 'time-cell');
    cell(row, event.client_ip || '来源未知', 'identity-cell');
    cell(row, operation[0], 'operation-cell').append(node('span', operation[1], 'secondary'), node('code', `${event.method} ${event.path}`, 'secondary'));
    cell(row, '', 'status-cell').append(badge(result.label, result.failed));
    const protocol = cell(row, `HTTP ${event.http_status}`, 'protocol-cell');
    if (event.ipp_status != null) protocol.append(node('span', `IPP 0x${event.ipp_status.toString(16).padStart(4, '0')}`, 'secondary'));
    cell(row, event.detail || '—', 'details-cell');
  }
  list.replaceChildren(el);
}
async function refreshDevices() {
  const devices = await invoke('list_devices');
  const list = $('device-list'); list.replaceChildren();
  for (const device of devices) {
    const row = node('article', undefined, 'list-card');
    const body = node('div'); const heading = node('div', undefined, 'device-heading');
    heading.append(node('h3', device.name), badge('可打印'));
    body.append(heading, node('code', `用户名：${device.id}`));
    row.append(body, button('撤销并移除', async () => {
      if (!await confirmAction('撤销设备凭据？', `“${device.name}”的凭据将立即失效并从列表移除，历史打印任务仍保留。`, '撤销并移除')) return;
      await invoke('revoke_device', { id: device.id }); await refreshDevices(); message('设备凭据已撤销并移除。');
    }, 'small-button danger'));
    list.append(row);
  }
  const auth = service.running ? service.active_authentication_required : service.authentication_required;
  if (!devices.length) empty(list, auth ? '创建设备凭据后，Mac 才能提交任务。' : '认证已关闭，Mac 可以直接提交任务。');
}
function activateTab(tab) {
  for (const item of document.querySelectorAll('[data-tab]')) {
    const active = item === tab; item.classList.toggle('active', active); item.setAttribute('aria-selected', active);
    item.tabIndex = active ? 0 : -1;
    $(item.dataset.tab).hidden = !active;
  }
  window.history.replaceState(null, '', `#${tab.dataset.tab}`);
  if (tab.dataset.tab !== 'devices') hideSecret();
  return action(tab.dataset.tab === 'connections' ? refreshConnections : tab.dataset.tab === 'jobs' ? refreshJobs : tab.dataset.tab === 'devices' ? refreshDevices : tab.dataset.tab === 'settings' ? refreshSumatra : refreshPrinters, tab);
}
const tabs = [...document.querySelectorAll('[data-tab]')];
for (const tab of tabs) {
  tab.addEventListener('click', () => activateTab(tab));
  tab.addEventListener('keydown', (event) => {
    const index = tabs.indexOf(tab);
    const next = { ArrowRight: (index + 1) % tabs.length, ArrowLeft: (index + tabs.length - 1) % tabs.length, Home: 0, End: tabs.length - 1 }[event.key];
    if (next === undefined) return;
    event.preventDefault();
    tabs[next].focus();
    activateTab(tabs[next]);
  });
}
$('printer-form').addEventListener('submit', (event) => { event.preventDefault(); action(async () => {
  await invoke('save_printer', { printer: printerFromForm(new FormData(event.target)) });
  resetPrinter(); await refreshPrinters(); message('打印机配置已保存。');
}, event.submitter); });
function resetPrinter() { $('printer-form').reset(); $('printer-form').elements.id.readOnly = false; }
$('reset-printer').addEventListener('click', resetPrinter);
$('refresh-printers').addEventListener('click', () => action(async () => { await Promise.all([refreshQueues(), refreshPrinters()]); }, $('refresh-printers')));
$('refresh-jobs').addEventListener('click', () => action(refreshJobs, $('refresh-jobs')));
$('refresh-connections').addEventListener('click', () => action(refreshConnections, $('refresh-connections')));
$('clear-jobs').addEventListener('click', () => action(async () => {
  if (!await confirmAction('清除全部已结束记录？', '将删除全部打印机的已结束任务及其缓存文档，不受当前筛选影响。等待、处理中和待核查任务将保留，此操作无法撤销。', '清除记录')) return;
  const count = await invoke('clear_job_history');
  await refreshJobs();
  message(`已清除 ${count} 条打印记录。等待、处理中和待核查任务已保留。`);
}, $('clear-jobs')));
$('clear-connections').addEventListener('click', () => action(async () => {
  if (!await confirmAction('清除连接诊断记录？', '将清空本次运行已记录的连接诊断，后续请求会继续记录。此操作无法撤销。', '清除记录')) return;
  const count = await invoke('clear_connection_events');
  await refreshConnections();
  message(`已清除 ${count} 条诊断记录。后续请求将继续记录。`);
}, $('clear-connections')));
$('filter').addEventListener('change', () => action(refreshJobs));
$('address').addEventListener('input', renderPrinters);
$('start').addEventListener('click', () => action(async () => {
  await invoke('start_service', { address: $('address').value.trim() }); await refreshStatus(); renderPrinters(); message('IPP 服务已启动。');
}, $('start')));
$('stop').addEventListener('click', () => action(async () => {
  await invoke('stop_service'); await refreshStatus(); renderPrinters(); message('服务已停止接收新任务，等待任务已保留。');
}, $('stop')));
$('authentication-required').addEventListener('change', async () => {
  if (busy) { $('authentication-required').checked = service.authentication_required; return; }
  $('authentication-required').disabled = true;
  await action(async () => {
    try {
      await invoke('set_authentication_required', { required: $('authentication-required').checked });
      await refreshStatus(); await refreshDevices(); message(authenticationNotice(service));
    } finally { $('authentication-required').checked = service.authentication_required; }
  });
  $('authentication-required').disabled = false;
});
function hideSecret() { $('secret').hidden = true; $('secret-id').value = ''; $('secret-token').value = ''; }
function renderSumatra() {
  const status = componentStatus;
  const downloading = status.downloading || downloadPending;
  $('sumatra-current').textContent = status.ready ? status.current_version || status.version : '未安装';
  $('sumatra-latest').textContent = status.latest_version || '尚未检查';
  $('sumatra-state').textContent = downloading ? '正在下载并校验，请稍候…' : checkingUpdate ? '正在检查官方稳定版…' : status.update_available ? service.running ? '有可用更新，请先停止服务。' : '有可用更新。' : status.ready ? '已就绪 · 校验通过' : '请下载打印组件。';
  $('sumatra-path').textContent = status.path;
  $('check-sumatra-update').disabled = checkingUpdate || downloading;
  $('download-sumatra').disabled = service.running || checkingUpdate || downloading || (status.ready && !status.update_available);
  $('download-sumatra').textContent = downloading ? '正在下载…' : status.ready ? status.update_available ? '下载更新' : '已安装' : '下载组件';
}
async function refreshSumatra() {
  const status = await invoke('sumatra_status');
  if (componentStatus?.latest_version && !status.latest_version) {
    status.latest_version = componentStatus.latest_version;
    status.update_available = componentStatus.update_available && status.current_version === componentStatus.current_version;
  }
  componentStatus = status; renderSumatra();
}
$('check-sumatra-update').addEventListener('click', () => action(async () => {
  checkingUpdate = true; renderSumatra();
  try {
    componentStatus = await invoke('check_sumatra_update');
    message(componentStatus.update_available ? `发现 SumatraPDF ${componentStatus.latest_version}，可下载更新。` : componentStatus.ready ? 'SumatraPDF 已是最新稳定版。' : `最新稳定版为 ${componentStatus.latest_version}，可下载组件。`);
  } finally { checkingUpdate = false; renderSumatra(); }
}, $('check-sumatra-update')));
$('download-sumatra').addEventListener('click', () => action(async () => {
  if (downloadPending || service.running) return;
  downloadPending = true; renderSumatra();
  try { componentStatus = await invoke('download_sumatra'); message('打印组件已下载并校验，可启动真实打印服务。'); }
  finally { downloadPending = false; await refreshSumatra(); }
}, $('download-sumatra')));
$('dismiss-secret').addEventListener('click', hideSecret);
$('device-form').addEventListener('submit', (event) => { event.preventDefault(); action(async () => {
  hideSecret();
  const created = await invoke('create_device', { name: new FormData(event.target).get('name').trim() });
  $('secret-id').value = created.device.id; $('secret-token').value = created.token; $('secret').hidden = false;
  event.target.reset(); await refreshDevices();
}, event.submitter); });
window.addEventListener('pagehide', hideSecret);
setInterval(() => {
  if (!window.__TAURI__ || document.hidden) return;
  action(async () => {
    if (service.running) await refreshStatus();
    if (!$('connections').hidden) await refreshConnections();
    else if (!$('jobs').hidden) await refreshJobs();
  });
}, 2000);
async function initializeApp() {
  await window.__TAURI__.event.listen('hide-secret', hideSecret);
  await window.__TAURI__.event.listen('exit-blocked', async ({ payload }) => { hideSecret(); message(payload, true); await refreshStatus(); });
  await refreshStatus();
  await Promise.all([
    async () => { await refreshPrinters(); await refreshJobs(); },
    refreshDevices, refreshQueues, refreshSumatra,
  ].map((load) => load().catch((error) => message(String(error), true))));
}
if (window.__TAURI__) initializeApp().catch((error) => message(String(error), true)).then(() => {
  const saved = tabs.find((tab) => `#${tab.dataset.tab}` === window.location.hash);
  if (saved) activateTab(saved);
});
else { $('start').disabled = true; message('请从 Windows 桌面应用打开此界面。浏览器预览不连接打印服务。', true); }
