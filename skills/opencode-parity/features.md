# 功能台账

基线: v2.2.3

状态：`aligned` 已对齐；`partial` 部分对齐；`missing` ox 没有；`deviation` 有意偏差（备注必须写原因）；`n/a` 不适用（备注必须写原因）；`todo` 刚登记，待核对。

| kind | id | 上游 | ox | 状态 | 备注 |
|---|---|---|---|---|---|
| behavior | block-tool | routes/session/index.tsx:2883 | crates/ox-tui/src/ui/block_tool.rs:68 | partial | ┃ 轨、上下空行、内容缩进 2 列、整块同色带,与用户消息共用 frame。轨与色带同色故不可见(2.2.5 的 borderColor 是透明 background)。偏差:hover 用 palette.panel;未移植 path 标题截断、headerColor/errorColor、权限色;无动画时不显示 ⋯ |
| behavior | completion-notice | routes/session/index.tsx:2090 | crates/ox-tui/src/pipeline.rs:58 | partial | `↳ Shell/Subagent finished · ...` 历史与实时两路都做了,子代理行可点击跳转。偏差:标题固定为 Subagent(ox 元数据无 agent 名);hover 只提亮标题 |
| behavior | diff-block | routes/session/index.tsx:3439 | crates/ox-tui/src/ui/diff.rs:208 | partial | unified/split（按 width-4 选视图）、行号 gutter（pad,号,符号,pad）、hunk 行、12 个 diff 主题色、解析失败回退原文。偏差见 diff.rs:20-41:无语法高亮、无词级高亮、无 diffwrap 切换 |
| behavior | edit-block | routes/session/index.tsx:3439 | crates/ox-tui/src/ui/edit_block.rs:38 | partial | `← Edit path` BlockTool + diff。偏差:非成功态仍为单行 tool_text;无 diagnostics(服务端无);不可展开点击 |
| behavior | explored-expand | routes/session/group-view.tsx:152 | crates/ox-tui/src/ui/timeline.rs:432 | aligned | 点击标题行展开/折叠,状态按 group id 保存 |
| behavior | explored-group | routes/session/group-view.tsx:115 | crates/ox-tui/src/pipeline.rs:1 | partial | 运行中 spinner `Exploring:`、按首次出现计数、展开子项用专用文案。偏差:无 completePrevious/partitionPending;子项不可点击 |
| behavior | family-busy | context/session-tabs.tsx:170 | crates/ox-client/src/store.rs:365 | aligned | 自身回合或任一子会话在跑都算忙;Esc/Ctrl+C 仍只作用于当前会话 |
| behavior | hover-highlight | routes/session/group-view.tsx:150 | crates/ox-tui/src/ui/timeline.rs:475 | partial | Explored/Thought/用户消息已做;Ansi16 与无色主题下用户块悬停背景不可见;Tokens、Instructions 行未核对 |
| behavior | inline-tool-row | routes/session/index.tsx:2789 | crates/ox-tui/src/ui/tool_row.rs:48 | partial | spinner/pending 文案/完成 muted/失败 error 色+点击展开错误已做;失败行无 hover;denied 删除线未移植;通用行未按宽度截断 |
| behavior | mouse-select-copy | util/selection.ts:34 | crates/ox-tui/src/ui/selection.rs:167 | deviation | 拖选/双击/三击/自动复制/toast 已做。偏差:软换行处复制带 \\n;仅转录区可选;拖出边缘不自动滚动;无右键复制;双击三击按显示行而非源行 |
| behavior | paste-attachment | component/prompt/local-attachment.ts:1 | crates/ox-tui/src/attachment.rs:37 | deviation | 图片/PDF 路径与 file:// 成为附件,上限 32 个 / 20MB;附件不写入历史,提交失败不恢复文本;图片附件的预览与放大见 prompt-image-strip / transcript-images 两行 |
| behavior | paste-fold | component/prompt/index.tsx:1447 | crates/ox-tui/src/ui/paste.rs:88 | deviation | 3 行以上或超 150 字符折叠为 [Pasted ~N lines];占位符是普通文本(tui-textarea 无 extmark),无点击展开 |
| behavior | patch-block | routes/session/index.tsx:3503 | crates/ox-tui/src/ui/patch_block.rs:44 | partial | 每文件一个 diff 块,patch/apply_patch 两名都认,失败态也画块。偏差:无 metadata.applied、无 move 源路径、失败无 metadata(服务端缺口) |
| behavior | prompt-surface | component/prompt/index.tsx:1610 | crates/ox-tui/src/ui/prompt.rs:765 | partial | 左边框轨与 `╹` 脚用 agent 色(2.2.5 实测 #b294bb,home 与根会话一致;无 agent 色时退 border.base),底色透明、`▀` 影线与上游同形。偏差:ox 只有一个 agent 色,不做上游按 agent 序号轮转的分类色 |
| behavior | prompt-footer-live | feature-plugins/prompt/footer.tsx:24 | crates/ox-tui/src/ui/prompt.rs:187 | deviation | 会话 footer 的活胶囊 `↓ N subagent · N shell`:位置(右段首位)、顺序、配色(`↓` 与 `ctrl+p` 用 text.base,计数与 ` · ` 用 muted)、` · ` 分隔、与 hint 间两空格都与上游 footer.tsx:73-92 同形;不受 showDetails 约束、不计入 promptFooterLayout 预算(footer.tsx:156-166,实现 crates/ox-tui/src/ui/prompt.rs:812-818);计数为 0 时整段消失(无 shell 会话实测只剩 `ctrl+p commands`);hover 时计数转 text.base(prompt.rs:191,live_spans 在 187),点击派发 session.child.first。2.2.5 30×100 端到端实测:footer 行与上游逐格一致(0 格差异,两侧 `↓` 73 列、`1 shell` 75-81 列 muted、`ctrl+p` 84-89 列 text、`commands` 91-98 列 muted)。偏差:(a) subagent 只数被 transcript tool part 命名的子会话(crates/ox-client/src/store.rs:752 running_subagents),上游数常驻 family 中 status 为 running 的成员(footer.tsx:24-29);(b) shell 只数 running(crates/ox-client/src/store.rs:737 running_shells_of,该结构只含 running),等价上游 Shell 标签页的计数而非 footer 的计数(footer.tsx:30-36 数 list() 里全部) |
| behavior | activity-panel | routes/session/composer/index.tsx:91 | crates/ox-tui/src/ui/activity_panel.rs:460 | partial | Subagents/Shell 标签面板占据输入框位置,标签头、行、`Running`、`esc`、footer 提示对齐。偏差:面板底色用 raised_high(上游 `background.raised.base`,实测 2.2.5 为 #25364d、ox 为 #2a3d58);当前行底色用 raised_base(上游 `background.action.primary.base/selected/focused`);仅按键打开,上游对 `session.parentID` 且无表单的会话自动打开(2.2.5 实测三个子会话均自动显示、根会话显示输入框) |
| behavior | prompt-image-strip | component/prompt/index.tsx:1701 | crates/ox-tui/src/ui/prompt.rs:1003 | deviation | 输入框上方图片预览条:最多 3 个缩略图 + `+N more`(格宽 = 格高×2、gap 1、高 max(4,min(8,终端高/4))、上下各 1 空行),点缩略图从该图打开放大对话框,点 `+N more` 从第一个没画出来的图打开(对齐上游 onMouseUp,index.tsx:1741-1766)。偏差:(a) 始终显示,上游受 `prompt.image_preview` 开关控制(用户 2026-11-04 决定 ox 不读该开关);(b) 终端画不了图时整条不占位;(c) 放不下整格的缩略图直接跳过(kitty 占位格无法裁切),`+N more` 也只在放得下时画;(d) 条内没有逐格 `No preview` 兜底(上游 index.tsx:1729-1735),解码失败的格子留空并由对话框报 `No preview` |
| behavior | transcript-images | routes/session/index.tsx:2656 | crates/ox-tui/src/ui/media.rs:1 | deviation | 用户消息的附件与工具结果里的图片在转录中画成缩略图条(最多 3 个 + `+N more`,几何同上游 SessionImages,`file`/`dir` 标签行),点开即放大对话框,可从该图起 ←/→ 翻到没画出来的那几张;kitty 用 `f=100` 透传 PNG + 占位格交给终端缩放,sixel 交 ratatui-image 按格编码。偏差:(a) 始终显示,无 `session.image_preview` 开关;(b) 无半格 blocks 兜底、不做 iTerm2,不支持的终端不预留行;(c) 取字节走 ox-only `GET /ox/session/{id}/media/{mid}?index=N`(crates/ox-server/src/api/media.rs:52),上游用 `media:<hash>` 资产库;(d) 上游缩略图 `fit="cover"`(裁切填满),ox kitty 由终端缩放(拉伸填满)、sixel 用 Fit(留边);(e) JPEG/GIF/WebP 在客户端一次性重编码为 PNG 再透传(crates/ox-tui/src/graphics/png.rs:28),上游资产库本来就只存一份 blob;(f) 乐观回显的首条消息没有缩略图(本地没有 message id,确认后历史重拉补上);(g) `mention` 恒为 None,不做上游按 mention 去重 |
| behavior | queued-dock | routes/session/index.tsx:225 | crates/ox-tui/src/ui/dock.rs:98 | partial | 输入框上方逐条列出排队提示词,<leader>q 有排队时开对话框、无则退出(keymap.tsx:231)。偏差见 dock.rs/queued_dialog.rs 模块文档 |
| behavior | question-block | routes/session/index.tsx:3651 | crates/ox-tui/src/ui/question_block.rs:1 | partial | `# Questions` BlockTool + 问答列表;服务端在 metadata.answers 回传答案(ox-tools/src/question.rs)。偏差见 question_block.rs 模块文档 |
| behavior | reasoning-markdown | routes/session/message-parts.tsx:59 | crates/ox-tui/src/ui/markdown.rs:57 | partial | 展开的思考正文按 muted markdown 渲染,`   ┃ ` 前缀、宽度 width-5(markdown::Tone::Muted) |
| behavior | session-rows | routes/session/index.tsx:2022 | crates/ox-tui/src/ui/timeline.rs:1604 | partial | Error 挂在消息下;Retry 倒计时;agent/model/location 切换行;Instructions updated;Skill;Compaction queued;压缩 spinner。偏差见 store.rs/pipeline.rs 模块文档 |
| behavior | shell-background-output | routes/session/index.tsx:3015 | crates/ox-tui/src/shell_captures.rs:114 | partial | 展开时每秒拉取一页、在飞不重发,退出后 drain 一次,超 1 MiB 保留尾部并显示 [earlier output omitted]。偏差:复用 33ms UI tick 节流;折叠块不轮询;按原始字节计限,绘制时去 ANSI;失败不立即重试;后台调用无输出前不显示结果文本;失败调用只显示错误 |
| behavior | shell-block | routes/session/index.tsx:2969 | crates/ox-tui/src/ui/shell_block.rs:173 | partial | BlockTool 框、`$ [cd wd && ]cmd`、运行 spinner、Writing command…、折叠 10 行带 (N earlier lines)、点击/o 展开、Background 徽标、失败 error 已做(接线 ui/timeline.rs:1467)。偏差:有 shellID 的调用折叠时只显示命令、展开才显示输出(对齐 2.2.5 的 `output={metadata.shellID ? undefined : output}`);Background 仅在 shell 仍存活时显示;无部分输入解析;hover 用 palette.panel;无权限色;[ ] 焦点画 hover 带;spinner 只改缓存行的 spinner span |
| behavior | shell-message | routes/session/index.tsx:2322 |  | missing | 服务端已产生 Message::Shell(session.shell,crates/ox-core/src/projector/message_updater.rs 投影,60cdfa5);ox-tui 未渲染历史 shell 行,也没有 ! 输入模式 |
| behavior | sidebar-todo | routes/session/sidebar.tsx:1 | crates/ox-tui/src/ui/sidebar.rs:68 | partial | 42 列侧栏,宽>120 停靠右侧、否则浮层(Esc 先关),子会话不显示;Todo 节 ▼/▶ 折叠、(N remaining)、取最后一次成功 todowrite。偏差见 sidebar.rs 模块文档:无 revert 边界、不持久化、无滚动、只有 Todo 节、浮层无暗化背景 |
| behavior | subagent-background-badge | routes/session/index.tsx:3289 | crates/ox-tui/src/ui/timeline.rs:1626 | aligned | 子会话运行时子代理工具行显示转圈和 Background |
| behavior | tool-inline-texts | routes/session/index.tsx:3143 | crates/ox-tui/src/ui/tool_text.rs:93 | deviation | Read/Glob/Grep/WebFetch/Skill/Subagent/Question/Generic 文案已移植。偏差:Generic 键序按字母;子代理模型显示原始 provider/model,无 agent 名时为 General;展开体限 TOOL_OUTPUT_MAX_LINES |
| behavior | write-block | routes/session/index.tsx:3143 | crates/ox-tui/src/ui/write_block.rs:40 | partial | `# Wrote path` + 带行号全文。偏差:无语法高亮;无 diagnostics;超宽单字形独占一行 |
| dialog | dialog-agent | component/dialog-agent.tsx:1 |  | todo |  |
| dialog | dialog-alert | ui/dialog-alert.tsx:1 |  | todo |  |
| dialog | dialog-config | component/dialog-config.tsx:1 |  | todo |  |
| dialog | dialog-confirm | ui/dialog-confirm.tsx:1 |  | todo |  |
| dialog | dialog-debug | component/dialog-debug.tsx:1 |  | todo |  |
| dialog | dialog-error-details | component/dialog-error-details.tsx:1 |  | todo |  |
| dialog | dialog-execute | routes/session/dialog-execute.tsx:1 |  | todo |  |
| dialog | dialog-experiments | component/dialog-experiments.tsx:1 |  | todo |  |
| dialog | dialog-export-options | ui/dialog-export-options.tsx:1 |  | todo |  |
| dialog | dialog-export-result | ui/dialog-export-result.tsx:1 |  | todo |  |
| dialog | dialog-fork | routes/session/dialog-fork.tsx:1 |  | todo |  |
| dialog | dialog-go-upsell | component/dialog-go-upsell.tsx:1 |  | n/a | 上游商业推广弹窗(待用户确认) |
| dialog | dialog-help | ui/dialog-help.tsx:1 |  | todo |  |
| dialog | dialog-highlights | component/dialog-highlights.tsx:1 |  | todo |  |
| dialog | dialog-image-preview | component/dialog-image-preview.tsx:13 | crates/ox-tui/src/ui/image_preview.rs:58 | partial | 放大对话框:`Image N of M` 标题、`← previous` / `next →`(仅多于一张)、Esc 关闭、页脚文件名,等比缩放(高 = 终端高 − 8,最少 3),面板 116 列宽、8 行 chrome 居中。偏差:对话框打开期间 ox 只认键盘(pointer_enabled 为假),页脚箭头是标签不是可点按钮;`No preview` 要等该图的取字节失败才出现;不做半格 blocks 兜底与 iTerm2 协议(用户 2026-11-04 决定只做真像素) |
| dialog | dialog-integration | component/dialog-integration.tsx:1 |  | todo |  |
| dialog | dialog-mcp | component/dialog-mcp.tsx:1 | crates/ox-tui/src/ui/mcp_dialog.rs:49 | deviation | /mcps 对话框:按名排序、状态标签、space 连接/断开、失败行 enter 看错误详情(c 复制、i 新会话起草排查);接线 crates/ox-tui/src/app/mod.rs:409-485(feat/tui-mcps)。偏差:D1 无 needs_auth/OAuth(ox 无 MCP OAuth);D2 无会话的 Home 收不到 mcp.status.changed;D3 搜索占位符用 Search;D4 ✓ copied 在发起复制时显示;D5 错误详情无 source/diagnosticRef;D8 mcp.status.changed 每帧一次 GET /api/mcp 不合并 |
| dialog | dialog-message | routes/session/dialog-message.tsx:1 |  | todo |  |
| dialog | dialog-model | component/dialog-model.tsx:1 | crates/ox-tui/src/ui/models.rs:99 | todo | ox 有对应对话框,字段与交互未逐项核对 |
| dialog | dialog-model-free | component/dialog-model-free.tsx:1 |  | todo |  |
| dialog | dialog-open | component/dialog-open.tsx:1 |  | todo |  |
| dialog | dialog-pair | component/dialog-pair.tsx:1 |  | todo |  |
| dialog | dialog-prompt | ui/dialog-prompt.tsx:1 |  | todo |  |
| dialog | dialog-select | ui/dialog-select.tsx:1 |  | todo |  |
| dialog | dialog-session-list | component/dialog-session-list.tsx:1 | crates/ox-tui/src/ui/session_dialog.rs:63 | todo | ox 有对应对话框,字段与交互未逐项核对 |
| dialog | dialog-session-rename | component/dialog-session-rename.tsx:1 | crates/ox-tui/src/ui/session_dialog.rs:97 | todo | ox 有对应对话框,字段与交互未逐项核对 |
| dialog | dialog-shell-output | component/dialog-shell-output.tsx:1 | crates/ox-tui/src/ui/output_dialog.rs:45 | deviation | 显示命令、状态、最近 64 KiB 输出(去 ANSI),↑↓/PageUp/PageDown/Home/End 滚动;不轮询,ctrl+r 刷新 |
| dialog | dialog-skill | component/dialog-skill.tsx:1 |  | todo |  |
| dialog | dialog-stash | component/dialog-stash.tsx:1 |  | todo |  |
| dialog | dialog-status | component/dialog-status.tsx:1 |  | todo |  |
| dialog | dialog-theme-list | component/dialog-theme-list.tsx:1 |  | todo |  |
| dialog | dialog-timeline | routes/session/dialog-timeline.tsx:1 | crates/ox-tui/src/ui/timeline_dialog.rs:15 | todo | ox 有对应对话框,字段与交互未逐项核对 |
| dialog | dialog-update | component/dialog-update.tsx:1 |  | todo |  |
| dialog | dialog-variant | component/dialog-variant.tsx:1 |  | todo |  |
| dialog | dialog-workspace-file-changes | component/dialog-workspace-file-changes.tsx:1 |  | todo |  |
| dialog | dialog-workspaces | component/dialog-workspaces.tsx:1 |  | todo |  |
| dialog | dialog-worktree-name | component/dialog-worktree-name.tsx:1 |  | todo |  |
| keybind | agent.cycle | config/keybind.ts:176 |  | todo |  |
| keybind | agent.cycle.reverse | config/keybind.ts:177 |  | todo |  |
| keybind | agent.list | config/keybind.ts:175 |  | todo |  |
| keybind | app.clear | config/keybind.ts:49 |  | todo |  |
| keybind | app.console | config/keybind.ts:51 |  | todo |  |
| keybind | app.debug | config/keybind.ts:50 |  | todo |  |
| keybind | app.exit | config/keybind.ts:48 |  | todo |  |
| keybind | app.scrap | config/keybind.ts:52 |  | todo |  |
| keybind | app.toggle.animations | config/keybind.ts:53 |  | todo |  |
| keybind | app.toggle.diffwrap | config/keybind.ts:55 |  | todo |  |
| keybind | app.toggle.file_context | config/keybind.ts:54 |  | todo |  |
| keybind | app.toggle.paste_summary | config/keybind.ts:56 |  | todo |  |
| keybind | command.palette.show | config/keybind.ts:57 |  | todo |  |
| keybind | composer.shell.down | config/keybind.ts:254 | crates/ox-tui/src/ui/shell_dialog.rs:20 | deviation | 偏差:用 Select 对话框而非 composer 面板;入口 ctrl+x b 或 /shells(上游无独立键) |
| keybind | composer.shell.kill | config/keybind.ts:256 | crates/ox-tui/src/ui/shell_dialog.rs:20 | deviation | 偏差:对话框形态;ctrl+d 杀掉后重新拉取列表,光标回到顶部;无点击和页脚 M shells 提示 |
| keybind | composer.shell.select | config/keybind.ts:255 | crates/ox-tui/src/ui/shell_dialog.rs:20 | deviation | 偏差:Enter 打开只读输出查看器(output_dialog.rs:45),不轮询,ctrl+r 手动刷新(上游每秒轮询) |
| keybind | composer.shell.up | config/keybind.ts:253 | crates/ox-tui/src/ui/shell_dialog.rs:20 | deviation | 偏差:用 Select 对话框而非 composer 面板;入口 ctrl+x b 或 /shells(上游无独立键) |
| keybind | composer.subagent.down | config/keybind.ts:250 | crates/ox-tui/src/ui/activity_panel.rs:227 | deviation | 偏差:底部 composer 面板(上游同形);↑/↓ 在列出的行内移动,列表变化后夹紧;入口 /subagents 或裸 ↓(历史走完后 fallthrough,见 session.child.first 行);无点击命中(上游 click 导航) |
| keybind | composer.subagent.interrupt | config/keybind.ts:252 | crates/ox-tui/src/ui/activity_panel.rs:149 | deviation | 偏差:ctrl+d 只对运行中的行有效,面板保持打开;ctrl+a 切换显示已结束;无点击命中测试 |
| keybind | composer.subagent.select | config/keybind.ts:251 | crates/ox-tui/src/ui/activity_panel.rs:227 | deviation | 偏差:Enter 跳到子会话(App::switch_session);列表来自 GET /ox/session/{id}/family(ox 专用端点);只在根会话可开;上游是 mouseUp 导航 |
| keybind | composer.subagent.up | config/keybind.ts:249 | crates/ox-tui/src/ui/activity_panel.rs:227 | deviation | 偏差:底部 composer 面板;↑/↓ 在列出的行内移动,Esc 关闭 |
| keybind | composer.terminal.down | config/keybind.ts:258 |  | todo |  |
| keybind | composer.terminal.select | config/keybind.ts:259 |  | todo |  |
| keybind | composer.terminal.up | config/keybind.ts:257 |  | todo |  |
| keybind | dialog.integration.delete | config/keybind.ts:270 |  | todo |  |
| keybind | dialog.integration.rename | config/keybind.ts:269 |  | todo |  |
| keybind | dialog.mcp.toggle | config/keybind.ts:283 | crates/ox-tui/src/ui/mcp_dialog.rs:135 | deviation | 默认 space 同上游。偏差 D7:ox 无 keybind 配置,不可改键 |
| keybind | dialog.move_session.delete | config/keybind.ts:274 |  | todo |  |
| keybind | dialog.move_session.move | config/keybind.ts:273 |  | todo |  |
| keybind | dialog.move_session.new | config/keybind.ts:272 |  | todo |  |
| keybind | dialog.move_session.refresh | config/keybind.ts:275 |  | todo |  |
| keybind | dialog.plugins.check | config/keybind.ts:286 |  | todo |  |
| keybind | dialog.plugins.error | config/keybind.ts:284 |  | todo |  |
| keybind | dialog.plugins.update | config/keybind.ts:285 |  | todo |  |
| keybind | dialog.prompt.submit | config/keybind.ts:268 |  | todo |  |
| keybind | dialog.select.end | config/keybind.ts:266 |  | todo |  |
| keybind | dialog.select.home | config/keybind.ts:265 |  | todo |  |
| keybind | dialog.select.next | config/keybind.ts:262 |  | todo |  |
| keybind | dialog.select.page_down | config/keybind.ts:264 |  | todo |  |
| keybind | dialog.select.page_up | config/keybind.ts:263 |  | todo |  |
| keybind | dialog.select.prev | config/keybind.ts:261 |  | todo |  |
| keybind | dialog.select.submit | config/keybind.ts:267 |  | todo |  |
| keybind | dialog.worktree.generate | config/keybind.ts:271 |  | todo |  |
| keybind | diff.close | config/keybind.ts:65 |  | todo |  |
| keybind | diff.collapse | config/keybind.ts:78 |  | todo |  |
| keybind | diff.down | config/keybind.ts:66 |  | todo |  |
| keybind | diff.expand | config/keybind.ts:76 |  | todo |  |
| keybind | diff.expand_all | config/keybind.ts:77 |  | todo |  |
| keybind | diff.first | config/keybind.ts:72 |  | todo |  |
| keybind | diff.half_page.down | config/keybind.ts:70 |  | todo |  |
| keybind | diff.half_page.up | config/keybind.ts:71 |  | todo |  |
| keybind | diff.help | config/keybind.ts:90 |  | todo |  |
| keybind | diff.last | config/keybind.ts:73 |  | todo |  |
| keybind | diff.mark_reviewed | config/keybind.ts:89 |  | todo |  |
| keybind | diff.next_file | config/keybind.ts:82 |  | todo |  |
| keybind | diff.next_hunk | config/keybind.ts:80 |  | todo |  |
| keybind | diff.open | config/keybind.ts:64 |  | todo |  |
| keybind | diff.page.down | config/keybind.ts:68 |  | todo |  |
| keybind | diff.page.up | config/keybind.ts:69 |  | todo |  |
| keybind | diff.previous_file | config/keybind.ts:83 |  | todo |  |
| keybind | diff.previous_hunk | config/keybind.ts:81 |  | todo |  |
| keybind | diff.single_patch | config/keybind.ts:85 |  | todo |  |
| keybind | diff.switch_focus | config/keybind.ts:79 |  | todo |  |
| keybind | diff.switch_source | config/keybind.ts:86 |  | todo |  |
| keybind | diff.toggle | config/keybind.ts:75 |  | todo |  |
| keybind | diff.toggle_file_tree | config/keybind.ts:84 |  | todo |  |
| keybind | diff.toggle_fullscreen | config/keybind.ts:88 |  | todo |  |
| keybind | diff.toggle_view | config/keybind.ts:87 |  | todo |  |
| keybind | diff.up | config/keybind.ts:67 |  | todo |  |
| keybind | docs.open | config/keybind.ts:59 |  | todo |  |
| keybind | help.show | config/keybind.ts:58 |  | todo |  |
| keybind | input.backspace | config/keybind.ts:235 |  | todo |  |
| keybind | input.buffer.end | config/keybind.ts:229 |  | todo |  |
| keybind | input.buffer.home | config/keybind.ts:228 |  | todo |  |
| keybind | input.delete | config/keybind.ts:236 |  | todo |  |
| keybind | input.delete.line | config/keybind.ts:232 |  | todo |  |
| keybind | input.delete.to.line.end | config/keybind.ts:233 |  | todo |  |
| keybind | input.delete.to.line.start | config/keybind.ts:234 |  | todo |  |
| keybind | input.delete.word.backward | config/keybind.ts:244 |  | todo |  |
| keybind | input.delete.word.forward | config/keybind.ts:243 |  | todo |  |
| keybind | input.line.end | config/keybind.ts:221 |  | todo |  |
| keybind | input.line.home | config/keybind.ts:220 |  | todo |  |
| keybind | input.move.down | config/keybind.ts:215 |  | todo |  |
| keybind | input.move.left | config/keybind.ts:212 |  | todo |  |
| keybind | input.move.right | config/keybind.ts:213 |  | todo |  |
| keybind | input.move.up | config/keybind.ts:214 |  | todo |  |
| keybind | input.newline | config/keybind.ts:211 |  | todo |  |
| keybind | input.redo | config/keybind.ts:238 |  | todo |  |
| keybind | input.select.all | config/keybind.ts:245 |  | todo |  |
| keybind | input.select.buffer.end | config/keybind.ts:231 |  | todo |  |
| keybind | input.select.buffer.home | config/keybind.ts:230 |  | todo |  |
| keybind | input.select.down | config/keybind.ts:219 |  | todo |  |
| keybind | input.select.left | config/keybind.ts:216 |  | todo |  |
| keybind | input.select.line.end | config/keybind.ts:223 |  | todo |  |
| keybind | input.select.line.home | config/keybind.ts:222 |  | todo |  |
| keybind | input.select.right | config/keybind.ts:217 |  | todo |  |
| keybind | input.select.up | config/keybind.ts:218 |  | todo |  |
| keybind | input.select.visual.line.end | config/keybind.ts:227 |  | todo |  |
| keybind | input.select.visual.line.home | config/keybind.ts:226 |  | todo |  |
| keybind | input.select.word.backward | config/keybind.ts:242 |  | todo |  |
| keybind | input.select.word.forward | config/keybind.ts:241 |  | todo |  |
| keybind | input.submit | config/keybind.ts:210 |  | todo |  |
| keybind | input.undo | config/keybind.ts:237 |  | todo |  |
| keybind | input.visual.line.end | config/keybind.ts:225 |  | todo |  |
| keybind | input.visual.line.home | config/keybind.ts:224 |  | todo |  |
| keybind | input.word.backward | config/keybind.ts:240 |  | todo |  |
| keybind | input.word.forward | config/keybind.ts:239 |  | todo |  |
| keybind | leader | config/keybind.ts:46 |  | todo |  |
| keybind | location.reload | config/keybind.ts:63 |  | todo |  |
| keybind | mcp.list | config/keybind.ts:172 |  | deviation | 上游默认 none(无键);ox 只能经 /mcps 或命令面板打开。偏差 D7:ox 无 keybind 配置 |
| keybind | messages.copy | config/keybind.ts:194 |  | todo |  |
| keybind | model.cycle_favorite | config/keybind.ts:170 |  | todo |  |
| keybind | model.cycle_favorite_reverse | config/keybind.ts:171 |  | todo |  |
| keybind | model.cycle_recent | config/keybind.ts:168 |  | todo |  |
| keybind | model.cycle_recent_reverse | config/keybind.ts:169 |  | todo |  |
| keybind | model.dialog.favorite | config/keybind.ts:165 |  | todo |  |
| keybind | model.dialog.provider | config/keybind.ts:164 |  | todo |  |
| keybind | model.dialog.usage | config/keybind.ts:166 |  | todo |  |
| keybind | model.list | config/keybind.ts:167 |  | todo |  |
| keybind | open.menu | config/keybind.ts:112 |  | todo |  |
| keybind | opencode.debug | config/keybind.ts:104 |  | todo |  |
| keybind | opencode.settings | config/keybind.ts:60 |  | todo |  |
| keybind | opencode.status | config/keybind.ts:103 |  | todo |  |
| keybind | pane.focus.left | config/keybind.ts:97 |  | todo |  |
| keybind | pane.focus.right | config/keybind.ts:98 |  | todo |  |
| keybind | permission.prompt.fullscreen | config/keybind.ts:281 |  | todo |  |
| keybind | plugins.list | config/keybind.ts:290 |  | todo |  |
| keybind | plugins.toggle | config/keybind.ts:282 |  | todo |  |
| keybind | prompt.autocomplete.complete | config/keybind.ts:280 |  | todo |  |
| keybind | prompt.autocomplete.hide | config/keybind.ts:278 |  | todo |  |
| keybind | prompt.autocomplete.next | config/keybind.ts:277 |  | todo |  |
| keybind | prompt.autocomplete.prev | config/keybind.ts:276 |  | todo |  |
| keybind | prompt.autocomplete.select | config/keybind.ts:279 |  | todo |  |
| keybind | prompt.clear | config/keybind.ts:208 |  | todo |  |
| keybind | prompt.editor | config/keybind.ts:92 |  | todo |  |
| keybind | prompt.editor_context.clear | config/keybind.ts:201 |  | todo |  |
| keybind | prompt.history.next | config/keybind.ts:247 | crates/ox-tui/src/ui/prompt.rs:517 | aligned | 裸 ↓:先按可视行下移/移到文末,到文末再走历史;历史走完返回 Declined 让给 session.child.first(上游同样 return false,component/prompt/index.tsx:1086) |
| keybind | prompt.history.previous | config/keybind.ts:246 | crates/ox-tui/src/ui/prompt.rs:517 | aligned | 裸 ↑:先按可视行上移/移到文首,上文首再走历史;走完返回 Declined(ox 无 session.parent 绑定) |
| keybind | prompt.images.view | config/keybind.ts:202 | crates/ox-tui/src/app/keymap.rs:49 | aligned | `<leader>i` 打开输入框图片附件的放大预览;没有可预览附件时不动作,也不开空对话框(上游 `enabled: imageAttachments().length > 0`,component/prompt/index.tsx:496-499) |
| keybind | prompt.paste | config/keybind.ts:209 | crates/ox-tui/src/app/keymap.rs:120 | aligned | ctrl+v 读取剪贴板图片 |
| keybind | prompt.queue | config/keybind.ts:200 |  | todo |  |
| keybind | prompt.skills | config/keybind.ts:203 |  | todo |  |
| keybind | prompt.stash | config/keybind.ts:204 |  | todo |  |
| keybind | prompt.stash.list | config/keybind.ts:206 |  | todo |  |
| keybind | prompt.stash.pop | config/keybind.ts:205 |  | todo |  |
| keybind | prompt.submit | config/keybind.ts:199 |  | todo |  |
| keybind | provider.connect | config/keybind.ts:174 |  | todo |  |
| keybind | queued_prompt.delete | config/keybind.ts:135 |  | todo |  |
| keybind | queued_prompt.undo | config/keybind.ts:134 |  | todo |  |
| keybind | server.pair | config/keybind.ts:61 |  | todo |  |
| keybind | service.restart | config/keybind.ts:62 |  | todo |  |
| keybind | session.aside | config/keybind.ts:131 |  | todo |  |
| keybind | session.background | config/keybind.ts:129 |  | missing | 服务端路由已有(crates/ox-server/src/api/session/control.rs:47);ox-tui 尚未绑定 ctrl+b |
| keybind | session.cd | config/keybind.ts:132 |  | todo |  |
| keybind | session.child.first | config/keybind.ts:138 | crates/ox-tui/src/app/keymap.rs:117 | deviation | 裸 `↓` 已绑定,靠与上游相同的 fallthrough 到达选择器:历史走完时 prompt 返回 Declined(prompt.rs:213-219,上游 `component/prompt/index.tsx:1086`),`App::declined_key`(crates/ox-tui/src/app/mod.rs:1113)派发 Subagents 面板;footer 提示与 palette 同显示 `↓`(keymap.rs:117-122)。偏差:ox 无父会话信息(上游 `session.parentID`),所以子会话里按 `↓` 打开的是活动面板(空列表),上游此时关闭 composer(`routes/session/index.tsx:1404`);面板已打开时 `↓` 归面板所有(crates/ox-tui/src/app/mod.rs:933-944),不做上游那种关闭切换。`session.child.next/previous` 仍未绑 |
| keybind | session.child.next | config/keybind.ts:139 |  | todo |  |
| keybind | session.child.previous | config/keybind.ts:140 |  | todo |  |
| keybind | session.compact | config/keybind.ts:130 |  | missing | 服务端路由已有(crates/ox-server/src/api/session/compaction.rs,60cdfa5);ox-tui 尚未绑定 |
| keybind | session.copy | config/keybind.ts:107 |  | todo |  |
| keybind | session.copy.id | config/keybind.ts:108 |  | todo |  |
| keybind | session.delete | config/keybind.ts:124 |  | todo |  |
| keybind | session.export | config/keybind.ts:106 |  | todo |  |
| keybind | session.first | config/keybind.ts:187 |  | todo |  |
| keybind | session.fork | config/keybind.ts:122 |  | todo |  |
| keybind | session.half.page.down | config/keybind.ts:186 |  | todo |  |
| keybind | session.half.page.up | config/keybind.ts:185 |  | todo |  |
| keybind | session.interrupt | config/keybind.ts:128 |  | todo |  |
| keybind | session.last | config/keybind.ts:188 |  | todo |  |
| keybind | session.line.down | config/keybind.ts:184 |  | todo |  |
| keybind | session.line.up | config/keybind.ts:183 |  | todo |  |
| keybind | session.list | config/keybind.ts:111 |  | todo |  |
| keybind | session.message.next | config/keybind.ts:189 |  | todo |  |
| keybind | session.message.previous | config/keybind.ts:190 |  | todo |  |
| keybind | session.message.user.next | config/keybind.ts:191 |  | todo |  |
| keybind | session.message.user.previous | config/keybind.ts:192 |  | todo |  |
| keybind | session.messages_last_user | config/keybind.ts:193 |  | todo |  |
| keybind | session.move | config/keybind.ts:109 |  | todo |  |
| keybind | session.new | config/keybind.ts:110 |  | todo |  |
| keybind | session.page.down | config/keybind.ts:182 |  | todo |  |
| keybind | session.page.up | config/keybind.ts:181 |  | todo |  |
| keybind | session.parent | config/keybind.ts:141 |  | todo |  |
| keybind | session.pin.toggle | config/keybind.ts:142 |  | todo |  |
| keybind | session.queued_prompts | config/keybind.ts:133 | crates/ox-tui/src/app/keymap.rs:38 | aligned | <leader>q;无排队时退出,同上游 keymap.tsx:231 |
| keybind | session.quick_switch.1 | config/keybind.ts:143 |  | todo |  |
| keybind | session.quick_switch.2 | config/keybind.ts:144 |  | todo |  |
| keybind | session.quick_switch.3 | config/keybind.ts:145 |  | todo |  |
| keybind | session.quick_switch.4 | config/keybind.ts:146 |  | todo |  |
| keybind | session.quick_switch.5 | config/keybind.ts:147 |  | todo |  |
| keybind | session.quick_switch.6 | config/keybind.ts:148 |  | todo |  |
| keybind | session.quick_switch.7 | config/keybind.ts:149 |  | todo |  |
| keybind | session.quick_switch.8 | config/keybind.ts:150 |  | todo |  |
| keybind | session.quick_switch.9 | config/keybind.ts:151 |  | todo |  |
| keybind | session.redo | config/keybind.ts:196 |  | todo |  |
| keybind | session.rename | config/keybind.ts:123 |  | todo |  |
| keybind | session.share | config/keybind.ts:126 |  | todo |  |
| keybind | session.sidebar.toggle | config/keybind.ts:96 | crates/ox-tui/src/app/keymap.rs:40 | aligned | <leader>b;原 <leader>b 打开 Shells,改为侧栏,/shells 保留 |
| keybind | session.tab.close | config/keybind.ts:119 |  | todo |  |
| keybind | session.tab.history.back | config/keybind.ts:115 |  | todo |  |
| keybind | session.tab.history.forward | config/keybind.ts:116 |  | todo |  |
| keybind | session.tab.next | config/keybind.ts:113 |  | todo |  |
| keybind | session.tab.next_unread | config/keybind.ts:117 |  | todo |  |
| keybind | session.tab.previous | config/keybind.ts:114 |  | todo |  |
| keybind | session.tab.previous_unread | config/keybind.ts:118 |  | todo |  |
| keybind | session.tab.reopen | config/keybind.ts:120 |  | todo |  |
| keybind | session.tab.select.1 | config/keybind.ts:152 |  | todo |  |
| keybind | session.tab.select.10 | config/keybind.ts:161 |  | todo |  |
| keybind | session.tab.select.2 | config/keybind.ts:153 |  | todo |  |
| keybind | session.tab.select.3 | config/keybind.ts:154 |  | todo |  |
| keybind | session.tab.select.4 | config/keybind.ts:155 |  | todo |  |
| keybind | session.tab.select.5 | config/keybind.ts:156 |  | todo |  |
| keybind | session.tab.select.6 | config/keybind.ts:157 |  | todo |  |
| keybind | session.tab.select.7 | config/keybind.ts:158 |  | todo |  |
| keybind | session.tab.select.8 | config/keybind.ts:159 |  | todo |  |
| keybind | session.tab.select.9 | config/keybind.ts:160 |  | todo |  |
| keybind | session.timeline | config/keybind.ts:121 |  | todo |  |
| keybind | session.toggle.exploration_grouping | config/keybind.ts:136 |  | todo |  |
| keybind | session.toggle.scrollbar | config/keybind.ts:102 |  | todo |  |
| keybind | session.toggle.thinking | config/keybind.ts:197 |  | todo |  |
| keybind | session.undo | config/keybind.ts:195 |  | todo |  |
| keybind | session.unshare | config/keybind.ts:127 |  | todo |  |
| keybind | session.verbosity.cycle | config/keybind.ts:137 |  | todo |  |
| keybind | stash.delete | config/keybind.ts:163 |  | todo |  |
| keybind | terminal.close | config/keybind.ts:101 |  | todo |  |
| keybind | terminal.select | config/keybind.ts:99 |  | todo |  |
| keybind | terminal.suspend | config/keybind.ts:288 |  | todo |  |
| keybind | terminal.title.toggle | config/keybind.ts:289 |  | todo |  |
| keybind | terminal.toggle | config/keybind.ts:100 |  | todo |  |
| keybind | theme.mode.lock | config/keybind.ts:95 |  | todo |  |
| keybind | theme.switch | config/keybind.ts:93 |  | todo |  |
| keybind | theme.switch_mode | config/keybind.ts:94 |  | todo |  |
| keybind | usage.show | config/keybind.ts:173 |  | todo |  |
| keybind | variant.cycle | config/keybind.ts:178 |  | todo |  |
| keybind | variant.list | config/keybind.ts:179 |  | todo |  |
| keybind | which-key.end | config/keybind.ts:302 |  | todo |  |
| keybind | which-key.group.next | config/keybind.ts:296 |  | todo |  |
| keybind | which-key.group.previous | config/keybind.ts:295 |  | todo |  |
| keybind | which-key.home | config/keybind.ts:301 |  | todo |  |
| keybind | which-key.layout.toggle | config/keybind.ts:293 |  | todo |  |
| keybind | which-key.page.down | config/keybind.ts:300 |  | todo |  |
| keybind | which-key.page.up | config/keybind.ts:299 |  | todo |  |
| keybind | which-key.pending.toggle | config/keybind.ts:294 |  | todo |  |
| keybind | which-key.scroll.down | config/keybind.ts:298 |  | todo |  |
| keybind | which-key.scroll.up | config/keybind.ts:297 |  | todo |  |
| keybind | which-key.toggle | config/keybind.ts:292 |  | todo |  |
| sdk_call | credential.activate | component/dialog-integration.tsx:231 |  | todo |  |
| sdk_call | experimental.mcp.connect | component/dialog-mcp.tsx:124 | crates/ox-client/src/rest.rs:924 | aligned | 服务端 crates/ox-server/src/api/mcp.rs(60cdfa5);客户端 POST /api/experimental/mcp/{server}/connect,server 名按单段百分号编码,2xx 成功(实测 204,未知名 404);/mcps 的 space 调用,失败 toast `Failed to connect MCP server`,之后重新列表 |
| sdk_call | experimental.mcp.disconnect | component/dialog-mcp.tsx:124 | crates/ox-client/src/rest.rs:930 | aligned | 服务端 crates/ox-server/src/api/mcp.rs(60cdfa5);客户端 POST …/disconnect,同上;失败 toast `Failed to disconnect MCP server` |
| sdk_call | experimental.usage.limits | component/usage.tsx:37 |  | partial | 服务端返回 data:null(无 Console 账号,即上游未登录答案;crates/ox-server/src/api/compat.rs,60cdfa5);ox-tui 未调用 |
| sdk_call | file.find | component/prompt/autocomplete.tsx:378 |  | todo |  |
| sdk_call | file.list | app.tsx:227 |  | todo |  |
| sdk_call | location.get | app.tsx:229 |  | todo |  |
| sdk_call | server.info | component/devtools-bar.tsx:57 |  | todo |  |
| sdk_call | server.pair | component/dialog-pair.tsx:22 |  | todo |  |
| sdk_call | session.active | component/dialog-open.tsx:179 |  | partial | 服务端已对齐,返回运行中会话(crates/ox-server/src/api/session/switch.rs,60cdfa5);ox-tui 未调用 |
| sdk_call | session.background | component/prompt/index.tsx:544 |  | partial | 服务端已对齐(crates/ox-server/src/api/session/control.rs:47,04229b4);ox-client/ox-tui 未调用 |
| sdk_call | session.command | component/prompt/index.tsx:1325 |  | todo |  |
| sdk_call | session.compact | routes/session/index.tsx:1092 |  | partial | 服务端已对齐(crates/ox-server/src/api/session/compaction.rs,60cdfa5);偏差:跳过 SessionRevert.commit、进程级准入锁;ox-tui 未调用 |
| sdk_call | session.environment | component/prompt/index.tsx:1255 |  | todo |  |
| sdk_call | session.interrupt | component/prompt/index.tsx:524 |  | todo |  |
| sdk_call | session.list | component/dialog-session-list.tsx:63 |  | todo |  |
| sdk_call | session.move | component/dialog-workspaces.tsx:318 |  | todo |  |
| sdk_call | session.shell | component/prompt/index.tsx:1315 |  | partial | 服务端已对齐(crates/ox-server/src/api/session/user_shell.rs,60cdfa5);偏差 D1-D3 见 .ox/plans/2026-11-01-cheap-parity-gaps.md;ox-client/ox-tui 未调用 |
| sdk_call | session.switchAgent | component/prompt/index.tsx:1288 |  | todo |  |
| sdk_call | session.switchModel | component/prompt/index.tsx:1294 |  | todo |  |
| sdk_call | session.synthetic | component/prompt/index.tsx:1352 |  | todo |  |
| sdk_call | session.update | routes/session/index.tsx:1051 |  | todo |  |
| sdk_call | session.view | context/session-tabs.tsx:245 |  | todo |  |
| sdk_call | shell.get | component/dialog-shell-output.tsx:41 |  | todo |  |
| sdk_call | shell.output | component/dialog-shell-output.tsx:46 | crates/ox-client/src/rest.rs:433 | aligned |  |
| sdk_call | shell.remove | routes/session/composer/shell-tab.tsx:104 | crates/ox-client/src/rest.rs:485 | aligned |  |
| sdk_call | worktree.create | component/prompt/move.tsx:41 |  | todo |  |
| sdk_call | worktree.list | component/dialog-open.tsx:122 |  | todo |  |
| sdk_call | worktree.refresh | component/dialog-open.tsx:133 |  | todo |  |
| slash | agents | app.tsx:952 | crates/ox-tui/src/ui/commands.rs:58 | todo | ox 有同名命令,行为未逐项核对 |
| slash | btw | feature-plugins/prompt/btw.tsx:57 |  | todo |  |
| slash | cd | component/prompt/index.tsx:271 |  | todo |  |
| slash | clear | app.tsx:804 | crates/ox-tui/src/ui/commands.rs:66 | todo | ox 有同名命令,行为未逐项核对 |
| slash | compact | routes/session/index.tsx:1087 |  | todo |  |
| slash | connect | app.tsx:1022 |  | todo |  |
| slash | copy | routes/session/index.tsx:1264 |  | todo |  |
| slash | debug | app.tsx:1122 |  | todo |  |
| slash | diff | feature-plugins/system/diff-viewer.tsx:1319 |  | todo |  |
| slash | editor | component/prompt/index.tsx:554 |  | todo |  |
| slash | exit | app.tsx:1180 | crates/ox-tui/src/ui/commands.rs:74 | todo | ox 有同名命令,行为未逐项核对 |
| slash | export | routes/session/index.tsx:1337 |  | todo |  |
| slash | fork | routes/session/index.tsx:1069 | crates/ox-tui/src/ui/commands.rs:90 | todo | ox 有同名命令,行为未逐项核对 |
| slash | help | app.tsx:1162 |  | todo |  |
| slash | highlights | app.tsx:1037 |  | todo |  |
| slash | mcps | app.tsx:970 | crates/ox-tui/src/ui/commands.rs:95 | aligned | /mcps 打开 MCP 对话框(AppCommand::McpList(Dialog),crates/ox-tui/src/app/mod.rs:1306) |
| slash | models | app.tsx:907 | crates/ox-tui/src/ui/commands.rs:82 | todo | ox 有同名命令,行为未逐项核对 |
| slash | new | app.tsx:778 | crates/ox-tui/src/ui/commands.rs:98 | todo | ox 有同名命令,行为未逐项核对 |
| slash | open | app.tsx:831 |  | todo |  |
| slash | pair | app.tsx:1076 |  | todo |  |
| slash | plugins | feature-plugins/system/plugins.tsx:340 |  | todo |  |
| slash | redo | routes/session/index.tsx:1138 |  | todo |  |
| slash | reload | app.tsx:1106 |  | todo |  |
| slash | rename | routes/session/index.tsx:1045 | crates/ox-tui/src/ui/commands.rs:106 | todo | ox 有同名命令,行为未逐项核对 |
| slash | restart | app.tsx:1087 |  | todo |  |
| slash | sessions | app.tsx:768 | crates/ox-tui/src/ui/commands.rs:114 | todo | ox 有同名命令,行为未逐项核对 |
| slash | settings | app.tsx:1047 |  | todo |  |
| slash | skills | component/prompt/index.tsx:587 |  | todo |  |
| slash | stats | feature-plugins/system/stats.tsx:200 |  | todo |  |
| slash | status | app.tsx:1056 | crates/ox-tui/src/ui/commands.rs:130 | todo | ox 有同名命令,行为未逐项核对 |
| slash | terminal | component/session-frame.tsx:250 |  | todo |  |
| slash | themes | app.tsx:1131 |  | todo |  |
| slash | timeline | routes/session/index.tsx:1060 | crates/ox-tui/src/ui/commands.rs:122 | todo | ox 有同名命令,行为未逐项核对 |
| slash | undo | routes/session/index.tsx:1110 |  | todo |  |
| slash | update | app.tsx:1067 |  | todo |  |
| slash | usage | app.tsx:961 |  | todo |  |
| slash | variants | app.tsx:997 |  | todo |  |
| slash | worktrees | component/prompt/index.tsx:627 |  | todo |  |
| slash_alias | continue | app.tsx:768 | crates/ox-tui/src/ui/commands.rs:114 | todo | ox 有同名别名,行为未逐项核对 |
| slash_alias | effort | app.tsx:997 |  | todo |  |
| slash_alias | project | app.tsx:831 |  | todo |  |
| slash_alias | projects | app.tsx:831 |  | todo |  |
| slash_alias | q | app.tsx:1180 | crates/ox-tui/src/ui/commands.rs:74 | todo | ox 有同名别名,行为未逐项核对 |
| slash_alias | quit | app.tsx:1180 | crates/ox-tui/src/ui/commands.rs:74 | todo | ox 有同名别名,行为未逐项核对 |
| slash_alias | resume | app.tsx:768 | crates/ox-tui/src/ui/commands.rs:114 | todo | ox 有同名别名,行为未逐项核对 |
| slash_alias | thinking | app.tsx:997 |  | todo |  |
| slash_alias | web | app.tsx:1076 |  | todo |  |
| sse_event | form.cancelled | feature-plugins/system/notifications.ts:64 |  | todo |  |
| sse_event | form.created | feature-plugins/system/notifications.ts:58 |  | todo |  |
| sse_event | form.replied | feature-plugins/system/notifications.ts:63 |  | todo |  |
| sse_event | installation.update-available | context/update-notification.tsx:132 |  | todo |  |
| sse_event | installation.updated | context/update-notification.tsx:142 |  | todo |  |
| sse_event | log.synced | component/devtools-bar.tsx:166 |  | n/a | devtools 状态栏专用,非核心体验(待用户确认) |
| sse_event | permission.asked | feature-plugins/system/notifications.ts:65 |  | todo |  |
| sse_event | permission.replied | feature-plugins/system/notifications.ts:70 |  | todo |  |
| sse_event | plugin.updated | feature-plugins/system/plugins.tsx:39 |  | todo |  |
| sse_event | provider.budget-limit | component/usage.tsx:276 |  | todo |  |
| sse_event | provider.free-limit | feature-plugins/system/notifications.ts:87 |  | todo |  |
| sse_event | provider.go-limit | component/usage.tsx:267 |  | todo |  |
| sse_event | provider.model-budget-limit | component/usage.tsx:274 |  | todo |  |
| sse_event | queued.prompts | mini/footer.ts:506 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | server.connected | context/client-mcp.tsx:108 |  | n/a | 偏差 D6:上游 client-mcp 实验功能(TUI 自己充当 MCP server),ox 不做 |
| sse_event | session.agent.selected | context/local.tsx:71 |  | todo |  |
| sse_event | session.compaction.delta | mini/stream-v2.transport.ts:1113 |  | partial | 服务端已发射(crates/ox-schema/src/event.rs:622-655),ox-client 消费(crates/ox-client/src/store.rs:43);字段未逐项核对 |
| sse_event | session.compaction.ended | mini/stream-v2.transport.ts:1120 |  | partial | 服务端已发射(crates/ox-schema/src/event.rs:622-655),ox-client 消费(crates/ox-client/src/store.rs:43);字段未逐项核对 |
| sse_event | session.compaction.failed | mini/stream-v2.transport.ts:1133 |  | partial | 服务端已发射(crates/ox-schema/src/event.rs:622-655),ox-client 消费(crates/ox-client/src/store.rs:43);字段未逐项核对 |
| sse_event | session.compaction.started | mini/stream-v2.transport.ts:1105 |  | partial | 服务端已发射(crates/ox-schema/src/event.rs:622-655),ox-client 消费(crates/ox-client/src/store.rs:43);字段未逐项核对 |
| sse_event | session.deleted | app.tsx:1366 |  | todo |  |
| sse_event | session.execution.failed | feature-plugins/system/notifications.ts:79 |  | todo |  |
| sse_event | session.execution.interrupted | feature-plugins/system/notifications.ts:78 |  | todo |  |
| sse_event | session.execution.started | feature-plugins/system/notifications.ts:71 |  | todo |  |
| sse_event | session.execution.succeeded | component/usage.tsx:220 |  | todo |  |
| sse_event | session.inbox.cancelled | mini/stream-v2.subagent.ts:673 |  | todo |  |
| sse_event | session.inbox.delivered | mini/stream-v2.subagent.ts:663 |  | todo |  |
| sse_event | session.inbox.delivery.changed | mini/stream-v2.transport.ts:1080 |  | todo |  |
| sse_event | session.inbox.enqueued | context/session-tabs.tsx:334 |  | todo |  |
| sse_event | session.instructions.updated | routes/session/rows.ts:266 |  | todo |  |
| sse_event | session.model.selected | context/local.tsx:344 |  | todo |  |
| sse_event | session.moved | app.tsx:1360 |  | todo |  |
| sse_event | session.reasoning.delta | mini/stream-v2.subagent.ts:725 |  | todo |  |
| sse_event | session.reasoning.ended | mini/stream-v2.subagent.ts:743 |  | todo |  |
| sse_event | session.reasoning.started | mini/stream-v2.subagent.ts:722 |  | todo |  |
| sse_event | session.renamed | mini/stream-v2.transport.ts:1055 |  | todo |  |
| sse_event | session.retry.scheduled | routes/session/rows.ts:303 |  | todo |  |
| sse_event | session.shell.ended | mini/stream-v2.transport.ts:1165 |  | partial | 服务端发射并投影(crates/ox-schema/src/event.rs,60cdfa5);ox-client 未消费 |
| sse_event | session.shell.started | mini/stream-v2.transport.ts:1144 |  | partial | 服务端发射并投影,事件 id = body.id 的 msg_→evt_(crates/ox-server/src/api/session/user_shell.rs,60cdfa5);ox-client 未消费 |
| sse_event | session.skill.activated | mini/stream-v2.transport.ts:1097 |  | todo |  |
| sse_event | session.step.ended | mini/stream-v2.subagent.ts:903 |  | todo |  |
| sse_event | session.step.failed | mini/stream-v2.subagent.ts:904 |  | todo |  |
| sse_event | session.step.started | mini/stream-v2.subagent.ts:677 |  | todo |  |
| sse_event | session.synthetic | routes/session/rows.ts:267 |  | todo |  |
| sse_event | session.text.delta | mini/stream-v2.subagent.ts:687 |  | todo |  |
| sse_event | session.text.ended | mini/stream-v2.subagent.ts:705 |  | todo |  |
| sse_event | session.text.started | mini/stream-v2.subagent.ts:684 |  | todo |  |
| sse_event | session.tool.called | mini/stream-v2.subagent.ts:792 |  | todo |  |
| sse_event | session.tool.failed | mini/stream-v2.subagent.ts:842 |  | todo |  |
| sse_event | session.tool.input.delta | mini/stream-v2.subagent.ts:775 |  | todo |  |
| sse_event | session.tool.input.ended | mini/stream-v2.subagent.ts:775 |  | todo |  |
| sse_event | session.tool.input.started | mini/stream-v2.subagent.ts:760 |  | todo |  |
| sse_event | session.tool.progress | mini/stream-v2.subagent.ts:813 |  | todo |  |
| sse_event | session.tool.success | mini/stream-v2.subagent.ts:842 |  | todo |  |
| sse_event | stream.patch | mini/footer.ts:157 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | stream.subagent | mini/footer.ts:530 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | stream.view | mini/footer.ts:540 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | tui.command.execute | app.tsx:1337 |  | todo |  |
| sse_event | tui.prompt.append | component/prompt/index.tsx:340 |  | todo |  |
| sse_event | tui.session.select | app.tsx:1352 |  | todo |  |
| sse_event | tui.toast.show | app.tsx:1342 |  | todo |  |
| sse_event | turn.duration | mini/footer.ts:453 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | turn.idle | mini/footer.ts:150 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | turn.send | mini/footer.ts:141 |  | n/a | mini 精简 TUI 的内部发射器名,不是服务端 SSE 事件 |
| sse_event | worktree.updated | component/dialog-open.tsx:136 |  | todo |  |
| theme_token | accent | theme/assets/opencode.json:52 |  | todo |  |
| theme_token | background | theme/assets/opencode.json:80 |  | todo |  |
| theme_token | backgroundElement | theme/assets/opencode.json:88 |  | todo |  |
| theme_token | backgroundPanel | theme/assets/opencode.json:84 |  | todo |  |
| theme_token | border | theme/assets/opencode.json:92 |  | todo |  |
| theme_token | borderActive | theme/assets/opencode.json:96 |  | todo |  |
| theme_token | borderSubtle | theme/assets/opencode.json:100 |  | todo |  |
| theme_token | diffAdded | theme/assets/opencode.json:104 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffAddedBg | theme/assets/opencode.json:128 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffAddedLineNumberBg | theme/assets/opencode.json:144 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffContext | theme/assets/opencode.json:112 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffContextBg | theme/assets/opencode.json:136 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffHighlightAdded | theme/assets/opencode.json:120 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffHighlightRemoved | theme/assets/opencode.json:124 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffHunkHeader | theme/assets/opencode.json:116 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffLineNumber | theme/assets/opencode.json:140 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffRemoved | theme/assets/opencode.json:108 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffRemovedBg | theme/assets/opencode.json:132 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | diffRemovedLineNumberBg | theme/assets/opencode.json:148 | crates/ox-tui/src/theme.rs:117 | aligned | Palette.diff 对应 diff.* token |
| theme_token | error | theme/assets/opencode.json:56 |  | todo |  |
| theme_token | info | theme/assets/opencode.json:68 |  | todo |  |
| theme_token | markdownBlockQuote | theme/assets/opencode.json:172 |  | todo |  |
| theme_token | markdownCode | theme/assets/opencode.json:168 |  | todo |  |
| theme_token | markdownCodeBlock | theme/assets/opencode.json:204 |  | todo |  |
| theme_token | markdownEmph | theme/assets/opencode.json:176 |  | todo |  |
| theme_token | markdownHeading | theme/assets/opencode.json:156 | crates/ox-tui/src/ui/markdown.rs:69 | deviation | 偏差:只加粗不加下划线(与上游一致,采集核对 `b` 无 `u`);上游 heading 与正文同色,ox 用 palette.text |
| theme_token | markdownHorizontalRule | theme/assets/opencode.json:184 |  | todo |  |
| theme_token | markdownImage | theme/assets/opencode.json:196 |  | todo |  |
| theme_token | markdownImageText | theme/assets/opencode.json:200 |  | todo |  |
| theme_token | markdownLink | theme/assets/opencode.json:160 |  | todo |  |
| theme_token | markdownLinkText | theme/assets/opencode.json:164 |  | todo |  |
| theme_token | markdownListEnumeration | theme/assets/opencode.json:192 |  | todo |  |
| theme_token | markdownListItem | theme/assets/opencode.json:188 |  | todo |  |
| theme_token | markdownStrong | theme/assets/opencode.json:180 |  | todo |  |
| theme_token | markdownText | theme/assets/opencode.json:152 |  | todo |  |
| theme_token | primary | theme/assets/opencode.json:44 |  | todo |  |
| theme_token | secondary | theme/assets/opencode.json:48 |  | todo |  |
| theme_token | success | theme/assets/opencode.json:64 |  | todo |  |
| theme_token | syntaxComment | theme/assets/opencode.json:208 |  | todo |  |
| theme_token | syntaxFunction | theme/assets/opencode.json:216 |  | todo |  |
| theme_token | syntaxKeyword | theme/assets/opencode.json:212 |  | todo |  |
| theme_token | syntaxNumber | theme/assets/opencode.json:228 |  | todo |  |
| theme_token | syntaxOperator | theme/assets/opencode.json:236 |  | todo |  |
| theme_token | syntaxPunctuation | theme/assets/opencode.json:240 |  | todo |  |
| theme_token | syntaxString | theme/assets/opencode.json:224 |  | todo |  |
| theme_token | syntaxType | theme/assets/opencode.json:232 |  | todo |  |
| theme_token | syntaxVariable | theme/assets/opencode.json:220 |  | todo |  |
| theme_token | text | theme/assets/opencode.json:72 |  | todo |  |
| theme_token | textMuted | theme/assets/opencode.json:76 |  | todo |  |
| theme_token | warning | theme/assets/opencode.json:60 |  | todo |  |
| theme_token_v2 | @dialog.background.action.primary.$hovered | theme/assets/v2/opencode.json:154 |  | todo |  |
| theme_token_v2 | @dialog.background.base | theme/assets/v2/opencode.json:151 |  | todo |  |
| theme_token_v2 | background.action.destructive.base | theme/assets/v2/opencode.json:71 |  | todo |  |
| theme_token_v2 | background.action.primary.$focused | theme/assets/v2/opencode.json:64 |  | todo |  |
| theme_token_v2 | background.action.primary.$hovered | theme/assets/v2/opencode.json:63 |  | todo |  |
| theme_token_v2 | background.action.primary.$selected | theme/assets/v2/opencode.json:65 |  | todo |  |
| theme_token_v2 | background.action.primary.base | theme/assets/v2/opencode.json:62 |  | todo |  |
| theme_token_v2 | background.action.secondary.base | theme/assets/v2/opencode.json:68 |  | todo |  |
| theme_token_v2 | background.base | theme/assets/v2/opencode.json:54 |  | todo |  |
| theme_token_v2 | background.feedback.error.base | theme/assets/v2/opencode.json:79 |  | todo |  |
| theme_token_v2 | background.feedback.info.base | theme/assets/v2/opencode.json:88 |  | todo |  |
| theme_token_v2 | background.feedback.success.base | theme/assets/v2/opencode.json:85 |  | todo |  |
| theme_token_v2 | background.feedback.warning.base | theme/assets/v2/opencode.json:82 |  | todo |  |
| theme_token_v2 | background.formfield.base | theme/assets/v2/opencode.json:75 |  | todo |  |
| theme_token_v2 | background.raised.base | theme/assets/v2/opencode.json:56 |  | todo |  |
| theme_token_v2 | background.raised.high | theme/assets/v2/opencode.json:57 |  | todo |  |
| theme_token_v2 | background.raised.max | theme/assets/v2/opencode.json:58 |  | todo |  |
| theme_token_v2 | border.base | theme/assets/v2/opencode.json:93 |  | todo |  |
| theme_token_v2 | categorical | theme/assets/v2/opencode.json:4 |  | todo |  |
| theme_token_v2 | diff.background.added | theme/assets/v2/opencode.json:106 |  | todo |  |
| theme_token_v2 | diff.background.context | theme/assets/v2/opencode.json:108 |  | todo |  |
| theme_token_v2 | diff.background.removed | theme/assets/v2/opencode.json:107 |  | todo |  |
| theme_token_v2 | diff.highlight.added | theme/assets/v2/opencode.json:111 |  | todo |  |
| theme_token_v2 | diff.highlight.removed | theme/assets/v2/opencode.json:112 |  | todo |  |
| theme_token_v2 | diff.lineNumber.background.added | theme/assets/v2/opencode.json:117 |  | todo |  |
| theme_token_v2 | diff.lineNumber.background.removed | theme/assets/v2/opencode.json:118 |  | todo |  |
| theme_token_v2 | diff.lineNumber.text | theme/assets/v2/opencode.json:115 |  | todo |  |
| theme_token_v2 | diff.text.added | theme/assets/v2/opencode.json:100 |  | todo |  |
| theme_token_v2 | diff.text.context | theme/assets/v2/opencode.json:102 |  | todo |  |
| theme_token_v2 | diff.text.hunkHeader | theme/assets/v2/opencode.json:103 |  | todo |  |
| theme_token_v2 | diff.text.removed | theme/assets/v2/opencode.json:101 |  | todo |  |
| theme_token_v2 | markdown.blockQuote | theme/assets/v2/opencode.json:139 |  | todo |  |
| theme_token_v2 | markdown.code | theme/assets/v2/opencode.json:138 |  | todo |  |
| theme_token_v2 | markdown.codeBlock | theme/assets/v2/opencode.json:147 |  | todo |  |
| theme_token_v2 | markdown.emphasis | theme/assets/v2/opencode.json:140 |  | todo |  |
| theme_token_v2 | markdown.heading | theme/assets/v2/opencode.json:135 |  | todo |  |
| theme_token_v2 | markdown.horizontalRule | theme/assets/v2/opencode.json:142 |  | todo |  |
| theme_token_v2 | markdown.image | theme/assets/v2/opencode.json:145 |  | todo |  |
| theme_token_v2 | markdown.imageText | theme/assets/v2/opencode.json:146 |  | todo |  |
| theme_token_v2 | markdown.link | theme/assets/v2/opencode.json:136 |  | todo |  |
| theme_token_v2 | markdown.linkText | theme/assets/v2/opencode.json:137 |  | todo |  |
| theme_token_v2 | markdown.listEnumeration | theme/assets/v2/opencode.json:144 |  | todo |  |
| theme_token_v2 | markdown.listItem | theme/assets/v2/opencode.json:143 |  | todo |  |
| theme_token_v2 | markdown.strong | theme/assets/v2/opencode.json:141 |  | todo |  |
| theme_token_v2 | markdown.text | theme/assets/v2/opencode.json:134 |  | todo |  |
| theme_token_v2 | scrollbar.base | theme/assets/v2/opencode.json:96 |  | todo |  |
| theme_token_v2 | syntax.comment | theme/assets/v2/opencode.json:123 |  | todo |  |
| theme_token_v2 | syntax.function | theme/assets/v2/opencode.json:125 |  | todo |  |
| theme_token_v2 | syntax.keyword | theme/assets/v2/opencode.json:124 |  | todo |  |
| theme_token_v2 | syntax.number | theme/assets/v2/opencode.json:128 |  | todo |  |
| theme_token_v2 | syntax.operator | theme/assets/v2/opencode.json:130 |  | todo |  |
| theme_token_v2 | syntax.punctuation | theme/assets/v2/opencode.json:131 |  | todo |  |
| theme_token_v2 | syntax.string | theme/assets/v2/opencode.json:127 |  | todo |  |
| theme_token_v2 | syntax.type | theme/assets/v2/opencode.json:129 |  | todo |  |
| theme_token_v2 | syntax.variable | theme/assets/v2/opencode.json:126 |  | todo |  |
| theme_token_v2 | text.action.destructive.$disabled | theme/assets/v2/opencode.json:27 |  | todo |  |
| theme_token_v2 | text.action.destructive.base | theme/assets/v2/opencode.json:26 |  | todo |  |
| theme_token_v2 | text.action.primary.$disabled | theme/assets/v2/opencode.json:17 |  | todo |  |
| theme_token_v2 | text.action.primary.$focused | theme/assets/v2/opencode.json:18 |  | todo |  |
| theme_token_v2 | text.action.primary.$selected | theme/assets/v2/opencode.json:19 |  | todo |  |
| theme_token_v2 | text.action.primary.base | theme/assets/v2/opencode.json:16 |  | todo |  |
| theme_token_v2 | text.action.secondary.$hovered | theme/assets/v2/opencode.json:23 |  | todo |  |
| theme_token_v2 | text.action.secondary.base | theme/assets/v2/opencode.json:22 |  | todo |  |
| theme_token_v2 | text.base | theme/assets/v2/opencode.json:12 |  | todo |  |
| theme_token_v2 | text.feedback.error.base | theme/assets/v2/opencode.json:40 |  | todo |  |
| theme_token_v2 | text.feedback.info.base | theme/assets/v2/opencode.json:49 |  | todo |  |
| theme_token_v2 | text.feedback.success.base | theme/assets/v2/opencode.json:46 |  | todo |  |
| theme_token_v2 | text.feedback.warning.base | theme/assets/v2/opencode.json:43 |  | todo |  |
| theme_token_v2 | text.formfield.$disabled | theme/assets/v2/opencode.json:35 |  | todo |  |
| theme_token_v2 | text.formfield.$focused | theme/assets/v2/opencode.json:33 |  | todo |  |
| theme_token_v2 | text.formfield.$hovered | theme/assets/v2/opencode.json:32 |  | todo |  |
| theme_token_v2 | text.formfield.$pressed | theme/assets/v2/opencode.json:34 |  | todo |  |
| theme_token_v2 | text.formfield.$selected | theme/assets/v2/opencode.json:36 |  | todo |  |
| theme_token_v2 | text.formfield.base | theme/assets/v2/opencode.json:31 |  | todo |  |
| theme_token_v2 | text.muted | theme/assets/v2/opencode.json:13 |  | todo |  |
| tool_renderer | edit | routes/session/message-parts.tsx:29 | crates/ox-tui/src/ui/edit_block.rs:38 | partial | 见 behavior edit-block |
| tool_renderer | execute | routes/session/message-parts.tsx:31 |  | todo |  |
| tool_renderer | glob | routes/session/message-parts.tsx:23 |  | todo |  |
| tool_renderer | grep | routes/session/message-parts.tsx:25 |  | todo |  |
| tool_renderer | patch | routes/session/message-parts.tsx:32 | crates/ox-tui/src/ui/patch_block.rs:44 | partial | 见 behavior patch-block |
| tool_renderer | question | routes/session/message-parts.tsx:34 | crates/ox-tui/src/ui/question_block.rs:1 | partial | 见 behavior question-block |
| tool_renderer | read | routes/session/message-parts.tsx:24 |  | todo |  |
| tool_renderer | shell | routes/session/message-parts.tsx:22 | crates/ox-tui/src/ui/shell_block.rs:173 | partial | 见 behavior shell-block |
| tool_renderer | skill | routes/session/message-parts.tsx:35 |  | todo |  |
| tool_renderer | subagent | routes/session/message-parts.tsx:30 |  | todo |  |
| tool_renderer | todowrite | routes/session/message-parts.tsx:33 |  | todo |  |
| tool_renderer | webfetch | routes/session/message-parts.tsx:26 |  | todo |  |
| tool_renderer | websearch | routes/session/message-parts.tsx:27 |  | todo |  |
| tool_renderer | write | routes/session/message-parts.tsx:28 | crates/ox-tui/src/ui/write_block.rs:40 | partial | 见 behavior write-block |
