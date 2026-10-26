import test from 'node:test';
import assert from 'node:assert/strict';
import { canCancel, printerUrl, printerSetup, printerFromForm, stateLabel, connectionResult, authenticationNotice, discoveryNotice } from './model.mjs';

test('queued jobs and incomplete document uploads expose cancellation', () => {
  for (const state of ['pending', 'pending-document', 'pending-final-document']) assert.equal(canCancel({ state }), true);
  for (const state of ['processing', 'submitted', 'uncertain', 'failed', 'canceled', 'simulated']) {
    assert.equal(canCancel({ state }), false);
  }
});
test('IPP addresses do not contain credentials and encode bridge identifiers', () => {
  assert.equal(printerUrl(null, 'office'), null);
  assert.equal(printerUrl('192.168.1.8:8631', 'office'), 'ipp://192.168.1.8:8631/ipp/printers/office');
  assert.equal(printerUrl('[::1]:8631', 'front desk'), 'ipp://[::1]:8631/ipp/printers/front%20desk');
});
test('macOS setup fields split host, protocol and queue without duplicating the scheme', () => {
  assert.equal(printerSetup('', 'office'), null);
  assert.deepEqual(printerSetup('192.168.8.159:8631', 'front desk'), {
    address: '192.168.8.159:8631', protocol: 'Internet Printing Protocol - IPP', queue: 'ipp/printers/front%20desk', ppd: 'http://192.168.8.159:8631/ipp/printers/front%20desk/driver.ppd',
  });
});
test('authentication notice distinguishes pending configuration from the running policy', () => {
  assert.match(authenticationNotice({ running: true, authentication_restart_required: true, active_authentication_required: true }), /重启服务/);
  assert.match(authenticationNotice({ running: false, authentication_restart_required: false }), /下次启动/);
  assert.match(authenticationNotice({ running: true, active_authentication_required: false }), /无需认证/);
  assert.match(authenticationNotice({ running: true, active_authentication_required: true }), /要求设备认证/);
});
test('unchecked capability does not become enabled through a string cast', () => {
  const form = new FormData();
  for (const [key, value] of [['id', ' office '], ['name', ' Office '], ['queue', 'Exact Queue'], ['max_copies', '2'], ['enabled', 'on'], ['paper_sizes', 'A4'], ['default_paper', 'A4']]) form.append(key, value);
  assert.deepEqual(printerFromForm(form), { id: 'office', name: 'Office', queue: 'Exact Queue', max_copies: 2, enabled: true, duplex: false, paper_sizes: ['A4'], default_paper: 'A4' });
});
test('submitted state never claims physical completion', () => {
  assert.equal(stateLabel('submitted'), '已提交');
  assert.equal(stateLabel('uncertain'), '待核查');
});
test('connection diagnostics distinguish capability probes, accepted jobs and IPP errors within HTTP 200', () => {
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0, operation: 11 }), { failed: false, label: '已连接' });
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0, operation: 2 }), { failed: false, label: '文档已接收' });
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0, operation: 5 }), { failed: false, label: '等待文档' });
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0, operation: 6 }), { failed: false, label: '文档已接收' });
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0x040b, operation: 2 }), { failed: true, label: '请求被拒绝' });
  assert.deepEqual(connectionResult({ http_status: 401, ipp_status: null, operation: null }), { failed: true, label: '请求被拒绝' });
});

test('discovery notice distinguishes publication failure and stopped service', () => {
  assert.match(discoveryNotice({ running: false }), /启动服务后/);
  assert.match(discoveryNotice({ running: true, discovery_active: true }), /已启用/);
  assert.match(discoveryNotice({ running: true, discovery_active: false }), /未发布/);
  const failed = discoveryNotice({ running: true, discovery_active: true, discovery_error: 'multicast failed' });
  assert.match(failed, /multicast failed/);
  assert.match(failed, /仍可通过 IPP/);
  assert.equal(stateLabel('pending-document'), '等待文档');
});

test('printer form preserves multiple paper sizes and default selection', () => {
  const form = new FormData();
  form.append('paper_sizes', 'A4');
  form.append('paper_sizes', 'Letter');
  form.append('default_paper', 'Letter');
  form.append('duplex', 'on');
  const printer = printerFromForm(form);
  assert.deepEqual(printer.paper_sizes, ['A4', 'Letter']);
  assert.equal(printer.default_paper, 'Letter');
  assert.equal(printer.duplex, true);
});
