import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import * as model from './model.mjs';

class Element {
  textContent = '';
  value = '';
  children = [];
  listeners = {};
  attributes = {};
  dataset = {};
  classList = { toggle() {} };
  addEventListener(type, handler) { this.listeners[type] = handler; }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  removeAttribute(name) { delete this.attributes[name]; }
  focus() { this.focused = true; }
  showModal() {
    this.open = true;
    if (this.confirmChoice !== null) queueMicrotask(() => this.close(this.confirmChoice));
  }
  close(value) { this.returnValue = value; this.open = false; this.listeners.close(); }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  createTHead() { const head = new Element(); this.append(head); return head; }
  createTBody() { const body = new Element(); this.append(body); return body; }
  insertRow() { const row = new Element(); this.append(row); return row; }
  insertCell() { const cell = new Element(); this.append(cell); return cell; }
}

function app(invoke, { confirmChoice = 'confirm' } = {}) {
  const html = readFileSync(new URL('./index.html', import.meta.url), 'utf8');
  const elements = new Map([...html.matchAll(/\bid="([^"]+)"/g)].map(([, id]) => [id, new Element()]));
  const tabs = ['printers', 'jobs', 'devices', 'settings', 'connections'].map((name) => {
    const tab = elements.get(`tab-${name}`);
    tab.dataset.tab = name;
    return tab;
  });
  elements.get('confirm-dialog').confirmChoice = confirmChoice;
  const window = {
    addEventListener() {}, location: { hash: '' },
    history: { replaceState(_state, _title, hash) { window.location.hash = hash; } },
  };
  const context = vm.createContext({
    ...model, window,
    document: {
      getElementById: (id) => {
        assert.ok(elements.has(id), `Missing element: ${id}`);
        return elements.get(id);
      },
      querySelectorAll: (selector) => selector === '[data-tab]' ? tabs : [],
      createElement: () => new Element(),
    },
    setInterval() {},
  });
  const source = readFileSync(new URL('./app.js', import.meta.url), 'utf8');
  vm.runInContext(source.replace(/^import .*?;\n/, ''), context);
  window.__TAURI__ = { core: { invoke } };
  return { context, get: (id) => elements.get(id) };
}

test('clear print history refreshes the filtered list and count while keeping pending tasks', async () => {
  let jobs = [
    { id: 1, state: 'submitted', printer_id: 'office' },
    { id: 2, state: 'pending', printer_id: 'office' },
    { id: 3, state: 'failed', printer_id: 'warehouse' },
  ];
  const commands = [];
  const { context, get } = app(async (command) => {
    commands.push(command);
    if (command === 'list_jobs') return jobs;
    assert.equal(command, 'clear_job_history');
    jobs = jobs.filter((job) => job.state === 'pending');
    return 2;
  });
  get('filter').value = 'office';
  await context.refreshJobs();
  assert.equal(get('job-count').textContent, 2);
  await get('clear-jobs').listeners.click();
  assert.deepEqual(commands, ['list_jobs', 'clear_job_history', 'list_jobs']);
  assert.equal(get('job-count').textContent, 1);
  assert.equal(get('filter').value, 'office');
  assert.match(get('message').textContent, /已清除 2 条打印记录/);
  assert.match(get('message').textContent, /待核查任务已保留/);
});

test('clear diagnostics renders the empty list and later refreshes show new requests', async () => {
  const event = { timestamp: 1, method: 'POST', path: '/ipp/printers/office', operation: 11, http_status: 200, ipp_status: 0 };
  let events = [event];
  const { context, get } = app(async (command) => {
    if (command === 'list_connection_events') return events;
    assert.equal(command, 'clear_connection_events');
    const count = events.length;
    events = [];
    return count;
  });
  await context.refreshConnections();
  assert.equal(get('connection-count').textContent, 1);
  await get('clear-connections').listeners.click();
  assert.equal(get('connection-count').textContent, 0);
  assert.match(get('connection-list').children[0].textContent, /尚未收到请求/);
  assert.match(get('message').textContent, /已清除 1 条诊断记录/);
  events = [{ ...event, timestamp: 2 }];
  await context.refreshConnections();
  assert.equal(get('connection-count').textContent, 1);
  await get('clear-connections').listeners.click();
  await get('clear-connections').listeners.click();
  assert.match(get('message').textContent, /已清除 0 条诊断记录/);
});

test('clear failures report the error and leave displayed records intact', async () => {
  const { context, get } = app(async (command) => {
    if (command === 'list_jobs') return [{ id: 1, state: 'submitted', printer_id: 'office' }];
    throw new Error('清除失败');
  });
  await context.refreshJobs();
  const table = get('job-list').children[0];
  await get('clear-jobs').listeners.click();
  assert.equal(get('message').className, 'error');
  assert.match(get('message').textContent, /清除失败/);
  assert.equal(get('job-count').textContent, 1);
  assert.equal(get('job-list').children[0], table);
});

test('clearing records waits for confirmation and cancel never calls the backend', async () => {
  const commands = [];
  const { get } = app(async (command) => { commands.push(command); }, { confirmChoice: null });
  for (const id of ['clear-jobs', 'clear-connections']) {
    const pending = get(id).listeners.click();
    assert.equal(get('confirm-dialog').open, true);
    assert.equal(get(id).attributes['aria-busy'], 'true');
    assert.equal(get('operation-state').hidden, false);
    assert.deepEqual(commands, []);
    if (id === 'clear-jobs') assert.match(get('confirm-description').textContent, /全部打印机.*不受当前筛选影响/);
    get('confirm-dialog').close('cancel');
    await pending;
    assert.deepEqual(commands, []);
    assert.equal(get('operation-state').hidden, true);
    assert.equal(get(id).attributes['aria-busy'], undefined);
  }
});

test('device revocation names the device and requires explicit confirmation', async () => {
  const commands = [];
  const { context, get } = app(async (command) => {
    commands.push(command);
    return command === 'list_devices' ? [{ id: 'mac', name: '办公室 Mac' }] : undefined;
  }, { confirmChoice: null });
  await context.refreshDevices();
  const revoke = get('device-list').children[0].children[1];
  const canceled = revoke.listeners.click();
  assert.match(get('confirm-description').textContent, /办公室 Mac.*立即失效/);
  get('confirm-dialog').close('');
  await canceled;
  assert.deepEqual(commands, ['list_devices']);
  const confirmed = revoke.listeners.click();
  get('confirm-dialog').close('confirm');
  await confirmed;
  assert.deepEqual(commands, ['list_devices', 'revoke_device', 'list_devices']);
});

test('arrow keys wrap tabs and keep selection, panel visibility and URL aligned', async () => {
  const { context, get } = app(async (command) => {
    assert.equal(command, 'list_connection_events');
    return [];
  });
  let prevented = false;
  get('tab-printers').listeners.keydown({ key: 'ArrowLeft', preventDefault() { prevented = true; } });
  await Promise.resolve();
  assert.equal(prevented, true);
  assert.equal(get('tab-connections').focused, true);
  assert.equal(get('tab-connections').attributes['aria-selected'], 'true');
  assert.equal(get('tab-connections').tabIndex, 0);
  assert.equal(get('tab-printers').tabIndex, -1);
  assert.equal(get('connections').hidden, false);
  assert.equal(get('printers').hidden, true);
  assert.equal(context.window.location.hash, '#connections');
});

test('busy feedback prevents duplicate actions and clears after backend failure', async () => {
  let reject;
  let calls = 0;
  const { context, get } = app(() => {});
  const control = get('refresh-jobs');
  const pending = context.action(() => { calls++; return new Promise((_resolve, fail) => { reject = fail; }); }, control);
  await context.action(() => { calls++; }, control);
  assert.equal(calls, 1);
  assert.equal(control.attributes['aria-busy'], 'true');
  reject(new Error('连接失败，请重试'));
  await pending;
  assert.equal(get('message').className, 'error');
  assert.match(get('message').textContent, /连接失败/);
  assert.equal(control.attributes['aria-busy'], undefined);
  assert.equal(get('operation-state').hidden, true);
});
