import test from 'node:test';
import assert from 'node:assert/strict';
import { canCancel, printerUrl, printerFromForm, stateLabel } from './model.mjs';

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
