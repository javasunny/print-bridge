# Design: Print Bridge

此文件记录五个桌面管理页签共用的设计规范，样式值以 `desktop/ui/tokens.css` 为准。

## System

- Genre: modern-minimal。
- Shape: app screen，保留五个任务页签。
- Theme: brand，沿用深绿与暖灰纸面，保留 Georgia 字标。
- 服务状态、监听地址与启停操作集中在常驻服务栏。
- 宽窗口中打印机连接信息与配置表单并列，窄窗口中按内容顺序排列。
- 任务和连接诊断用横向表格，内容超宽时只在表格区域滚动。

## Tokens

- `--color-paper`：原有暖灰底色 `#f3f1e9`。
- `--color-accent`：原有品牌绿 `#244c3d`，用于主按钮、选中项和焦点。
- `--color-surface`：输入框及页头表面。
- `--color-muted`：辅助文字，保持小字对比度。
- `--color-field-edge`：输入框边界，和背景至少 3:1。
- `--font-body` / `--font-display`：系统中文无衬线字体。
- `--font-outlier`：Georgia，仅用于字标。
- `--font-code`：Consolas / SFMono-Regular，仅用于连接参数及代码。
- 正文 16px，控件与标签 14px，辅助文字不小于 12px。
- 间距使用 4、8、12、16、24、40、64px 的共享刻度。

## Controls

- 主按钮使用品牌绿，次按钮使用描边，共用 4px 圆角。
- 控件高 40px，按钮命中区域至少 44px；触屏控件至少 44px。
- 键盘焦点立即显示；不靠边框宽度变化表示状态。
- 原生复选框和选择框，页签使用标准角色及键盘行为。
- 不可恢复的清除与凭据撤销须先确认。
- 用户操作等待时显示状态，保留真实后端的成功及错误结果。

## Motion

- 静态工具界面，不添加装饰动效。
- 支持 prefers-reduced-motion。

## Exports

- `desktop/ui/tokens.css` 定义颜色、字体、间距和尺寸。
- `desktop/ui/base.css` 定义基础可访问性与浏览器表面。
- `desktop/ui/style.css` 定义业务布局。
