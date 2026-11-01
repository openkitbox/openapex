//! gallery: 静态快照画廊——真实组件 + mock 数据 → 真实渲染路径 → ANSI 输出。
//! 开发工具：看组件形态不需要从 home 页起全 app。
//!
//! 用法: cargo run -p ox-tui --bin gallery [-- --width 80]

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal};

use ox_client::rest::{McpRef, McpStatus};
use ox_client::store::{
    CompletionSource, DataStore, ErrorInfo, Notice, Queued, RetryInfo, Switch, ToolState,
};
use ox_schema::support::Record;
use ox_tui::app::keymap;
use ox_tui::theme::{Deck, Palette};
use ox_tui::ui::dialog;
use ox_tui::ui::dock::{DockHover, StatusMode, queued_dock, status_line};
use ox_tui::ui::home;
use ox_tui::ui::markdown;
use ox_tui::ui::palette;
use ox_tui::ui::prompt::{Activity, Chrome, Footer, McpSummary, Prompt, placeholder_line};
use ox_tui::ui::session_dialog;
use ox_tui::ui::timeline::{Block, Explored, ExploredCount, Transcript, TurnFooter};
use ox_tui::ui::toast::{self, Toast, Variant};

/// 一个用例：独立 TestBackend 终端画一帧，然后把 Buffer 打成 ANSI。
/// 每个用例的 draw 都在闭包内部自建状态，Fn 即可（无跨帧可变捕获）。
type Draw = Box<dyn Fn(&mut Frame, Rect)>;

struct Case {
    name: &'static str,
    height: u16,
    draw: Draw,
}

fn main() {
    let width = parse_width();
    for case in cases() {
        let mut terminal =
            Terminal::new(TestBackend::new(width, case.height)).expect("test terminal");
        terminal
            .draw(|frame| (case.draw)(frame, frame.area()))
            .expect("draw");
        println!("── {} ──", case.name);
        print!("{}", buffer_to_ansi(terminal.backend().buffer()));
    }
}

/// 默认 100 列；`--width N`（>= 20）覆盖。非法参数 fail closed。
fn parse_width() -> u16 {
    let mut width = 100;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg != "--width" {
            usage();
        }
        match args.next().and_then(|v| v.parse::<u16>().ok()) {
            Some(w) if (20..=500).contains(&w) => width = w,
            _ => usage(),
        }
    }
    width
}

fn usage() -> ! {
    eprintln!("usage: gallery [--width <20..500>]");
    std::process::exit(2);
}

