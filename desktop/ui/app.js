import { stateLabel, canCancel, printerUrl, printerFromForm } from './model.mjs';

const $ = (id) => document.getElementById(id);
const invoke = (command, args = {}) => window.__TAURI__.core.invoke(command, args);
let printers = [];
let service = { running: false, address: null };
let busy = false;
let downloadPending = false;

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
async function action(work) {
  if (busy) return;
  busy = true;
  try { await work(); } catch (error) { message(String(error), true); }
  finally { busy = false; }
}
function button(text, handler) {
  const el = node('button', text);
  el.type = 'button';
  el.addEventListener('click', () => action(handler));
  return el;
}
function empty(list, text) { list.replaceChildren(node('div', text, 'empty')); }
function card(title, badge, warning = false) {
  const row = node('article', undefined, 'list-card');
  const body = node('div');
  const heading = node('h3', title);
  if (badge) heading.append(node('span', badge, `badge${warning ? ' warning' : ''}`));
  body.append(heading); row.append(body);
  const actions = node('div', undefined, 'card-actions'); row.append(actions);
  return { row, body, actions };
}
async function refreshStatus() {
  service = await invoke('service_status');
  $('mode').textContent = service.simulation ? '模拟模式 · 不真实打印' : 'Windows 本机';
  $('service-state').textContent = service.running ? 'IPP 服务运行中' : '服务未启动';
  $('service-address').textContent = service.address || '仅在可信局域网使用';
  $('lamp').classList.toggle('running', service.running);
  $('start').disabled = service.running;
  $('stop').disabled = !service.running;
  $('address').disabled = service.running;
  $('authentication-required').checked = service.authentication_required;
  $('authentication-required').disabled = service.running;
  $('save-authentication').disabled = service.running;
  if (service.address) $('address').value = service.address;
}
async function refreshQueues() {
  const available = await invoke('available_printers');
  const selected = $('queue').value;
  $('queue').replaceChildren(new Option('请选择本机队列', ''));
  for (const printer of available) $('queue').add(new Option(printer.name, printer.name));
  $('queue').value = selected;
}
async function refreshPrinters() {
  printers = await invoke('list_printers');
  $('printer-count').textContent = printers.length;
  const list = $('printer-list'); list.replaceChildren();
  const selected = $('filter').value;
  $('filter').replaceChildren(new Option('全部打印机', ''));
  for (const printer of printers) {
    $('filter').add(new Option(printer.name, printer.id));
    const { row, body, actions } = card(printer.name, printer.enabled ? '已启用' : '已停用');
    body.append(node('p', `${printer.queue} · A4 · ${printer.duplex ? '支持双面' : '仅单面'} · 最多 ${printer.max_copies} 份`));
    const url = printerUrl(service.address, printer.id);
    body.append(node('code', url || `启动服务后显示地址 · ${printer.id}`));
    actions.append(button('编辑', async () => {
      const form = $('printer-form');
      for (const key of ['id', 'name', 'max_copies']) form.elements[key].value = printer[key];
      if (![...$('queue').options].some((option) => option.value === printer.queue)) $('queue').add(new Option(`${printer.queue}（当前未枚举到）`, printer.queue));
      form.elements.queue.value = printer.queue;
      form.elements.enabled.checked = printer.enabled;
      form.elements.duplex.checked = printer.duplex;
      form.elements.id.readOnly = true;
      form.elements.name.focus();
    }));
    if (url) actions.append(button('复制地址', async () => {
      await navigator.clipboard.writeText(url); message('IPP 地址已复制。');
    }));
    list.append(row);
  }
  $('filter').value = selected;
  if (!printers.length) empty(list, '还没有共享打印机。先选择一个 Windows 队列。');
}
async function refreshJobs() {
  const jobs = await invoke('list_jobs');
  const list = $('job-list'); list.replaceChildren();
  const filtered = jobs.filter((job) => !$('filter').value || job.printer_id === $('filter').value);
  for (const job of filtered) {
    const printer = printers.find((p) => p.id === job.printer_id);
    const { row, body, actions } = card(`#${job.id}　${printer?.name || job.printer_id}`, stateLabel(job.state), ['uncertain', 'failed'].includes(job.state));
    body.append(node('p', `${job.copies} 份 · ${job.sides} · 设备 ${job.device_id}`));
    if (job.detail) body.append(node('p', job.detail));
    if (job.created_at) body.append(node('p', new Date(job.created_at * 1000).toLocaleString('zh-CN')));
    if (canCancel(job)) actions.append(button('取消任务', async () => { await invoke('cancel_job', { id: job.id }); await refreshJobs(); message('等待任务已取消。'); }));
    list.append(row);
  }
  if (!filtered.length) empty(list, '暂无打印任务。');
}
async function refreshDevices() {
  const devices = await invoke('list_devices');
  const list = $('device-list'); list.replaceChildren();
  for (const device of devices) {
    const { row, body, actions } = card(device.name, device.revoked ? '已撤销' : '可打印');
    body.append(node('code', `用户名：${device.id}`));
    if (!device.revoked) actions.append(button('撤销访问', async () => {
      await invoke('revoke_device', { id: device.id }); await refreshDevices(); message('设备凭据已撤销，后续请求将被拒绝。');
    }));
    list.append(row);
  }
  if (!devices.length) empty(list, service.authentication_required ? '创建设备凭据后，Mac 才能提交任务。' : '认证已关闭，Mac 可以直接提交任务。');
}
for (const tab of document.querySelectorAll('[data-tab]')) tab.addEventListener('click', () => {
  for (const item of document.querySelectorAll('[data-tab]')) {
    const active = item === tab; item.classList.toggle('active', active); item.setAttribute('aria-selected', active);
    $(item.dataset.tab).hidden = !active;
  }
  if (tab.dataset.tab !== 'devices') hideSecret();
  action(tab.dataset.tab === 'jobs' ? refreshJobs : tab.dataset.tab === 'devices' ? refreshDevices : tab.dataset.tab === 'settings' ? refreshSumatra : refreshPrinters);
});
$('printer-form').addEventListener('submit', (event) => { event.preventDefault(); action(async () => {
  await invoke('save_printer', { printer: printerFromForm(new FormData(event.target)) });
  resetPrinter(); await refreshPrinters(); message('打印机配置已保存。');
}); });
function resetPrinter() { $('printer-form').reset(); $('printer-form').elements.id.readOnly = false; }
$('reset-printer').addEventListener('click', resetPrinter);
$('refresh-printers').addEventListener('click', () => action(async () => { await refreshQueues(); await refreshPrinters(); }));
$('refresh-jobs').addEventListener('click', () => action(refreshJobs));
$('filter').addEventListener('change', () => action(refreshJobs));
$('start').addEventListener('click', () => action(async () => {
  await invoke('start_service', { address: $('address').value.trim() }); await refreshStatus(); await refreshPrinters(); message('IPP 服务已启动。');
}));
$('stop').addEventListener('click', () => action(async () => {
  await invoke('stop_service'); await refreshStatus(); await refreshPrinters(); message('服务已停止接收新任务，等待任务已保留。');
}));
$('save-authentication').addEventListener('click', () => action(async () => {
  await invoke('set_authentication_required', { required: $('authentication-required').checked });
  await refreshStatus(); await refreshDevices();
  message(service.authentication_required ? '设备认证已开启。' : '设备认证已关闭，启动服务后可直接打印。');
}));
function hideSecret() { $('secret').hidden = true; $('secret-id').value = ''; $('secret-token').value = ''; }
async function refreshSumatra() {
  const status = await invoke('sumatra_status');
  $('sumatra-version').textContent = status.version;
  $('sumatra-state').textContent = status.downloading || downloadPending ? '正在下载并校验，请稍候…' : status.ready ? '已就绪 · 校验通过' : '尚未安装或校验未通过';
  $('sumatra-path').textContent = status.path;
  $('download-sumatra').disabled = status.ready || status.downloading || downloadPending;
  $('download-sumatra').textContent = status.ready ? '已安装' : status.downloading || downloadPending ? '正在下载…' : '下载打印组件';
}
$('refresh-sumatra').addEventListener('click', () => action(refreshSumatra));
$('download-sumatra').addEventListener('click', async () => {
  if (downloadPending) return;
  downloadPending = true;
  $('download-sumatra').disabled = true;
  $('download-sumatra').textContent = '正在下载…';
  $('sumatra-state').textContent = '正在从官方网站下载并校验，请稍候…';
  try {
    await invoke('download_sumatra');
    message('打印组件已下载并校验，可启动真实打印服务。');
  } catch (error) { message(`下载失败：${String(error)}`, true); }
  finally { downloadPending = false; await refreshSumatra().catch((error) => message(String(error), true)); }
});
$('dismiss-secret').addEventListener('click', hideSecret);
$('device-form').addEventListener('submit', (event) => { event.preventDefault(); action(async () => {
  hideSecret();
  const created = await invoke('create_device', { name: new FormData(event.target).get('name').trim() });
  $('secret-id').value = created.device.id; $('secret-token').value = created.token; $('secret').hidden = false;
  event.target.reset(); await refreshDevices();
}); });
window.addEventListener('pagehide', hideSecret);
if (window.__TAURI__) action(async () => {
  await window.__TAURI__.event.listen('hide-secret', hideSecret);
  await window.__TAURI__.event.listen('exit-blocked', async ({ payload }) => { hideSecret(); message(payload, true); await refreshStatus(); });
  await refreshStatus(); await refreshPrinters(); await refreshDevices(); await refreshJobs(); await refreshQueues(); await refreshSumatra();
});
else { $('start').disabled = true; message('请从 Windows 桌面应用打开此界面。浏览器预览不连接打印服务。', true); }
