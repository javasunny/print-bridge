const states = {
  pending: '等待', 'pending-document': '等待文档', 'pending-final-document': '等待结束上传', processing: '处理中', submitted: '已提交',
  failed: '失败', uncertain: '待核查', canceled: '已取消', simulated: '模拟完成', completed: '完成（无文档）',
};
export const stateLabel = (state) => states[state] || state;
export const canCancel = (job) => ['pending', 'pending-document', 'pending-final-document'].includes(job.state);
export function connectionResult(event) {
  const failed = event.http_status >= 400 || (event.ipp_status != null && event.ipp_status >= 0x0400);
  return { failed, label: failed ? '请求被拒绝' : [0x0002, 0x0006].includes(event.operation) && event.ipp_status != null ? '文档已接收' : event.operation === 0x0005 && event.ipp_status != null ? '等待文档' : '已连接' };
}
export function discoveryNotice(service) {
  if (!service.running) return '启动服务后，同一局域网的 Mac、iPhone 和 iPad 可自动发现共享打印机。';
  if (service.discovery_error) return `自动发现不可用：${service.discovery_error}。仍可通过 IPP 地址连接。`;
  return service.discovery_active ? 'AirPrint 自动发现已启用；设备需在同一局域网，并允许 UDP 5353 和打印服务端口。' : '当前未发布自动发现服务；请使用局域网监听地址并启用共享队列。';
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