fn cases() -> Vec<Case> {
    vec![
        transcript_case(
            "user-box-short",
            3,
            vec![Block::user("帮我把这个函数改成迭代实现")],
        ),
        // 用户消息盒：长文本换行
        transcript_case(
            "user-box-wrapped",
            5,
            vec![Block::user(
                "把对话区的渲染对齐上游 opencode，用户消息用左侧竖线的 raised 盒子，\
                 助手文本不要 ✻ 头部，reasoning 折叠成 Thought 头部，这段文字足够长可以换行验证",
            )],
        ),
        // 助手正文
        transcript_case(
            "assistant-markdown",
            14,
            assistant(
                "## 完成\n- first\n- second\n> quoted\n```rust\nlet ok = true;\n```\n**bold** and plain",
                "",
                true,
            ),
        ),
        transcript_case("assistant-plain", 4, assistant("可以交付了。", "", true)),
        transcript_case(
            "assistant-streaming",
            4,
            assistant("正在输出中……", "", false),
        ),
        // reasoning 三态
        transcript_case(
            "reasoning-collapsed",
            4,
            assistant("答案。", "正在权衡两种布局方案，比较各自开销", true),
        ),
        // reasoning 展开态：需要 toggle，单独一个 Case
        Case {
            name: "reasoning-expanded",
            height: 14,
            draw: Box::new(|frame, area| {
                let mut t = Transcript::new(area.width, area.height);
                t.sync(
                    &assistant(
                        "答案。",
                        "**比较布局**\n\n正在权衡两种布局方案，比较各自**开销**\n\n- 方案 A 用 `grid`\n- 方案 B\n\n```rust\nfn main() {}\n```",
                        true,
                    ),
                    0,
                );
                t.toggle_last_reasoning();
                t.render(frame, area);
            }),
        },
        transcript_case(
            "reasoning-streaming",
            4,
            assistant("", "正在权衡两种布局方案", false),
        ),
        // shell 块（上游 BlockTool + Shell）
        transcript_case(
            "shell-writing",
            5,
            vec![shell(
                ToolState::Input,
                serde_json::json!({}),
                "",
                Live::default(),
            )],
        ),
        transcript_case(
            "shell-running",
            7,
            vec![shell(
                ToolState::Running,
                serde_json::json!({"command": "cargo test -p ox-tui"}),
                "Compiling ox-tui v0.1.0",
                Live::default(),
            )],
        ),
        transcript_case(
            "shell-done-workdir",
            8,
            vec![shell(
                ToolState::Success,
                serde_json::json!({"command": "ls", "workdir": "/var/log"}),
                "\u{1b}[34msystem.log\u{1b}[0m\ninstall.log",
                Live::default(),
            )],
        ),
        transcript_case(
            "shell-collapsed-long",
            13,
            vec![shell(
                ToolState::Success,
                serde_json::json!({"command": "seq 30"}),
                &long_output(),
                Live::default(),
            )],
        ),
        Case {
            name: "shell-expanded",
            height: 36,
            draw: Box::new(|frame, area| {
                let mut t = Transcript::new(area.width, area.height);
                t.sync(
                    &[shell(
                        ToolState::Success,
                        serde_json::json!({"command": "seq 30"}),
                        &long_output(),
                        Live::default(),
                    )],
                    0,
                );
                t.toggle_tool_output();
                t.render(frame, area);
            }),
        },
        transcript_case(
            "shell-failed",
            7,
            vec![shell(
                ToolState::Failed,
                serde_json::json!({"command": "cargo build"}),
                "error[E0599]: no variant named `Shell`",
                Live::default(),
            )],
        ),
        transcript_case(
            "shell-background",
            7,
            vec![shell(
                ToolState::Success,
                serde_json::json!({"command": "npm run dev"}),
                "VITE ready in 312 ms",
                Live {
                    shell: true,
                    background: true,
                    running: true,
                },
            )],
        ),
        // 工具行四态
        transcript_case(
            "tool-input",
            2,
            vec![tool("shell", "cargo test -p ox-tui", ToolState::Input, "")],
        ),
        transcript_case(
            "tool-running",
            2,
            vec![tool(
                "shell",
                "cargo test -p ox-tui",
                ToolState::Running,
                "",
            )],
        ),
        transcript_case("tool-subagent-background", 2, {
            let mut block = tool("subagent", "explore the repo", ToolState::Success, "");
            if let Block::Tool { background, .. } = &mut block {
                *background = true;
            }
            vec![block]
        }),
        transcript_case(
            "tool-success",
            3,
            vec![tool(
                "shell",
                "cargo test -p ox-tui",
                ToolState::Success,
                "139 passed",
            )],
        ),
        transcript_case("tool-failed", 6, {
            let mut block = tool_with(
                "edit",
                ToolState::Failed,
                serde_json::json!({"path": "src/store.rs"}),
                None,
            );
            if let Block::Tool { output, .. } = &mut block {
                *output = Arc::new("file modified since read".to_owned());
            }
            vec![block]
        }),
        // edit 完成态：diff 块（>120 列为 split，否则 unified）
        transcript_case(
            "edit-diff",
            17,
            vec![tool_with(
                "edit",
                ToolState::Success,
                serde_json::json!({"path": "src/store.rs"}),
                Some(
                    serde_json::json!({"files": [file_diff("src/store.rs", "modified", EDIT_PATCH)]}),
                ),
            )],
        ),
        // patch 完成态：每个文件一个块（新增 / 修改 / 删除），块间空一行
        transcript_case(
            "patch-files",
            31,
            vec![tool_with(
                "patch",
                ToolState::Success,
                serde_json::json!({"patchText": ""}),
                Some(serde_json::json!({"files": [
                    file_diff("src/cache.rs", "added", PATCH_ADD),
                    file_diff("src/store.rs", "modified", EDIT_PATCH),
                    file_diff("src/legacy.rs", "deleted", PATCH_DELETE),
                ]})),
            )],
        ),
        // patch 失败：单一目标时标题带路径，错误文字为 muted
        transcript_case("patch-failed", 7, {
            let mut block = tool_with(
                "patch",
                ToolState::Failed,
                serde_json::json!({"patchText": "*** Begin Patch\n*** Update File: src/store.rs\n@@\n-a\n+b\n*** End Patch"}),
                None,
            );
            if let Block::Tool { output, .. } = &mut block {
                *output = Arc::new("hunk 1 did not match src/store.rs".to_owned());
            }
            vec![block]
        }),
        // write 完成态：带行号的文件内容（语法高亮，按显示宽度折行）
        transcript_case(
            "write-file",
            13,
            vec![tool_with(
                "write",
                ToolState::Success,
                serde_json::json!({"path": "src/cache.rs", "content": WRITE_CONTENT}),
                None,
            )],
        ),
        // question 已答：# Questions 块，问题 muted、答案 base，未答显示 (no answer)
        transcript_case(
            "question-answers",
            12,
            vec![tool_with(
                "question",
                ToolState::Success,
                serde_json::json!({"questions": [
                    {"question": "Which storage backend should the cache use?"},
                    {"question": "Which platforms must the build support?"},
                    {"question": "Anything else to keep in mind?"},
                ]}),
                Some(serde_json::json!({"answers": [["SQLite"], ["linux", "macos"], []]})),
            )],
        ),
        // 工具输出展开：聚焦态提示 + 展开态全文（o / [ ]）
        Case {
            name: "tool-focused",
            height: 3,
            draw: Box::new(|frame, area| {
                let mut t = Transcript::new(area.width, area.height);
                t.sync(
                    &[tool(
                        "shell",
                        "cargo test -p ox-tui",
                        ToolState::Success,
                        "146 passed",
                    )],
                    0,
                );
                t.focus_tool(true);
                t.render(frame, area);
            }),
        },
        Case {
            name: "tool-expanded",
            height: 7,
            draw: Box::new(|frame, area| {
                let mut t = Transcript::new(area.width, area.height);
                t.sync(
                    &[tool(
                        "shell",
                        "cargo test -p ox-tui",
                        ToolState::Success,
                        "running 146 tests\ntest result: ok. 146 passed\nfinished in 0.32s",
                    )],
                    0,
                );
                t.toggle_tool_output();
                t.render(frame, area);
            }),
        },
        // 各工具的单行文案（上游 InlineTool：Read/Grep/Glob/WebFetch/Skill/Generic）
        transcript_case(
            "tool-inline-texts",
            14,
            vec![
                tool_with(
                    "read",
                    ToolState::Success,
                    serde_json::json!({"path": "src/main.rs", "offset": 10, "limit": 20}),
                    None,
                ),
                tool_with(
                    "grep",
                    ToolState::Success,
                    serde_json::json!({"pattern": "fn main", "path": "crates"}),
                    Some(serde_json::json!({"matches": 3})),
                ),
                tool_with(
                    "glob",
                    ToolState::Running,
                    serde_json::json!({"pattern": "**/*.rs"}),
                    None,
                ),
                tool_with(
                    "webfetch",
                    ToolState::Success,
                    serde_json::json!({"url": "https://example.com/docs"}),
                    None,
                ),
                tool_with(
                    "skill",
                    ToolState::Success,
                    serde_json::json!({"id": "rust-expert"}),
                    Some(serde_json::json!({"name": "rust-expert"})),
                ),
                tool_with(
                    "lsp_symbols",
                    ToolState::Success,
                    serde_json::json!({"query": "Transcript", "limit": 5}),
                    None,
                ),
            ],
        ),
        // Explored 折叠行：已完成 / 进行中
        transcript_case(
            "explored-done",
            2,
            vec![Block::Explored(Explored {
                counts: vec![ExploredCount {
                    noun: "read",
                    count: 1,
                }],
                ..Explored::default()
            })],
        ),
        transcript_case(
            "explored-running",
            2,
            vec![Block::Explored(Explored {
                counts: vec![
                    ExploredCount {
                        noun: "read",
                        count: 2,
                    },
                    ExploredCount {
                        noun: "search",
                        count: 1,
                    },
                ],
                running: true,
                ..Explored::default()
            })],
        ),
        // 会话级行：切换、通知、错误、排队压缩
        transcript_case(
            "session-rows",
            10,
            vec![
                Block::Switch(Switch::Agent {
                    agent: Arc::from("build"),
                    previous: Some(Arc::from("plan")),
                }),
                Block::Switch(Switch::Location {
                    directory: Arc::from("/work/next"),
                }),
                Block::Notice(Notice::System { description: None }),
                Block::Error(ErrorInfo {
                    kind: Arc::from("provider"),
                    message: Arc::from("provider overloaded"),
                }),
                Block::CompactionQueued,
            ],
        ),
        // 重试行挂在助手消息页脚上方（上游 index.tsx:2022-2035）
        transcript_case("session-retry", 7, {
            let mut blocks = assistant("Working on it.", "", true);
            if let Some(Block::Assistant {
                footer: Some(footer),
                ..
            }) = blocks.last_mut()
            {
                footer.retry = Some(RetryInfo {
                    attempt: 2,
                    // Fires 5 s after the snapshot is drawn.
                    at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                        + 5_000,
                    message: Arc::from("rate limited"),
                });
            }
            blocks
        }),
        // 完成通知行
        transcript_case(
            "completion-notices",
            8,
            vec![
                completion("n1", CompletionSource::Shell, "completed", "cargo test"),
                completion(
                    "n2",
                    CompletionSource::Shell,
                    "error",
                    "cargo build --release",
                ),
                completion(
                    "n3",
                    CompletionSource::Subagent,
                    "cancelled",
                    "explore the repo",
                ),
            ],
        ),
        // prompt footer
        prompt_case(
            "prompt-home-footer",
            6,
            Footer::Home,
            Some(McpSummary {
                connected: 6,
                failed: 0,
            }),
            Usage::default(),
            false,
            false,
        ),
        prompt_case(
            "prompt-home-footer-failed",
            6,
            Footer::Home,
            Some(McpSummary {
                connected: 6,
                failed: 1,
            }),
            Usage::default(),
            false,
            false,
        ),
        prompt_case(
            "prompt-session-footer",
            6,
            Footer::Session,
            None,
            Usage {
                ctx: Some("16.3k"),
                cost: Some("$0.42"),
            },
            false,
            false,
        ),
        prompt_case(
            "prompt-session-no-ctx",
            6,
            Footer::Session,
            None,
            Usage::default(),
            false,
            false,
        ),
        prompt_case(
            "prompt-running",
            6,
            Footer::Session,
            None,
            Usage::default(),
            true,
            false,
        ),
        prompt_case(
            "prompt-menu",
            24,
            Footer::Session,
            None,
            Usage::default(),
            false,
            true,
        ),
        prompt_files_case(),
        // dock
        dock_case("dock-idle", DataStore::default(), StatusMode::Normal),
        queued_dock_case("dock-queued", DockHover::Off),
        queued_dock_case("dock-queued-hover", DockHover::On),
        dock_case(
            "dock-search",
            DataStore::default(),
            StatusMode::Search {
                query: "cache",
                matches: 12,
                current: Some(2),
            },
        ),
        dock_case("dock-help", DataStore::default(), StatusMode::Help),
        dialog_case("dialog-status"),
        home_case(),
        markdown_case(),
        swatch_case("swatch-truecolor", Deck::Truecolor),
        swatch_case("swatch-ansi16", Deck::Ansi16),
        swatch_case("swatch-nocolor", Deck::NoColor),
        select_case("select-grouped", 30, grouped_select()),
        select_case("select-filtered", 30, {
            let mut select = grouped_select();
            for ch in "ses".chars() {
                select.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            select
        }),
        select_case("select-long-list", 30, {
            let items = (0..40)
                .map(|i| {
                    dialog::Item::new(format!("m{i}"), format!("model-{i}")).category("Anthropic")
                })
                .collect();
            let mut select = dialog::Select::new("Select model", items);
            for _ in 0..14 {
                select.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            select
        }),
        select_case("select-no-results", 20, {
            let mut select = grouped_select();
            for ch in "zzzz".chars() {
                select.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            select
        }),
        toast_case("toast-error", Variant::Error, None, "Directory is required"),
        toast_case(
            "toast-warning-titled",
            Variant::Warning,
            Some("Model unavailable"),
            "The selected model is not connected; pick another with /models.",
        ),
        toast_case(
            "toast-info-wrapped",
            Variant::Info,
            None,
            "Copied to clipboard. This message is deliberately long so that it wraps at the sixty column cap upstream applies to the toast.",
        ),
        toast_case(
            "toast-success",
            Variant::Success,
            None,
            "Configuration reloaded",
        ),
        transcript_case("todo-panel", 14, {
            use ox_client::todo::{Todo, TodoStatus::*};
            let todo = |content: &str, status| Todo {
                content: content.into(),
                status,
            };
            vec![
                Block::user("按计划推进"),
                Block::Todo {
                    id: "t1".into(),
                    version: 0,
                    state: ToolState::Success,
                    todos: Arc::new(vec![
                        todo("Write the failing tests", Completed),
                        todo("Implement the store projection", InProgress),
                        todo("Drop the abandoned spike", Cancelled),
                        todo(
                            "Wire the block into the timeline and check that a very long todo wraps under its text",
                            Pending,
                        ),
                    ]),
                    output: Arc::default(),
                },
            ]
        }),
        // Session sidebar: open on the left, folded on the right.
        Case {
            name: "sidebar",
            height: 12,
            draw: Box::new(|frame, area| {
                use ox_client::todo::{Todo, TodoStatus::*};
                use ox_tui::ui::sidebar::{self, SIDEBAR_WIDTH, TodoOpen};
                let todo = |content: &str, status| Todo {
                    content: content.into(),
                    status,
                };
                let todos = [
                    todo("Write the failing tests", Completed),
                    todo("Implement the store projection", InProgress),
                    todo("Drop the abandoned spike", Cancelled),
                    todo(
                        "Wire the sidebar into the session frame and check that a long todo wraps",
                        Pending,
                    ),
                ];
                let title = "Align the sidebar with upstream";
                let width = area.width.min(SIDEBAR_WIDTH);
                let open = Rect { width, ..area };
                let _ = sidebar::render(title, &todos, TodoOpen::Open, open, frame.buffer_mut());
                let x = area.x + SIDEBAR_WIDTH + 2;
                if x + SIDEBAR_WIDTH <= area.right() {
                    let closed = Rect { x, width, ..area };
                    let _ = sidebar::render(
                        title,
                        &todos,
                        TodoOpen::Closed,
                        closed,
                        frame.buffer_mut(),
                    );
                }
            }),
        },
        transcript_case("todo-inline-states", 8, {
            let row = |id: &str, state| Block::Todo {
                id: id.into(),
                version: 0,
                state,
                todos: Arc::default(),
                output: Arc::new("todowrite: invalid status".into()),
            };
            vec![
                row("t1", ToolState::Running),
                row("t2", ToolState::Success),
                row("t3", ToolState::Failed),
            ]
        }),
        transcript_case("compaction-states", 24, {
            use ox_client::store::CompactionStatus::*;
            let block = |id: &str, status, text: &str, cancelled, tokens| Block::Compaction {
                id: id.into(),
                version: 0,
                status,
                text: Arc::new(text.into()),
                cancelled,
                tokens,
            };
            let usage = ox_schema::support::TokenUsage {
                input: 12_400,
                output: 850,
                reasoning: 0,
                cache: ox_schema::support::TokenCache {
                    read: 3_100,
                    write: 0,
                },
                estimated: None,
            };
            vec![
                block("c1", Running, "", false, None),
                block(
                    "c2",
                    Completed,
                    "## Goal\n- ship the compaction block\n\nDone so far: store and projection.",
                    false,
                    Some(usage),
                ),
                block("c3", Failed, "provider returned 529", false, None),
                block("c4", Failed, "aborted", true, None),
            ]
        }),
        select_case("model-dialog", 30, model_dialog()),
        select_case("agent-dialog", 20, agent_dialog()),
        select_case(
            "command-palette",
            30,
            palette::select(
                palette::Context {
                    in_session: true,
                    has_queue: false,
                },
                keymap::hint,
            ),
        ),
        session_case("sessions-list", 30, |_| {}),
        session_case("sessions-delete-armed", 30, |dialog| {
            dialog.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            dialog.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        }),
        Case {
            name: "sessions-rename",
            height: 20,
            draw: Box::new(|frame, area| {
                dialog::render(
                    frame,
                    area,
                    &session_dialog::rename("ses_a", "Fix the SSE reconnect"),
                )
            }),
        },
        form_case("form-select-30", 30, FormKind::Select),
        form_case("form-multiselect-30", 30, FormKind::Multi),
        form_case("form-text-error-24", 24, FormKind::TextError),
    ]
}

/// `/sessions` over fixed timestamps (2026-10-31 12:00 UTC), after `prepare`.
fn session_case(
    name: &'static str,
    height: u16,
    prepare: impl FnOnce(&mut dialog::Dialog),
) -> Case {
    use ox_client::rest::{SessionSummary, SessionTime};
    const NOW: i64 = 1_793_448_000_000;
    let summary = |id: &str, title: Option<&str>, age_ms: i64| SessionSummary {
        id: id.into(),
        title: title.map(String::from),
        time: SessionTime {
            created: NOW - age_ms,
            updated: NOW - age_ms,
            ..SessionTime::default()
        },
        location: None,
        parent_id: None,
    };
    let sessions = [
        summary("ses_a", Some("Fix the SSE reconnect"), 5 * 60_000),
        summary("ses_b", None, 3 * 3_600_000),
        summary("ses_c", Some("帮我写一个 Rust 解析器"), 2 * 86_400_000),
        summary("ses_d", Some("Parity gold for dialogs"), 9 * 86_400_000),
    ];
    let mut dialog = session_dialog::list(
        &sessions,
        Some("ses_a"),
        jiff::Timestamp::from_millisecond(NOW).expect("fixed instant"),
        &jiff::tz::TimeZone::UTC,
    );
    prepare(&mut dialog);
    Case {
        name,
        height,
        draw: Box::new(move |frame, area| dialog::render(frame, area, &dialog)),
    }
}

fn model_dialog() -> dialog::Select {
    use ox_client::rest::{ModelInfo, ProviderRef};
    let model = |provider: &str, id: &str, name: &str| ModelInfo {
        model_id: id.into(),
        provider_id: provider.into(),
        name: name.into(),
        enabled: true,
        limit: Default::default(),
    };
    let provider = |id: &str, name: &str| ProviderRef {
        id: id.into(),
        name: name.into(),
    };
    ox_tui::ui::models::model_select(
        &[
            model("openai", "gpt-5", "GPT-5"),
            model("anthropic", "sonnet", "Claude Sonnet"),
            model("anthropic", "opus", "Claude Opus"),
            model("openai", "gpt-5-mini", "GPT-5 mini"),
        ],
        &[
            provider("openai", "OpenAI"),
            provider("anthropic", "Anthropic"),
        ],
        Some("openai/gpt-5".into()),
    )
}

fn agent_dialog() -> dialog::Select {
    use ox_client::rest::AgentRef;
    let agent = |id: &str| AgentRef {
        id: id.into(),
        name: id.into(),
        mode: Some("primary".into()),
        hidden: false,
    };
    ox_tui::ui::models::agent_select(&[agent("build"), agent("plan")], Some("Plan"))
}

/// 对话区组件统一走公开入口：Transcript::sync + render。
fn transcript_case(name: &'static str, height: u16, blocks: Vec<Block>) -> Case {
    Case {
        name,
        height,
        draw: Box::new(move |frame, area| {
            let mut t = Transcript::new(area.width, area.height);
            t.sync(&blocks, 0);
            t.render(frame, area);
        }),
    }
}

/// Buffer → ANSI：逐 cell 比较 fg/bg 差量输出 SGR，行末 reset。
/// 只用 SGR，不含光标移动——天然适合终端滚动回看和复制。
/// （ratatui 0.30 起 Cell 的颜色是 `Color`，终端默认色即 `Color::Reset`。）
///
/// 宽字形的尾 cell 必须整格跳过（不输出任何字节）：ratatui 0.30 的渲染路径
/// （`Buffer::set_stringn`）把尾 cell `.reset()` 成空格、且从不设置
/// `skip`/`diff_option`（已实证），所以唯一的可靠判据是跟在上一格字形显示
/// 宽度后面的格子——按宽度走查，输出流的显示宽度才与 Buffer 一致。
fn buffer_to_ansi(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut tail = 0u16;
        for x in buffer.area.x..buffer.area.right() {
            let cell = &buffer[(x, y)];
            if tail > 0 {
                tail -= 1;
                continue;
            }
            if cell.fg != fg {
                push_color(&mut out, "38", cell.fg);
                fg = cell.fg;
            }
            if cell.bg != bg {
                push_color(&mut out, "48", cell.bg);
                bg = cell.bg;
            }
            out.push_str(cell.symbol());
            tail = cell.symbol().cell_width().saturating_sub(1);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

/// 命名色统一按 xterm-256 索引落盘（Black=0 … White=15，与 ratatui 的 SGR
/// 映射一一对应），终端按自己的 16 色主题着色——Ansi16 deck 的色卡才可见。
fn base_index(color: Color) -> Option<u8> {
    Some(match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        _ => return None,
    })
}

fn push_color(out: &mut String, channel: &str, color: Color) {
    let reset = if channel == "38" { "39" } else { "49" };
    match color {
        Color::Indexed(i) => {
            out.push_str(&format!("\x1b[{channel};5;{i}m"));
        }
        Color::Rgb(r, g, b) => {
            out.push_str(&format!("\x1b[{channel};2;{r};{g};{b}m"));
        }
        named => match base_index(named) {
            Some(i) => out.push_str(&format!("\x1b[{channel};5;{i}m")),
            None => out.push_str(&format!("\x1b[{reset}m")),
        },
    }
}

// ---- mock 构造器 ----

/// 一条助手消息拆成的块：非空 reasoning 的 `+ Thought` 头、正文，done 时带
/// turn footer（上游 `AssistantFooter`）。
fn assistant(text: &str, reasoning: &str, done: bool) -> Vec<Block> {
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(Block::reasoning("msg_1", 1, reasoning, done));
    }
    blocks.push(Block::Assistant {
        id: "msg_1".into(),
        version: 1,
        text: Arc::new(text.to_owned()),
        footer: done.then(|| TurnFooter {
            agent: Some("build".into()),
            model: Some("provider/model".into()),
            duration_ms: Some(4_200),
            tokens_per_second: Some(70.0),
            error: None,
            retry: None,
        }),
    });
    blocks
}

/// The store-derived flags of a shell block.
#[derive(Default)]
struct Live {
    shell: bool,
    background: bool,
    running: bool,
}

fn shell(state: ToolState, input: serde_json::Value, output: &str, live: Live) -> Block {
    let serde_json::Value::Object(map) = input else {
        panic!("gallery shell input must be a JSON object, got {input}");
    };
    Block::Shell {
        id: "part_sh".into(),
        version: 1,
        state,
        input: Arc::new(map.into_iter().collect()),
        output: Arc::new(output.to_owned()),
        omitted: false,
        shell: live.shell,
        background: live.background,
        shell_running: live.running,
    }
}

fn long_output() -> String {
    (1..=30).map(|n| format!("line {n}\n")).collect()
}

fn tool(tool: &str, summary: &str, state: ToolState, output: &str) -> Block {
    Block::Tool {
        id: "part_1".into(),
        version: 1,
        tool: tool.into(),
        state,
        summary: summary.into(),
        output: Arc::new(output.to_owned()),
        background: false,
        input: Arc::default(),
        metadata: None,
    }
}

/// A tool row carrying real input and result metadata, the way `tool_text`
/// reads them; `input`/`metadata` must be JSON objects.
fn tool_with(
    tool: &str,
    state: ToolState,
    input: serde_json::Value,
    metadata: Option<serde_json::Value>,
) -> Block {
    let record = |value: serde_json::Value| -> Arc<Record> {
        let serde_json::Value::Object(map) = value else {
            panic!("gallery tool input must be a JSON object, got {value}");
        };
        Arc::new(map.into_iter().collect())
    };
    Block::Tool {
        id: format!("part_{tool}"),
        version: 1,
        tool: tool.into(),
        state,
        summary: String::new(),
        output: Arc::default(),
        background: false,
        input: record(input),
        metadata: metadata.map(record),
    }
}

/// An edit's patch as ox's `render_patch` writes it: two hunks, the second
/// with a line long enough to wrap.
const EDIT_PATCH: &str = "--- src/store.rs\n+++ src/store.rs\n\
@@ -3,4 +3,4 @@\n use std::sync::Arc;\n \n-pub fn load(path: &str) -> Store {\n+pub fn load(path: &Path) -> Result<Store> {\n     let raw = read(path);\n\
@@ -40,3 +40,4 @@\n     store.flush();\n+    tracing::debug!(entries = store.len(), \"flushed the store to disk after compaction\");\n     Ok(())\n }\n";

const PATCH_ADD: &str =
    "--- /dev/null\n+++ src/cache.rs\n@@ -0,0 +1,3 @@\n+pub struct Cache {\n+    hits: u64,\n+}\n";

const PATCH_DELETE: &str =
    "--- src/legacy.rs\n+++ src/legacy.rs\n@@ -1,2 +0,0 @@\n-fn old() {}\n-fn older() {}\n";

const WRITE_CONTENT: &str = "use std::collections::HashMap;\n\n/// Hit counts per key, kept until the next compaction pass rewrites the store on disk.\npub struct Cache {\n    hits: HashMap<String, u64>,\n}\n";

/// One `metadata.files` entry in ox's wire shape (`ox-tools` `FileDiff`).
fn file_diff(file: &str, status: &str, patch: &str) -> serde_json::Value {
    let count = |sign: char| {
        patch
            .lines()
            .filter(|line| {
                line.starts_with(sign) && !line.starts_with("+++") && !line.starts_with("---")
            })
            .count()
    };
    serde_json::json!({
        "file": file,
        "status": status,
        "patch": patch,
        "additions": count('+'),
        "deletions": count('-'),
    })
}

// ---- chrome mock ----

/// `/status` 弹窗：上游 `dialog-status.tsx` 的 MCP 列表，含三种状态。
fn dialog_case(name: &'static str) -> Case {
    Case {
        name,
        height: 12,
        draw: Box::new(|frame, area| {
            let servers = vec![
                McpRef {
                    name: "context7".into(),
                    status: McpStatus::Connected,
                },
                McpRef {
                    name: "zread".into(),
                    status: McpStatus::Connected,
                },
                McpRef {
                    name: "web-reader".into(),
                    status: McpStatus::Pending,
                },
                McpRef {
                    name: "old-server".into(),
                    status: McpStatus::Failed {
                        error: "connection refused".into(),
                    },
                },
            ];
            dialog::render(frame, area, &dialog::status(&servers));
        }),
    }
}

/// Select 弹窗：上游 `ui/dialog-select.tsx`，带分组、右侧 footer、描述和底部提示。
fn select_case(name: &'static str, height: u16, select: dialog::Select) -> Case {
    let dialog = dialog::Dialog::Select(select);
    Case {
        name,
        height,
        draw: Box::new(move |frame, area| dialog::render(frame, area, &dialog)),
    }
}

fn grouped_select() -> dialog::Select {
    dialog::Select::new(
        "Switch session",
        vec![
            dialog::Item::new("s1", "Refactor the session store")
                .category("Today")
                .footer("10:42"),
            dialog::Item::new("s2", "Fix the SSE reconnect")
                .category("Today")
                .description("2 files")
                .footer("09:15"),
            dialog::Item::new("s3", "Parity gold for dialogs")
                .category("Yesterday")
                .footer("21:03"),
            dialog::Item::new("s4", "帮我写一个 Rust 解析器")
                .category("Yesterday")
                .footer("14:20"),
        ],
    )
    .hints(vec![
        dialog::Hint::new("Delete", "ctrl+d"),
        dialog::Hint::new("Rename", "ctrl+r"),
    ])
}

fn chrome(mcp: Option<McpSummary>) -> Chrome {
    Chrome {
        agent: Some("Build".into()),
        model: Some("k3-256k".into()),
        provider: Some("Cardinal".into()),
        variant: None,
        directory: "~/Developer/openkitbox/ox".into(),
        branch: Some("main".into()),
        mcp,
    }
}

/// prompt 用例按高度收纳：块本体 6 行（面板 4 + ╹▀ 边 + footer），菜单用例
/// 需要上方空间画浮层，保持 24。
/// 右段用量：session footer 的 ctx / cost，按出现顺序渲染。
#[derive(Default, Clone, Copy)]
struct Usage {
    ctx: Option<&'static str>,
    cost: Option<&'static str>,
}

fn prompt_case(
    name: &'static str,
    height: u16,
    footer: Footer,
    mcp: Option<McpSummary>,
    usage: Usage,
    running: bool,
    slash: bool,
) -> Case {
    Case {
        name,
        height,
        draw: Box::new(move |frame, area| {
            let mut prompt = Prompt::new("Ask anything…", chrome(mcp), footer);
            if let Some(ctx) = usage.ctx {
                prompt.set_ctx(Some(ctx.to_owned()));
            }
            if let Some(cost) = usage.cost {
                prompt.set_cost(Some(cost.to_owned()));
            }
            if slash {
                let _ = prompt.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
            }
            let h = prompt.height(area.height);
            let panel_area = Rect {
                y: area.height - h,
                height: h,
                ..area
            };
            let activity = if running {
                Activity::Turn
            } else {
                Activity::Idle
            };
            prompt.render(frame, panel_area, area, activity);
        }),
    }
}

/// The `@` file menu with a canned `fs/find` answer (no upstream gold yet).
fn prompt_files_case() -> Case {
    Case {
        name: "prompt-files",
        height: 24,
        draw: Box::new(|frame, area| {
            let mut prompt = Prompt::new("Ask anything…", chrome(None), Footer::Session);
            for ch in "look at @sr".chars() {
                let _ = prompt.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            let _ = prompt.take_file_request();
            let entry = |path: &str, kind: &str| ox_client::rest::FileEntry {
                path: path.into(),
                kind: kind.into(),
            };
            prompt.set_files(
                "sr",
                &[
                    entry("src/main.rs", "file"),
                    entry("src/", "directory"),
                    entry("crates/ox-tui/src/", "directory"),
                    entry("README.md", "file"),
                ],
            );
            let h = prompt.height(area.height);
            let panel_area = Rect {
                y: area.height - h,
                height: h,
                ..area
            };
            prompt.render(frame, panel_area, area, Activity::Idle);
        }),
    }
}

fn dock_case(name: &'static str, store: DataStore, mode: StatusMode<'static>) -> Case {
    // StatusMode 不 impl Copy/Clone 且不能改库：构建期先把 Line 算好（status_line
    // 只读 store/mode），闭包里只剩渲染。
    let line = status_line(&store, mode);
    Case {
        name,
        height: 1,
        draw: Box::new(move |frame, area| {
            Paragraph::new(line.clone()).render(area, frame.buffer_mut());
        }),
    }
}

/// The queued-prompt dock over two prompts, the first one multi-line.
fn queued_dock_case(name: &'static str, hover: DockHover) -> Case {
    let queued = vec![
        Queued {
            inbox_id: "in_1".into(),
            text: Arc::from("run the tests again\nthen fix what breaks"),
        },
        Queued {
            inbox_id: "in_2".into(),
            text: Arc::from("q2"),
        },
    ];
    Case {
        name,
        height: 3,
        draw: Box::new(move |frame, area| {
            let lines = queued_dock(&queued, area.width, hover);
            Paragraph::new(lines).render(area, frame.buffer_mut());
        }),
    }
}

// ---- home / markdown / 调色板色卡 ----

fn home_case() -> Case {
    Case {
        name: "home",
        height: 30,
        draw: Box::new(|frame, area| {
            // Injected state: placeholder #0 and a healthy 4-server MCP set, so
            // the frame is the one the parity test pins against upstream.
            let mut prompt = Prompt::new(
                &placeholder_line(0),
                Chrome {
                    directory: "/private/tmp/parity-cwd".into(),
                    branch: None,
                    ..chrome(Some(McpSummary {
                        connected: 4,
                        failed: 0,
                    }))
                },
                Footer::Home,
            );
            home::render(frame, area, &mut prompt);
        }),
    }
}

fn markdown_case() -> Case {
    Case {
        name: "markdown-samples",
        height: 24,
        draw: Box::new(|frame, area| {
            let text = "# Title\n- first\n- second\n> quoted\n---\nplain\n```rust\nlet a = 1;\n```\n\n\
                | name | 说明 | notes |\n|:--|:-:|--:|\n\
                | foo | `bar` | short |\n\
                | 中文列 | **bold** | a deliberately long cell whose text must wrap onto several rows once the table runs out of horizontal room, 这一格故意写得很长以便观察换行 |";
            let lines = markdown::render(text, area.width, 3);
            Paragraph::new(lines).render(area, frame.buffer_mut());
        }),
    }
}

/// 三种 Deck 各一行：同一段色块串用各自的 Palette 渲染，肉眼比对降级路径。
fn swatch_case(name: &'static str, deck: Deck) -> Case {
    Case {
        name,
        height: 1,
        draw: Box::new(move |frame, area| {
            let pal = Palette::for_deck(deck);
            let line = Line::from(vec![
                Span::raw(format!("{deck:?} ")),
                Span::styled("████ text ", pal.text),
                Span::styled("████ dim ", pal.dim),
                Span::styled("████ accent ", pal.accent),
                Span::styled("████ warn ", pal.warn),
                Span::styled("████ agent ", pal.agent),
                Span::styled("  panel  ", pal.panel),
                Span::styled("  menu  ", pal.menu),
                Span::styled("  selected  ", pal.menu_selected),
            ]);
            Paragraph::new(line).render(area, frame.buffer_mut());
        }),
    }
}

/// 右上角 toast：沿用真实 `toast::render`，只给一个固定过期时间的值。
fn toast_case(
    name: &'static str,
    variant: Variant,
    title: Option<&'static str>,
    message: &'static str,
) -> Case {
    Case {
        name,
        height: 9,
        draw: Box::new(move |frame, area| {
            let toast = Toast::new(
                variant,
                title.map(String::from),
                message,
                std::time::Instant::now(),
            );
            toast::render(
                frame.buffer_mut(),
                area,
                &toast,
                &Palette::for_deck(Deck::Truecolor),
            );
        }),
    }
}

enum FormKind {
    Select,
    Multi,
    TextError,
}

/// 表单对话框：真实 `FormState` + 真实键盘路径，画在底部（上游把表单放在 prompt 的位置）。
fn form_case(name: &'static str, height: u16, kind: FormKind) -> Case {
    use ox_client::form::FormInfo;
    use ox_tui::ui::form::{FormState, draw};
    Case {
        name,
        height,
        draw: Box::new(move |frame, area| {
            let fields = match kind {
                FormKind::Select => serde_json::json!([{
                    "type": "string", "key": "stack", "title": "Stack",
                    "description": "Which stack do you want?",
                    "options": [
                        {"value": "rust", "label": "Rust", "description": "fast and safe"},
                        {"value": "go", "label": "Go"}
                    ],
                    "custom": true
                }]),
                FormKind::Multi => serde_json::json!([
                    {"type": "multiselect", "key": "tags", "title": "Tags", "required": true,
                     "description": "Which tags apply?",
                     "options": [
                        {"value": "bug", "label": "Bug", "description": "something is broken"},
                        {"value": "ui", "label": "UI"}
                     ], "custom": true},
                    {"type": "boolean", "key": "ok", "title": "Notify?"}
                ]),
                FormKind::TextError => serde_json::json!([
                    {"type": "integer", "key": "n", "title": "Count", "minimum": 1, "maximum": 5,
                     "description": "How many?"},
                    {"type": "boolean", "key": "ok", "title": "Notify?"}
                ]),
            };
            let info: FormInfo = serde_json::from_value(serde_json::json!({
                "id": "frm_gallery", "sessionID": "ses_gallery", "title": "Pick a stack",
                "fields": fields
            }))
            .expect("the gallery form sample is a valid FormInfo");
            let mut form = FormState::new(&info);
            let press = |form: &mut FormState, code: KeyCode| {
                form.on_key(KeyEvent::new(code, KeyModifiers::NONE));
            };
            match kind {
                FormKind::Select => {}
                FormKind::Multi => {
                    press(&mut form, KeyCode::Char(' '));
                    press(&mut form, KeyCode::Down);
                }
                FormKind::TextError => {
                    for c in "9".chars() {
                        press(&mut form, KeyCode::Char(c));
                    }
                    press(&mut form, KeyCode::Enter);
                }
            }
            draw(
                frame.buffer_mut(),
                area,
                &form,
                &Palette::for_deck(Deck::Truecolor),
            );
        }),
    }
}

fn completion(id: &str, source: CompletionSource, state: &str, description: &str) -> Block {
    Block::Completion {
        id: id.into(),
        source,
        state: Some(Arc::from(state)),
        description: Arc::from(description),
        child_id: None,
    }
}
