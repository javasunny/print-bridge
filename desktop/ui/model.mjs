const states = {
  pending: '等待', processing: '处理中', submitted: '已提交',
  failed: '失败', uncertain: '待核查', canceled: '已取消', simulated: '模拟完成',
};
export const stateLabel = (state) => states[state] || state;
export const canCancel = (job) => job.state === 'pending';
export function connectionResult(event) {
  const failed = event.http_status >= 400 || (event.ipp_status != null && event.ipp_status >= 0x0400);
  return { failed, label: failed ? '请求被拒绝' : event.operation === 0x0002 && event.ipp_status != null ? '任务已接收' : '已连接' };
}
export function printerUrl(address, id) {
  if (!address) return null;
  return `ipp://${address}/ipp/printers/${encodeURIComponent(id)}`;
}
export function printerSetup(address, id) {
  if (!address) return null;
  return { address, protocol: 'Internet Printing Protocol - IPP', queue: `ipp/printers/${encodeURIComponent(id)}`, ppd: `http://${address}/ipp/printers/${encodeURIComponent(id)}/driver.ppd` };
}
export function authenticationNotice(service) {
  if (service.authentication_restart_required) return '设置已保存，请重启服务后生效。';
  if (!service.running) return '设置已保存，下次启动服务时生效。';
  return service.active_authentication_required ? '当前服务要求设备认证。' : '当前服务无需认证。';
}
export function printerFromForm(form) {
  return {
    id: String(form.get('id') || '').trim(), name: String(form.get('name') || '').trim(),
    queue: String(form.get('queue') || ''), enabled: form.get('enabled') === 'on',
    duplex: form.get('duplex') === 'on', max_copies: Number(form.get('max_copies')),
    paper_sizes: form.getAll('paper_sizes'), default_paper: String(form.get('default_paper') || 'A4'),
  };
}
