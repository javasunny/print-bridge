import test from 'node:test';
import assert from 'node:assert/strict';
import { canCancel, printerUrl, printerFromForm, stateLabel, connectionResult } from './model.mjs';

test('only pending jobs expose cancellation', () => {
  assert.equal(canCancel({ state: 'pending' }), true);
  for (const state of ['processing', 'submitted', 'uncertain', 'failed', 'canceled', 'simulated']) {
    assert.equal(canCancel({ state }), false);
  }
});
test('IPP addresses do not contain credentials and encode bridge identifiers', () => {
  assert.equal(printerUrl(null, 'office'), null);
  assert.equal(printerUrl('192.168.1.8:8631', 'office'), 'ipp://192.168.1.8:8631/ipp/printers/office');
  assert.equal(printerUrl('[::1]:8631', 'front desk'), 'ipp://[::1]:8631/ipp/printers/front%20desk');
});
test('unchecked capability does not become enabled through a string cast', () => {
  const form = new Map([['id', ' office '], ['name', ' Office '], ['queue', 'Exact Queue'], ['max_copies', '2'], ['enabled', 'on']]);
  assert.deepEqual(printerFromForm(form), { id: 'office', name: 'Office', queue: 'Exact Queue', max_copies: 2, enabled: true, duplex: false });
});
test('submitted state never claims physical completion', () => {
  assert.equal(stateLabel('submitted'), '已提交');
  assert.equal(stateLabel('uncertain'), '待核查');
});
test('connection diagnostics distinguish capability probes, accepted jobs and IPP errors within HTTP 200', () => {
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0, operation: 11 }), { failed: false, label: '已连接' });
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0, operation: 2 }), { failed: false, label: '任务已接收' });
  assert.deepEqual(connectionResult({ http_status: 200, ipp_status: 0x040b, operation: 2 }), { failed: true, label: '请求被拒绝' });
  assert.deepEqual(connectionResult({ http_status: 401, ipp_status: null, operation: null }), { failed: true, label: '请求被拒绝' });
});
