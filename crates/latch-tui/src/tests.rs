use super::chrome::*;
use super::markdown::*;
use super::runtime::{enter_screen, leave_screen};
use super::transcript::*;
use super::*;

fn assistant(text: &str) -> TranscriptItem {
    TranscriptItem::Assistant {
        text: text.into(),
        streaming: false,
    }
}

fn app_with(items: Vec<TranscriptItem>) -> App {
    App {
        items,
        ..App::default()
    }
}

// ---- transcript scrolling (visual-row mechanics, preserved) ----

#[test]
fn empty_transcript_has_no_height() {
    assert_eq!(visual_height(&[], 80), 0);
}

#[test]
fn long_wrapped_message_height_tracks_width() {
    let items = vec![assistant(&"word ".repeat(200))];
    let wide = visual_height(&items, 80);
    let narrow = visual_height(&items, 20);
    assert!(narrow > wide, "narrow {narrow} should exceed wide {wide}");
    assert!(narrow > 20);
}

#[test]
fn single_message_taller_than_viewport_stays_in_bounds() {
    let items = vec![assistant(&"tall ".repeat(80))];
    let content = visual_height(&items, 10);
    assert!(content > 3);
    let mut app = app_with(items);
    app.sync_viewport(content, 3);
    assert!(app.follow);
    assert_eq!(app.scroll, content - 3);
    app.scroll_up(3);
    assert!(!app.follow);
    assert_eq!(app.scroll, content - 6);
    app.scroll_down(3);
    assert!(app.follow);
    assert_eq!(app.scroll, content - 3);
}

#[test]
fn page_up_and_page_down_respect_bounds() {
    let mut app = app_with(vec![assistant(&"x ".repeat(100))]);
    let content = visual_height(&app.items, 10);
    app.sync_viewport(content, 4);
    let bottom = app.scroll;
    app.scroll_up(4);
    assert_eq!(app.scroll, bottom - 4);
    assert!(!app.follow);
    app.scroll_down(4);
    assert_eq!(app.scroll, bottom);
    assert!(app.follow);
    app.scroll_up(usize::MAX);
    assert_eq!(app.scroll, 0);
    app.scroll_down(usize::MAX);
    assert_eq!(app.scroll, app.max_scroll());
    assert!(app.follow);
}

#[test]
fn home_and_end_jump_to_edges() {
    let mut app = app_with(vec![assistant(&"y ".repeat(100))]);
    let content = visual_height(&app.items, 10);
    app.sync_viewport(content, 4);
    app.scroll_up(2);
    assert!(!app.follow);
    app.scroll_home();
    assert_eq!(app.scroll, 0);
    assert!(!app.follow);
    app.scroll_end();
    assert_eq!(app.scroll, app.max_scroll());
    assert!(app.follow);
}

#[test]
fn home_keeps_following_when_everything_fits() {
    let mut app = app_with(vec![assistant("short")]);
    app.sync_viewport(1, 5);
    app.scroll_home();
    assert_eq!(app.scroll, 0);
    assert!(app.follow);
}

#[test]
fn auto_follow_pins_to_new_output_at_bottom() {
    let mut app = app_with(vec![assistant("short")]);
    app.sync_viewport(1, 5);
    assert!(app.follow);
    app.items.push(assistant(&"more ".repeat(50)));
    let rows = visual_height(&app.items, 10);
    app.sync_viewport(rows, 5);
    assert!(app.follow);
    assert_eq!(app.scroll, rows - 5);
}

#[test]
fn manual_scroll_is_not_yanked_back_to_bottom() {
    let mut app = app_with(vec![assistant(&"a ".repeat(100))]);
    let rows = visual_height(&app.items, 10);
    app.sync_viewport(rows, 4);
    app.scroll_up(3);
    let held = app.scroll;
    assert!(!app.follow);
    app.items.push(assistant(&"b ".repeat(100)));
    let rows = visual_height(&app.items, 10);
    app.sync_viewport(rows, 4);
    assert_eq!(app.scroll, held);
    assert!(!app.follow);
}

#[test]
fn resize_recomputes_rows_and_clamps_offset() {
    let mut app = app_with(vec![assistant(&"resize ".repeat(100))]);
    let narrow = visual_height(&app.items, 12);
    let wide = visual_height(&app.items, 60);
    assert!(narrow > wide);
    app.sync_viewport(narrow, 5);
    app.scroll_home();
    app.scroll_down(2);
    let held = app.scroll;
    assert!(!app.follow);
    app.sync_viewport(wide, 5);
    assert!(app.scroll <= app.max_scroll());
    assert_eq!(app.scroll, held.min(app.max_scroll()));
}

#[test]
fn unicode_content_wraps_without_panicking() {
    let items = vec![assistant(&"你好世界".repeat(60))];
    let narrow = visual_height(&items, 8);
    let wide = visual_height(&items, 80);
    assert!(narrow > wide);
    let mut app = app_with(items);
    app.sync_viewport(narrow, 3);
    assert!(app.scroll <= app.max_scroll());
    app.scroll_home();
    assert_eq!(app.scroll, 0);
    app.scroll_end();
    assert_eq!(app.scroll, app.max_scroll());
    assert!(app.follow);
}

#[test]
fn embedded_newlines_count_as_separate_rows() {
    let items = vec![assistant("first line\nsecond line\nthird line")];
    assert_eq!(visual_height(&items, 40), 3);
}

// ---- typed items and tool lifecycle ----

#[test]
fn tool_activity_upserts_one_row_by_call_id() {
    let mut app = App::default();
    app.apply_item(DisplayItem::ToolActivity {
        call_id: "call-1".into(),
        verb: "patch".into(),
        target: "calc.py".into(),
        detail: String::new(),
        status: ToolRunStatus::Running,
    });
    assert_eq!(app.items.len(), 1);
    app.apply_item(DisplayItem::ToolActivity {
        call_id: "call-1".into(),
        verb: "patch".into(),
        target: "calc.py".into(),
        detail: "+1 -1".into(),
        status: ToolRunStatus::Passed,
    });
    assert_eq!(app.items.len(), 1, "one lifecycle row per call");
    match &app.items[0] {
        TranscriptItem::Tool(row) => {
            assert_eq!(row.status, ToolRunStatus::Passed);
            assert_eq!(row.detail, "+1 -1");
        }
        other => panic!("expected tool row, got {other:?}"),
    }
}

#[test]
fn display_items_map_to_typed_rows() {
    let mut app = App::default();
    app.apply_item(DisplayItem::UserMessage {
        text: "hi".into(),
        media: vec![],
    });
    app.apply_item(DisplayItem::AssistantMessage {
        text: "hello".into(),
    });
    app.apply_item(DisplayItem::KernelNotice {
        text: "resumed".into(),
    });
    app.apply_item(DisplayItem::Error {
        text: "boom".into(),
    });
    assert_eq!(
        app.items,
        vec![
            TranscriptItem::User { text: "hi".into() },
            TranscriptItem::Assistant {
                text: "hello".into(),
                streaming: false
            },
            TranscriptItem::Notice {
                text: "resumed".into()
            },
            TranscriptItem::Error {
                text: "boom".into()
            },
        ]
    );
}

#[test]
fn streaming_assistant_renders_once_after_done() {
    let mut app = App::default();
    app.output(Output::AssistantDelta("he".into()));
    app.output(Output::AssistantDelta("llo".into()));
    assert!(matches!(
        app.items.last(),
        Some(TranscriptItem::Assistant {
            streaming: true,
            ..
        })
    ));
    app.output(Output::AssistantDone);
    assert!(matches!(
        app.items.last(),
        Some(TranscriptItem::Assistant {
            streaming: false,
            ..
        })
    ));
}

// ---- markdown rendering ----

#[test]
fn markdown_renders_headings_bullets_and_code() {
    let text = "# Title\n\n- bullet `code`\n```py\nx = 1\n```\n1. first\nplain **bold**";
    let lines = render_markdown_at(text, MARKDOWN_DEFAULT_WIDTH);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert_eq!(rendered[0], "Title");
    assert!(rendered[2].contains("• bullet"));
    assert!(rendered[2].contains("code"));
    assert_eq!(rendered[3], "  │ x = 1");
    assert!(rendered[4].contains("1. first"));
    assert!(rendered[5].contains("plain"));
    // Heading and code carry their own styles at the line level.
    assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(lines[3].style.fg, Some(Color::Cyan));
}

#[test]
fn markdown_hides_fence_markers() {
    let lines = render_markdown_at("```\nhello\n```", MARKDOWN_DEFAULT_WIDTH);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert_eq!(rendered.len(), 1);
    assert!(rendered[0].contains("hello"));
    assert!(!rendered[0].contains("```"));
}

#[test]
fn markdown_renders_links_italics_urls_and_tables() {
    let lines = render_markdown_at(
        "*note* [Latch](https://example.test)\n\n| A | B |\n|---|---|\n| 你 | https://example.test/x |",
        MARKDOWN_DEFAULT_WIDTH,
    );
    let rendered = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(rendered.contains("note"));
    assert!(rendered.contains("Latch (https://example.test)"));
    assert!(rendered.contains("A"));
    assert!(rendered.contains("你"));
    assert!(!rendered.contains("|---"), "separator rows are not literal");
    assert!(
        !rendered.contains("│ A │ B │"),
        "tables are aligned, not raw"
    );
    assert!(
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.add_modifier.contains(Modifier::ITALIC))
    );
    assert!(
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
    );
}

#[test]
fn markdown_tables_align_columns_and_hide_separators() {
    let lines = render_markdown_at(
        "| Name | Qty |\n|:-----|----:|\n| apple | 12 |\n| kiwi | 3 |",
        40,
    );
    assert_eq!(
        lines_text(&lines),
        "Name   Qty\n──────────\napple   12\nkiwi     3"
    );
    // The rule after the header is dim and bold header cells stay bold.
    assert_eq!(lines[0].spans[0].style.add_modifier, Modifier::BOLD);
    assert!(lines[1].style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn markdown_tables_wrap_wide_cells_within_the_width() {
    let lines = render_markdown_at(
        "| Feature | Description |\n|---|---|\n| alpha | a moderately long description that wraps |",
        30,
    );
    for line in &lines {
        let text = lines_text(std::slice::from_ref(line));
        assert!(
            display_width(&text) <= 30,
            "table line {text:?} exceeds the viewport"
        );
    }
    assert!(lines.len() > 4, "the wide cell wrapped onto extra rows");
    assert!(lines_text(&lines).contains("alpha"));
}

#[test]
fn markdown_tables_measure_cjk_by_display_width() {
    let lines = render_markdown_at("| 名称 | 数量 |\n|---|---|\n| 苹果 | 12 |", 40);
    assert_eq!(lines_text(&lines), "名称  数量\n──────────\n苹果  12  ");
    for line in &lines {
        let text = lines_text(std::slice::from_ref(line));
        assert_eq!(display_width(&text), 10);
    }
}

#[test]
fn markdown_tables_wrap_cjk_cells_without_splitting_characters() {
    // A narrow table forces the CJK cells to wrap; the renderer must
    // convert the composer's char offsets to byte offsets before slicing.
    let lines = render_markdown_at(
        "| 模块 | 职责 |\n|---|---|\n| 编辑器 | 可滚动的多行输入视口 |",
        24,
    );
    // Header, rule, then the wrapped body cell spans two rows.
    assert_eq!(lines.len(), 4, "{}", lines_text(&lines));
    let rendered = lines_text(&lines);
    assert!(
        !rendered
            .lines()
            .any(|line| line.contains("可滚动的多行输入视口")),
        "the wide CJK cell must wrap: {rendered}"
    );
    for line in &lines {
        let text = lines_text(std::slice::from_ref(line));
        assert!(
            display_width(&text) <= 24,
            "line {text:?} exceeds the viewport"
        );
        assert!(!text.contains('\u{FFFD}'), "no split characters: {text:?}");
    }
    assert!(rendered.contains("可滚"), "{rendered}");
    assert!(rendered.contains("视口"), "{rendered}");
}

#[test]
fn malformed_pipe_content_keeps_the_raw_treatment() {
    let lines = render_markdown_at("| a | b |\nno separator here", MARKDOWN_DEFAULT_WIDTH);
    let rendered = lines_text(&lines);
    assert!(rendered.contains("│ a │ b │"), "{rendered}");
    assert!(!rendered.contains('─'), "{rendered}");

    // A single-column pipe line is not an ordinary table.
    let single = render_markdown_at("| solo |\n|---|", MARKDOWN_DEFAULT_WIDTH);
    let rendered = lines_text(&single);
    assert!(rendered.contains("│ solo │"), "{rendered}");
    assert!(!rendered.contains("---"), "{rendered}");
}

// ---- input editor ----

fn editor_with(text: &str) -> Composer {
    let mut editor = Composer::new();
    for ch in text.chars() {
        editor.insert(ch);
    }
    editor
}

#[test]
fn cursor_edits_behave_like_a_line_editor() {
    let mut editor = editor_with("hello");
    assert_eq!(editor.cursor(), (0, 5));
    for _ in 0..3 {
        editor.left();
    }
    assert_eq!(editor.cursor(), (0, 2));
    editor.insert('X');
    assert_eq!(editor.text(), "heXllo");
    editor.backspace();
    assert_eq!(editor.text(), "hello");
    editor.line_home();
    editor.insert('a');
    assert_eq!(editor.text(), "ahello");
    editor.line_end();
    editor.delete();
    assert_eq!(editor.text(), "ahello");
    editor.left();
    editor.delete();
    assert_eq!(editor.text(), "ahell");
}

#[test]
fn ctrl_a_e_w_and_kill_keys_edit_deterministically() {
    let mut editor = editor_with("alpha beta gamma");
    editor.line_end();
    editor.kill_word();
    assert_eq!(editor.text(), "alpha beta ");
    editor.kill_word();
    assert_eq!(editor.text(), "alpha ");
    editor.line_home();
    editor.kill_to_line_end();
    assert!(editor.is_empty());
    let mut editor = editor_with("keep this");
    editor.line_end();
    editor.left();
    editor.kill_to_line_start();
    assert_eq!(editor.text(), "s");
}

#[test]
fn multiline_split_and_merge() {
    let mut editor = editor_with("abc");
    editor.line_home();
    for _ in 0..2 {
        editor.right();
    }
    editor.newline();
    assert_eq!(editor.text(), "ab\nc");
    assert_eq!(editor.cursor(), (1, 0));
    editor.backspace();
    assert_eq!(editor.text(), "abc");
    assert_eq!(editor.cursor(), (0, 2));
    let mut editor = editor_with("one");
    editor.line_end();
    editor.newline();
    editor.insert('!');
    assert_eq!(editor.text(), "one\n!");
    editor.delete();
    assert_eq!(editor.text(), "one\n!");
    editor.left();
    editor.down(40);
    assert_eq!(editor.cursor(), (1, 0));
}

#[test]
fn input_wraps_within_bounded_rows() {
    let editor = editor_with(&"word ".repeat(60));
    let rows = editor.layout(20);
    assert!(rows.len() > 1);
    assert!(rows.len() < 40);
    let (row, col) = editor.cursor_visual(&rows);
    assert!(row > 0);
    assert!(col <= 20);
    // Every visual row stays inside the width and the complete buffer is
    // reachable by concatenating rows in order.
    let joined: String = rows
        .iter()
        .map(|row| editor.row_text(row))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(joined, editor.text());
}

#[test]
fn multiline_wrapping_accounts_for_every_line() {
    let mut editor = Composer::new();
    for ch in "first line here\nsecond much longer line that wraps around a narrow width".chars() {
        editor.insert(ch);
    }
    let (row, _) = editor.cursor_visual(&editor.layout(20));
    assert!(row >= 1, "cursor sits on the second logical line");
}

// ---- prompt history ----

#[test]
fn history_recalls_without_mutating_and_restores_draft() {
    let mut editor = Composer::new();
    editor.seed_history(vec!["first".into(), "second".into()]);
    editor.insert('x');
    editor.history_previous();
    assert_eq!(editor.text(), "second");
    editor.history_previous();
    assert_eq!(editor.text(), "first");
    // Editing a recalled entry must not mutate stored history.
    editor.insert('!');
    assert_eq!(editor.text(), "first!");
    assert_eq!(editor.history, vec!["first", "second"]);
    editor.history_next();
    assert_eq!(editor.text(), "second");
    editor.history_next();
    assert_eq!(editor.text(), "x", "draft restored past newest entry");
    editor.history_previous();
    assert_eq!(editor.text(), "second");
}

#[test]
fn submit_records_history_and_resets() {
    let mut editor = Composer::new();
    editor.insert('h');
    editor.insert('i');
    assert_eq!(editor.take_for_submit(), "hi");
    assert!(editor.is_empty());
    assert_eq!(editor.history, vec!["hi"]);
    editor.insert('h');
    editor.insert('i');
    editor.take_for_submit();
    assert_eq!(
        editor.history,
        vec!["hi"],
        "no duplicate consecutive history"
    );
}

// ---- slash palette ----

#[test]
fn palette_filters_by_prefix() {
    let mo = filter_commands("/mod");
    let names: Vec<&str> = mo.iter().map(|command| command.name).collect();
    assert_eq!(names, vec!["/mode", "/model"]);
    assert!(
        filter_commands("/perm")
            .iter()
            .any(|command| command.name == "/permissions")
    );
    assert!(
        filter_commands("/safe")
            .iter()
            .any(|command| command.name == "/safety")
    );
    assert!(filter_commands("/mod").iter().any(|c| c.name == "/mode"));
    assert!(filter_commands("/und").iter().any(|c| c.name == "/undo"));
    assert!(filter_commands("/zzz").is_empty());
    assert_eq!(filter_commands("/").len(), SLASH_COMMANDS.len());
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn palette_opens_completes_and_closes() {
    let mut app = App::default();
    for ch in "/mod".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(app.palette.active(&app.input));
    // Enter dispatches the selected command directly.
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Some(Action::Submit { ref text, .. }) if text.trim() == "/mode"));
    // Typing again reopens; selection can move.
    app.input.set_text("");
    app.input.insert('/');
    app.input.insert('c');
    app.input.insert('o');
    app.input.insert('n');
    assert!(app.palette.active(&app.input));
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(action, Some(Action::Submit { ref text, .. }) if text.trim() == "/checkpoint")
    );
}

#[test]
fn palette_tab_completes_and_escape_closes() {
    let mut app = App::default();
    for ch in "/un".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "/undo ");
    let mut app = App::default();
    for ch in "/co".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "/co", "escape keeps the text");
}

#[test]
fn ctrl_p_opens_the_palette_and_ctrl_j_inserts_a_newline() {
    let mut app = App::default();
    // Ctrl+P opens the command list even without a leading slash.
    app.on_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(app.palette.active(&app.input));
    assert!(app.input.is_empty());
    // Typing filters it, and Enter dispatches the completion.
    for ch in "q".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Some(Action::Quit)));
    assert!(!app.palette.active(&app.input));

    // Escape closes an explicit palette without touching the text.
    let mut app = App::default();
    app.on_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.palette.active(&app.input));
    assert!(
        app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE))
            .is_none()
    );
    assert!(!app.palette.active(&app.input), "escape is not undone");

    // Ctrl+J is a literal newline everywhere, unlike Alt+Enter which some
    // terminals reserve for themselves.
    let mut app = App::default();
    app.on_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert_eq!(app.input.text(), "\n");
    assert!(!app.palette.active(&app.input));
}

#[test]
fn palette_does_not_capture_when_text_has_a_space() {
    let mut app = App::default();
    for ch in "/mode work".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(!app.palette.active(&app.input));
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(action, Some(Action::Submit { ref text, .. }) if text == "/mode work"),
        "submitted, not completed"
    );
}

#[test]
fn up_down_traverse_history_when_palette_closed() {
    let mut app = App::default();
    app.input.seed_history(vec!["earlier".into()]);
    app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "earlier");
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "");
}

// ---- single display path for user prompts ----

#[test]
fn normal_prompt_is_not_echoed_by_the_tui() {
    let mut app = App::default();
    for ch in "inspect this".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Some(Action::Submit { ref text, .. }) if text == "inspect this"));
    assert!(
        app.items.is_empty(),
        "normal prompts render from the durable UserMessage event, not a local echo"
    );
    // The durable event, delivered through the shared formatter, is the
    // one authoritative display path: exactly one visible item.
    app.apply_item(DisplayItem::UserMessage {
        text: "inspect this".into(),
        media: vec![],
    });
    assert_eq!(app.items.len(), 1);
}

#[test]
fn two_identical_normal_prompts_stay_two_visible_items() {
    let mut app = App::default();
    for text in ["same prompt", "same prompt"] {
        for ch in text.chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let before = app.items.len();
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit { .. })));
        assert_eq!(
            app.items.len(),
            before,
            "no local echo for normal prompts; durable event is pending"
        );
        // One durable UserMessage event per submission.
        app.apply_item(DisplayItem::UserMessage {
            text: text.into(),
            media: vec![],
        });
    }
    assert_eq!(app.items.len(), 2);
    assert!(matches!(
        app.items[0],
        TranscriptItem::User { ref text } if text == "same prompt"
    ));
    assert!(matches!(
        app.items[1],
        TranscriptItem::User { ref text } if text == "same prompt"
    ));
}

#[test]
fn slash_command_echoes_exactly_once() {
    let mut app = App::default();
    // Enter dispatches a unique palette match.
    for ch in "/diff".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Some(Action::Submit { ref text, .. }) if text.trim() == "/diff"));
    assert_eq!(
        app.items.len(),
        0,
        "slash commands are controls, not transcript messages"
    );
    // Slash commands produce no durable UserMessage.
    app.apply_item(DisplayItem::KernelNotice {
        text: "mode: WORK".into(),
    });
    assert_eq!(app.items.len(), 1);
}

// ---- shared formatter integration ----

#[test]
fn replay_items_rebuild_transcript_without_hidden_data() {
    use latch_protocol::{Event, EventPayload};
    let mut app = App::default();
    let session = uuid::Uuid::new_v4();
    let event = |payload| Event {
        id: uuid::Uuid::new_v4(),
        session_id: session,
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload,
    };
    for item in latch_protocol::display_items(&event(EventPayload::UserMessage {
        text: "fix bug".into(),
        media: vec![],
    })) {
        app.apply_item(item);
    }
    for item in latch_protocol::display_items(&event(EventPayload::AssistantMessageCompleted {
        text: "done".into(),
        tool_calls: vec![],
        reasoning_content: Some("hidden".into()),

        reasoning: vec![],
    })) {
        app.apply_item(item);
    }
    assert_eq!(app.items.len(), 2);
    assert!(matches!(app.items[0], TranscriptItem::User { .. }));
    assert!(matches!(app.items[1], TranscriptItem::Assistant { .. }));
}

#[test]
fn truncate_never_panics_on_multibyte_text() {
    let text = "你好世界".repeat(20);
    let cut = truncate(&text, 5);
    assert!(cut.chars().count() <= 5);
    assert!(cut.ends_with('…') || cut.chars().count() < 5);
}

#[test]
fn exit_aliases_are_local_and_graceful() {
    for command in ["/quit", "/exit"] {
        let mut app = App::default();
        for ch in command.chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(matches!(
            app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Quit)
        ));
        assert!(app.presentation.cells().is_empty());
        assert!(
            app.items.is_empty(),
            "exit commands must never look like user messages"
        );
    }
}

#[test]
fn ctrl_c_cancels_when_busy_and_quits_when_idle() {
    let mut app = App::default();
    assert!(matches!(
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Action::Quit)
    ));
    app.busy = true;
    assert!(matches!(
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Action::Cancel)
    ));
}

#[test]
fn grapheme_cursor_uses_terminal_display_width() {
    let mut editor = editor_with("你e\u{301}");
    assert_eq!(editor.cursor_visual(&editor.layout(20)), (0, 3));
    editor.left();
    assert_eq!(
        editor.cursor(),
        (0, 1),
        "combining sequence moves as one grapheme"
    );
    assert_eq!(editor.cursor_visual(&editor.layout(20)), (0, 2));
    editor.backspace();
    assert_eq!(editor.text(), "e\u{301}");
}

#[test]
fn paste_multiline_into_empty_editor() {
    let mut editor = Composer::new();
    editor.insert_text("hello\nworld");
    assert_eq!(editor.text(), "hello\nworld");
    assert_eq!(editor.cursor(), (1, 5));
}

#[test]
fn multiline_paste_places_the_cursor_on_the_visible_composer_row() {
    let backend = ratatui::backend::TestBackend::new(20, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::default();
    app.on_paste("one\ntwo");

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    // The cursor is on the second pasted line inside the composer body:
    // top border + border/padding columns + cursor display column.
    let body = app.composer_body;
    assert_eq!(app.input.cursor(), (1, 3));
    assert_eq!(app.last_cursor, Some((body.x + 2 + 3, body.y + 1)));
    terminal
        .backend_mut()
        .assert_cursor_position((body.x + 2 + 3, body.y + 1));
}

#[test]
fn setup_paste_edits_the_active_field_and_masks_secrets() {
    let mut app = App::default();
    app.input.insert_text("composer stays here");
    for label in ["endpoint", "environment variable", "model id", "API key"] {
        let secret = label == "API key";
        app.capture = Some(CaptureState::new(CaptureSpec {
            label: label.into(),
            initial: String::new(),
            masked: secret,
        }));
        let pasted = if secret {
            "secret-sentinel"
        } else {
            "pasted-value"
        };
        app.on_paste(pasted);
        let capture = app.capture.as_ref().unwrap();
        assert_eq!(capture.value, pasted, "{label}");
        assert_eq!(app.input.text(), "composer stays here");
        assert!(
            app.items.is_empty(),
            "setup paste must not enter transcript"
        );
        if secret {
            assert!(!capture.display_value().contains(pasted));
            assert!(!format!("{capture:?}").contains(pasted));
            assert!(!render_to_text(&mut app, 80, 24).contains(pasted));
        }
    }
    app.capture = None;
    app.on_paste(" normal");
    assert_eq!(app.input.text(), "composer stays here normal");
}

#[test]
fn missing_provider_opens_guided_setup_immediately() {
    let mut app = App::default();
    app.output(Output::SetupCatalog(vec![SetupKind {
        kind: "openai-compatible".into(),
        label: "Compatible provider".into(),
        default_base_url: "https://example.test/v1".into(),
        credential_label: "env:API_KEY".into(),
        default_model: "model".into(),
        models: vec![],
    }]));
    app.output(Output::SetupRequired);
    assert!(app.setup.is_some());
    assert!(render_to_text(&mut app, 80, 24).contains("Setup"));
}

#[test]
fn composer_is_a_neutral_band_with_a_prompt_gutter() {
    let backend = ratatui::backend::TestBackend::new(48, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::default();

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    let buffer = terminal.backend().buffer();
    let text = buffer_text(buffer);
    let rows: Vec<&str> = text.lines().collect();
    let area = app.composer_body;
    // No decorative border: the composer reads as a surface band.
    for row in &rows[area.y as usize..(area.y + area.height) as usize] {
        assert!(
            !row.contains('╭') && !row.contains('│') && !row.contains('╰'),
            "{row:?}"
        );
    }
    // Every composer cell carries the neutral surface, and the prompt gutter
    // marks the input row.
    let surface = crate::theme::palette().surface().bg;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = buffer.cell((x, y)).expect("in bounds");
            assert_eq!(cell.style().bg, surface, "composer band is continuous");
        }
    }
    let body_row = rows[area.y as usize + 1];
    assert!(body_row.starts_with("› "), "{body_row:?}");
    // The empty composer leaves the cursor on the first text cell, directly
    // before the placeholder — never one column inside it.
    assert_eq!(app.last_cursor, Some((area.x + 2, area.y + 1)));
}

#[test]
fn paste_splits_text_at_the_cursor_and_keeps_suffix() {
    let mut editor = editor_with("helloworld");
    for _ in 0..5 {
        editor.left();
    }
    editor.insert_text(" brave\nnew ");
    assert_eq!(editor.text(), "hello brave\nnew world");
    assert_eq!(editor.cursor(), (1, 4));
    editor.insert('!');
    assert_eq!(editor.text(), "hello brave\nnew !world");
}

#[test]
fn paste_normalizes_crlf_and_bare_cr() {
    let mut editor = Composer::new();
    editor.insert_text("one\r\ntwo\rthree");
    assert_eq!(editor.text(), "one\ntwo\nthree");
    assert_eq!(editor.cursor(), (2, 5));
}

#[test]
fn paste_preserves_trailing_newline_and_blank_lines() {
    let mut editor = Composer::new();
    editor.insert_text("hello\n\n\n");
    assert_eq!(editor.text(), "hello\n\n\n");
    assert_eq!(editor.lines, vec!["hello", "", "", ""]);
    assert_eq!(editor.cursor(), (3, 0));
}

#[test]
fn paste_preserves_cjk_emoji_and_combining_graphemes() {
    let mut editor = Composer::new();
    editor.insert_text("你好 👨‍👩‍👧‍👦 e\u{301}");
    assert_eq!(editor.text(), "你好 👨‍👩‍👧‍👦 e\u{301}");
    editor.backspace();
    assert_eq!(
        editor.text(),
        "你好 👨‍👩‍👧‍👦 ",
        "combining sequence is one grapheme"
    );
    editor.backspace();
    editor.backspace();
    assert_eq!(
        editor.text(),
        "你好 ",
        "emoji family is removed as one grapheme"
    );
}

#[test]
fn paste_never_submits_and_slash_text_waits_for_enter() {
    let mut app = App::default();
    app.on_paste("/help");
    assert_eq!(app.input.text(), "/help");
    assert!(app.presentation.cells().is_empty());
    assert!(app.items.is_empty());
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Some(Action::Submit { ref text, .. }) if text == "/help"));
}

#[test]
fn multiline_slash_paste_is_one_prompt_and_one_history_entry() {
    let mut app = App::default();
    let prompt = "/not-a-command\nsecond paragraph\n\nlast";
    app.on_paste(prompt);
    assert!(!app.palette.active(&app.input));
    assert!(
        app.items.is_empty(),
        "paste must not create a transcript item"
    );
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(action, Some(Action::Submit { ref text, .. }) if text == prompt));
    assert_eq!(app.input.history, vec![prompt]);
    assert!(
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
}

// ---- V3.1 responsive sidebar and diff inspector ----

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()));
        }
        out.push('\n');
    }
    out
}

fn render_to_text(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    buffer_text(terminal.backend().buffer())
}

#[test]
fn responsive_sidebar_rules_are_clamped_and_not_a_fixed_third() {
    assert!(!sidebar_visible(80, None));
    assert!(!sidebar_visible(100, None));
    assert!(sidebar_visible(110, None));
    assert!(sidebar_visible(200, None));
    // Explicit override wins at any width.
    assert!(sidebar_visible(80, Some(true)));
    assert!(!sidebar_visible(200, Some(false)));
    assert_eq!(sidebar_width(200, true), 44, "wide screens clamp at 44");
    assert_eq!(sidebar_width(160, true), 44);
    assert_eq!(sidebar_width(159, true), 40);
    assert_eq!(sidebar_width(130, true), 35);
    assert_eq!(sidebar_width(129, true), 30);
    assert_eq!(sidebar_width(110, true), 26);
    assert_eq!(sidebar_width(80, true), 20);
    assert!(sidebar_width(200, true) < 200 / 3);
    assert_eq!(sidebar_width(200, false), 0);
}

#[test]
fn ctrl_b_and_slash_sidebar_toggle_agree() {
    let mut app = App {
        last_width: 200,
        ..App::default()
    };
    assert!(app.sidebar_visible_now());
    app.on_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(app.sidebar_override, Some(false));
    assert!(!app.sidebar_visible_now());
    app.on_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert!(app.sidebar_visible_now());
    // The slash command goes through the same toggle and never submits.
    for ch in "/sidebar".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert!(
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(app.sidebar_override, Some(false));
    assert!(app.presentation.cells().is_empty());
}

#[test]
fn draw_never_panics_across_responsive_sizes_and_cjk_goal() {
    let mut app = App {
        model: "a-very-long-model-name-that-should-truncate-gracefully".into(),
        ..App::default()
    };
    app.sidebar.apply_event(&latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload: latch_protocol::EventPayload::ContextMaterialized {
            stats: latch_protocol::ContextStats {
                instructions_tokens: 3_000,
                state_tokens: 3_300,
                recent_tokens: 37_900,
                recall_tokens: 1_500,
                tools_tokens: 2_400,
                total_tokens: 48_100,
                budget_tokens: 243_808,
                window_tokens: 256_000,
                reserve_tokens: 12_192,
                headroom_tokens: 195_708,
                durable_events: 503,
                episodes: 11,
                estimated: true,
                status: "bounded".into(),
                ..latch_protocol::ContextStats::default()
            },
        },
    });
    app.sidebar.apply_event(&latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 2,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload: latch_protocol::EventPayload::TaskStateUpdated {
            state: latch_protocol::TaskState {
                goal: "实现一个内存 TTL 缓存并验证边界条件 🚀".into(),
                ..latch_protocol::TaskState::default()
            },
        },
    });
    for width in [
        1, 10, 20, 30, 40, 60, 80, 100, 110, 120, 130, 159, 160, 200, 240,
    ] {
        for height in [1, 2, 4, 6, 10, 24, 60] {
            let _ = render_to_text(&mut app, width, height);
        }
    }

    // Action surfaces must never panic on tiny terminals either.
    app.output(Output::Event(Box::new(permission_event(
        uuid::Uuid::new_v4(),
        None,
    ))));
    for width in [10, 20, 48, 100, 200] {
        for height in [1, 2, 4, 6, 10, 24] {
            let _ = render_to_text(&mut app, width, height);
        }
    }
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    for ch in "/permissions".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    for width in [10, 20, 48, 100, 200] {
        for height in [1, 2, 4, 6, 10, 24] {
            let _ = render_to_text(&mut app, width, height);
        }
    }
}

#[test]
fn wide_terminal_shows_sidebar_and_narrow_hides_it() {
    let mut wide = App::default();
    wide.sidebar.apply_event(&latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload: latch_protocol::EventPayload::ContextMaterialized {
            stats: latch_protocol::ContextStats {
                total_tokens: 48_100,
                budget_tokens: 243_808,
                window_tokens: 256_000,
                status: "bounded".into(),
                ..latch_protocol::ContextStats::default()
            },
        },
    });
    let text = render_to_text(&mut wide, 200, 40);
    assert!(text.contains("CONTEXT"), "{text}");
    assert!(text.contains("Working set"));

    let mut narrow = App::default();
    let text = render_to_text(&mut narrow, 80, 40);
    assert!(!text.contains("Working set"));
    // Resizing across the threshold recomputes visibility without panics.
    let mut resizing = App {
        last_width: 200,
        ..App::default()
    };
    assert!(resizing.sidebar_visible_now());
    let _ = render_to_text(&mut resizing, 80, 24);
    assert!(!resizing.sidebar_visible_now());
    let _ = render_to_text(&mut resizing, 200, 24);
    assert!(resizing.sidebar_visible_now());
}

#[test]
fn diff_overlay_scrolls_toggles_raw_and_closes() {
    let mut app = App {
        last_width: 200,
        ..App::default()
    };
    let mut raw = String::from(
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,100 +1,100 @@\n",
    );
    for index in 0..100 {
        raw.push_str(&format!("-old line {index}\n+new line {index}\n"));
    }
    app.open_diff(raw);
    assert!(app.diff_overlay.is_some());
    let _ = render_to_text(&mut app, 120, 20);
    assert!(app.diff_max_scroll > 0 || app.diff_viewport_rows > 0);
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.diff_scroll, 1);
    app.on_key(key(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.diff_scroll, app.diff_max_scroll);
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(app.diff_raw);
    // While the inspector is open, typing does not reach the composer.
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(app.input.text().is_empty());
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.diff_overlay.is_none());
}

fn permission_event(
    request_id: uuid::Uuid,
    resolved: Option<(bool, &str)>,
) -> latch_protocol::Event {
    let payload = match resolved {
        None => latch_protocol::EventPayload::PermissionRequested {
            request_id,
            tool: "shell".into(),
            arguments: serde_json::json!({"command":"sudo make install"}),
            reason: "outside-workspace write requires explicit approval".into(),
            capabilities: vec!["external_filesystem_write".into()],
        },
        Some((approved, source)) => latch_protocol::EventPayload::PermissionResolved {
            request_id,
            approved,
            source: source.into(),
            risk: None,
        },
    };
    latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload,
    }
}

#[test]
fn steering_submit_keeps_running_while_ctrl_c_still_cancels() {
    let mut app = App {
        busy: true,
        ..App::default()
    };
    app.input.insert_text("change direction");
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(action, Some(Action::Submit { ref text, .. }) if text == "change direction"),
        "submitting while running is steering, not a no-op"
    );
    assert!(app.busy, "steering never cancels the active task");
    assert_eq!(app.input.text(), "", "the steering draft is taken");

    app.input.insert_text("half-written");
    let action = app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(action, Some(Action::Cancel)), "Ctrl+C cancels");
    assert!(app.interrupted);
    assert_eq!(
        app.input.text(),
        "half-written",
        "cancel never submits the composer contents"
    );
}

#[test]
fn safety_and_permissions_selectors_change_the_policy() {
    let mut app = App::default();
    for ch in "/safety".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    // Enter opens the selector instead of submitting a bare command.
    assert!(
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(app.selector.is_some());
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        action,
        Some(Action::SetSafety(Safety::Autonomous))
    ));
    assert!(app.selector.is_none());

    for ch in "/permissions".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.selector.is_some());
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.selector.is_none());

    // An explicit argument still travels to the CLI as a command.
    app.input.set_text("/safety strict");
    let action = app.submit_action();
    assert!(matches!(action, Some(Action::Submit { ref text, .. }) if text == "/safety strict"));

    // Chrome state arrives through the same Output path replay uses.
    app.output(Output::Safety(Safety::Strict));
    app.output(Output::Permissions(PermissionMode::AiReview));
    assert_eq!(app.safety, Safety::Strict);
    assert_eq!(app.permissions, PermissionMode::AiReview);
}

#[test]
fn approval_surface_owns_the_keyboard_and_emits_real_decisions() {
    let mut app = App::default();
    let request_id = uuid::Uuid::new_v4();
    app.output(Output::Event(Box::new(permission_event(request_id, None))));
    assert!(app.permission.is_some());
    let text = render_to_text(&mut app, 100, 30);
    assert!(text.contains("Approval needed"), "{text}");
    assert!(text.contains("Approve"), "{text}");
    assert!(text.contains("Deny"), "{text}");
    assert!(
        text.contains("sudo make install"),
        "readable argument preview: {text}"
    );
    assert!(
        text.contains("capability: external_filesystem_write"),
        "{text}"
    );

    // The composer stays on screen above the bottom action surface.
    assert!(text.contains("Ask Latch…"), "{text}");

    // Ctrl+O opens the full request without resolving; Esc returns to the
    // pending surface rather than denying.
    app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let expanded = app.permission.as_ref().expect("still pending");
    assert!(expanded.expanded);
    let full = render_to_text(&mut app, 100, 30);
    assert!(full.contains("full request"), "{full}");
    assert!(full.contains("sudo make install"), "{full}");
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.permission
            .as_ref()
            .is_some_and(|prompt| !prompt.expanded),
        "Esc collapses the inspector before denying"
    );

    // Down selects Deny; Enter resolves the highlighted option.
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    let denied = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        denied,
        Some(Action::Permission {
            approved: false,
            request_id: id
        }) if id == request_id
    ));
    assert!(app.permission.is_none());

    // `y` is a direct approve shortcut.
    app.output(Output::Event(Box::new(permission_event(request_id, None))));
    let approved = app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(matches!(
        approved,
        Some(Action::Permission {
            approved: true,
            request_id: id
        }) if id == request_id
    ));

    // Esc denies without an extra confirmation.
    app.output(Output::Event(Box::new(permission_event(request_id, None))));
    let denied = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(
        denied,
        Some(Action::Permission {
            approved: false,
            request_id: id
        }) if id == request_id
    ));
    // A resolution event clears a prompt that arrived out of band (for
    // example a cancelled turn).
    app.output(Output::Event(Box::new(permission_event(request_id, None))));
    app.output(Output::Event(Box::new(permission_event(
        request_id,
        Some((false, "cancelled")),
    ))));
    assert!(app.permission.is_none());
    assert!(
        app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_none()
            || app.input.text().is_empty()
    );
}

#[test]
fn approval_surface_keeps_the_full_request_available() {
    let mut app = App::default();
    let request_id = uuid::Uuid::new_v4();
    let long = "x".repeat(600);
    app.output(Output::Event(Box::new(latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload: latch_protocol::EventPayload::PermissionRequested {
            request_id,
            tool: "shell".into(),
            arguments: serde_json::json!({"command": format!("echo {long}")}),
            reason: "needs network access".into(),
            capabilities: vec!["network".into()],
        },
    })));
    let prompt = app.permission.as_ref().expect("pending");
    assert_eq!(
        prompt.arguments.len(),
        serde_json::to_string_pretty(&serde_json::json!({"command": format!("echo {long}")}))
            .unwrap()
            .len(),
        "the full raw request is retained, not truncated"
    );
    let preview = render_to_text(&mut app, 80, 30);
    assert!(preview.contains("full request: Ctrl+O"), "{preview}");
    app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let full = render_to_text(&mut app, 80, 30);
    assert!(
        full.contains(&long[..40]),
        "Ctrl+O shows the request body, not only the preview"
    );
}

#[test]
fn permission_modes_render_distinct_labels_and_disable_during_a_turn() {
    let mut app = App::default();
    for ch in "/permissions".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    let text = render_to_text(&mut app, 100, 30);
    assert!(text.contains("Ask for approval"), "{text}");
    assert!(text.contains("Approve for me"), "{text}");
    assert!(text.contains("Auto approve"), "{text}");
    assert!(
        text.contains("(current)"),
        "the active mode is marked: {text}"
    );
    assert!(
        text.contains("Latch asks before operations"),
        "descriptions distinguish the modes: {text}"
    );

    // During a live turn the CLI refuses policy changes, so the surface says
    // so instead of hiding the choices.
    app.busy = true;
    app.selector = Some(PolicySelector {
        kind: SelectorKind::Permissions,
        selected: SelectorKind::Permissions.current(&app),
    });
    let busy = render_to_text(&mut app, 100, 30);
    assert!(busy.contains("unavailable: active turn"), "{busy}");
    assert!(busy.contains("finish or cancel"), "{busy}");
    assert!(
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
            .is_none(),
        "a disabled choice cannot be applied"
    );
}

#[test]
fn patch_delta_colors_are_independent() {
    let additions_only = patch_lines(&[PatchFile {
        call_id: "a".into(),
        path: "src/new.rs".into(),
        kind: 'A',
        additions: 18,
        deletions: 0,
        status: CellStatus::Passed,
        diagnostic: String::new(),
        raw: String::new(),
        preview: String::new(),
    }]);
    let spans = &additions_only[0].spans;
    let added = spans
        .iter()
        .find(|span| span.content == "+18")
        .expect("addition span");
    assert_eq!(added.style.fg, Some(Color::Green));
    let removed = spans
        .iter()
        .find(|span| span.content == "−0")
        .expect("deletion span");
    assert_ne!(
        removed.style.fg,
        Some(Color::Red),
        "zero is not colored red"
    );

    let deletions_only = patch_lines(&[PatchFile {
        call_id: "d".into(),
        path: "src/old.rs".into(),
        kind: 'D',
        additions: 0,
        deletions: 7,
        status: CellStatus::Passed,
        diagnostic: String::new(),
        raw: String::new(),
        preview: String::new(),
    }]);
    let spans = &deletions_only[0].spans;
    let added = spans
        .iter()
        .find(|span| span.content == "+0")
        .expect("addition span");
    assert_ne!(added.style.fg, Some(Color::Green));
    let removed = spans
        .iter()
        .find(|span| span.content == "−7")
        .expect("deletion span");
    assert_eq!(removed.style.fg, Some(Color::Red));
}

fn find_span_style(lines: &[Line<'static>], needle: &str) -> Style {
    for line in lines {
        for span in &line.spans {
            if span.content.contains(needle) {
                return span.style;
            }
        }
    }
    panic!("no rendered span contains {needle:?}");
}

fn lines_text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn inline_preview_colors_real_removed_and_added_source_lines() {
    let preview = "diff --git a/src/calc.rs b/src/calc.rs
--- a/src/calc.rs
+++ b/src/calc.rs
@@ -1,4 +1,4 @@
 pub fn add(a: i32, b: i32) -> i32 {
     let base = 10;
-    base + a + b
+    base + a - b
 }
";
    let lines = patch_lines(&[PatchFile {
        call_id: "p1".into(),
        path: "src/calc.rs".into(),
        kind: 'M',
        additions: 1,
        deletions: 1,
        status: CellStatus::Passed,
        diagnostic: String::new(),
        raw: String::new(),
        preview: preview.into(),
    }]);
    // The actual source lines carry the colors, not the +N/−N summary.
    assert_eq!(
        find_span_style(&lines, "base + a + b").fg,
        Some(Color::Red),
        "deleted source line must be red"
    );
    assert_eq!(
        find_span_style(&lines, "base + a - b").fg,
        Some(Color::Green),
        "added source line must be green"
    );
    assert!(
        find_span_style(&lines, "pub fn add")
            .add_modifier
            .contains(Modifier::DIM),
        "unchanged context is subdued"
    );
    let text = lines_text(&lines);
    assert!(text.contains("-    base + a + b"), "{text}");
    assert!(text.contains("+    base + a - b"), "{text}");
    assert!(text.contains("Edited src/calc.rs  +1 −1"), "{text}");
}

#[test]
fn inline_preview_of_a_new_file_is_all_additions() {
    let preview = "diff --git a/tests/calc.rs b/tests/calc.rs
--- /dev/null
+++ b/tests/calc.rs
@@ -0,0 +1,2 @@
+#[test]
+fn subtracts() {}
";
    let lines = patch_lines(&[PatchFile {
        call_id: "p2".into(),
        path: "tests/calc.rs".into(),
        kind: 'A',
        additions: 2,
        deletions: 0,
        status: CellStatus::Passed,
        diagnostic: String::new(),
        raw: String::new(),
        preview: preview.into(),
    }]);
    assert_eq!(find_span_style(&lines, "#[test]").fg, Some(Color::Green));
    assert!(
        !lines_text(&lines).contains("\n-"),
        "{}",
        lines_text(&lines)
    );
}

#[test]
fn inline_preview_handles_deleted_files_and_unicode_content() {
    let preview = "diff --git a/旧.rs b/旧.rs
--- a/旧.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-旧值 = 计算();
-保留
";
    let lines = patch_lines(&[PatchFile {
        call_id: "p4".into(),
        path: "旧.rs".into(),
        kind: 'D',
        additions: 0,
        deletions: 2,
        status: CellStatus::Passed,
        diagnostic: String::new(),
        raw: String::new(),
        preview: preview.into(),
    }]);
    assert_eq!(
        find_span_style(&lines, "旧值 = 计算();").fg,
        Some(Color::Red)
    );
    let text = lines_text(&lines);
    assert!(text.contains("Edited 旧.rs  +0 −2"), "{text}");
    assert!(!text.contains("\n+"), "{text}");
}

#[test]
fn inline_preview_is_bounded_and_points_at_the_full_diff() {
    let mut preview = String::from(
        "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -1,60 +1,60 @@\n",
    );
    for index in 0..60 {
        preview.push_str(&format!("-old line {index}\n+new line {index}\n"));
    }
    let lines = patch_lines(&[PatchFile {
        call_id: "p3".into(),
        path: "src/big.rs".into(),
        kind: 'M',
        additions: 60,
        deletions: 60,
        status: CellStatus::Passed,
        diagnostic: String::new(),
        raw: String::new(),
        preview,
    }]);
    // Header + blank spacer + at most 14 body lines + the omission note.
    assert!(lines.len() <= 17, "preview is bounded: {}", lines.len());
    let text = lines_text(&lines);
    assert!(text.contains("+new line 0"), "{text}");
    assert!(!text.contains("+new line 14"), "{text}");
    assert!(
        text.contains("diff lines omitted · /diff for the full diff"),
        "{text}"
    );
}

#[test]
fn header_pricing_reaches_the_sidebar() {
    let mut app = App::default();
    app.output(Output::Header {
        model: "deepseek-flash".into(),
        provider: "OpenCode Go".into(),
        provider_id: "opencode-go".into(),
        effort: latch_protocol::ReasoningEffort::Low,
        workspace: "/tmp/latch".into(),
        branch: "main".into(),
        resumed: false,
        pricing: Some(crate::sidebar::Pricing {
            input_per_million: Some(0.28),
            output_per_million: Some(0.42),
            cache_read_per_million: None,
            cache_write_per_million: None,
            currency: "USD".into(),
        }),
    });
    app.output(Output::Event(Box::new(latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload: latch_protocol::EventPayload::ModelUsage {
            usage: latch_protocol::Usage {
                input_tokens: 1_000_000,
                output_tokens: 0,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_miss_tokens: None,
                reasoning_tokens: None,
            },
        },
    })));
    let cost = app.sidebar.estimated_cost().expect("configured pricing");
    assert!((cost.amount - 0.28).abs() < 1e-9);
}

// ---- V4 composer and layout redesign ----

#[test]
fn user_messages_render_on_a_neutral_band_with_a_gutter() {
    let palette = crate::theme::palette();
    let lines = cell_lines(
        &Cell::User {
            text: "fix the parser".into(),
            media: Vec::new(),
        },
        false,
        40,
        true,
    );
    // Top pad, one content row, bottom pad.
    assert_eq!(lines.len(), 3);
    let band = palette.user_message().bg;
    assert!(band.is_some(), "user band is painted on rich terminals");
    for line in &lines {
        assert_eq!(line.style.bg, band, "every user row keeps the band");
    }
    let first: String = lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(first.starts_with("› "), "{first:?}");
    assert!(first.contains("fix the parser"));
    // The band reaches the full transcript width.
    let width: usize = lines[1]
        .spans
        .iter()
        .map(|span| display_width(&span.content))
        .sum();
    assert_eq!(width, 40, "band padding covers the row");
}

#[test]
fn wrapped_user_messages_keep_the_band_on_every_visual_row() {
    let text = "word ".repeat(30);
    let lines = cell_lines(
        &Cell::User {
            text,
            media: Vec::new(),
        },
        false,
        24,
        true,
    );
    let band = crate::theme::palette().user_message().bg;
    assert!(lines.len() > 4, "long text wraps into several rows");
    for line in &lines {
        assert_eq!(line.style.bg, band);
        let width: usize = line
            .spans
            .iter()
            .map(|span| display_width(&span.content))
            .sum();
        assert!(width <= 24, "row fits the viewport: {width}");
        if line.spans.len() > 1 {
            assert_eq!(width, 24, "content rows are padded to the full width");
        }
    }
    // Only the first content row carries the `›` gutter.
    let text_rows = &lines[1..lines.len() - 1];
    assert!(text_rows[0].spans[0].content == "› ");
    assert!(
        text_rows[1..]
            .iter()
            .all(|line| line.spans[0].content == "  ")
    );
}

#[test]
fn plain_export_keeps_user_and_assistant_text_clean() {
    let cells = vec![
        Cell::User {
            text: "hello".into(),
            media: Vec::new(),
        },
        Cell::Assistant {
            text: "world".into(),
        },
    ];
    let plain = render_cells_plain(&cells, false);
    assert!(plain.contains("› hello"), "{plain}");
    assert!(plain.contains("• world"), "{plain}");
    assert!(
        plain.lines().all(|line| !line.ends_with(' ')),
        "the copy-friendly export never pads the band: {plain:?}"
    );
}

#[test]
fn assistant_messages_stay_on_the_terminal_background() {
    let lines = cell_lines(
        &Cell::Assistant {
            text: "hello **world**".into(),
        },
        false,
        40,
        true,
    );
    assert!(lines[0].spans[0].content == "• ");
    assert!(lines[0].spans[0].style.bg.is_none());
    for span in lines.iter().flat_map(|line| &line.spans) {
        assert!(
            span.style.bg.is_none(),
            "assistant markdown never paints a surface: {span:?}"
        );
    }
}

fn assert_snapshot(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var("LATCH_UPDATE_SNAPSHOTS").is_ok() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing snapshot {name}"));
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "snapshot {name} changed"
    );
}

fn app_with_header(model: &str, workspace: &str) -> App {
    let mut app = App::default();
    app.output(Output::Header {
        model: model.into(),
        provider: "OpenCode Go".into(),
        provider_id: "opencode-go".into(),
        effort: latch_protocol::ReasoningEffort::Low,
        workspace: workspace.into(),
        branch: "main".into(),
        resumed: false,
        pricing: None,
    });
    app
}

fn presentation_event(payload: latch_protocol::EventPayload) -> latch_protocol::Event {
    latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload,
    }
}

/// A deterministic transcript for layout snapshots, fed through the same
/// durable-event path the live TUI uses.
fn transcript_fixture(app: &mut App) {
    for payload in [
        latch_protocol::EventPayload::UserMessage {
            text: "Fix the failing test.".into(),
            media: vec![],
        },
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "t1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cargo test"}),
            },
        },
        latch_protocol::EventPayload::ToolFailed {
            result: ToolResult {
                call_id: "t1".into(),
                name: "shell".into(),
                output: "exit code 1\nassertion failed".into(),
                is_error: true,
                artifact_id: None,
                media: Vec::new(),
            },
        },
        latch_protocol::EventPayload::AssistantMessageCompleted {
            text: "The addend is wrong; fixing it now.".into(),
            tool_calls: vec![],
            reasoning_content: None,

            reasoning: vec![],
        },
    ] {
        app.output(Output::Event(Box::new(presentation_event(payload))));
    }
}

/// A guarded edit and a newly created file, fed through the same
/// ToolRequested/FileChanged/ToolResult path the live TUI uses.
fn patch_preview_fixture(app: &mut App) {
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::UserMessage {
            text: "Fix the calculation.".into(),
            media: vec![],
        },
    ))));
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "p1".into(),
                name: "patch".into(),
                arguments: serde_json::json!({
                    "path": "src/calc.rs",
                    "old": "base + a + b",
                    "new": "base + a - b",
                }),
            },
        },
    ))));
    app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::FileChanged {
                created: false,
                before: None,
                after: latch_protocol::FileVersion {
                    path: "src/calc.rs".into(),
                    content_hash: "h1".into(),
                    size: 1,
                },
                owner: latch_protocol::ChangeOwner::Latch,
                undo_artifact: None,
                additions: 1,
                deletions: 1,
                preview: "diff --git a/src/calc.rs b/src/calc.rs\n--- a/src/calc.rs\n+++ b/src/calc.rs\n@@ -1,4 +1,4 @@\n pub fn add(a: i32, b: i32) -> i32 {\n     let base = 10;\n-    base + a + b\n+    base + a - b\n }\n".into(),
                call_id: Some("p1".into()),
            },
        ))));
    app.output(Output::ToolResult(ToolResult {
        call_id: "p1".into(),
        name: "patch".into(),
        output: "updated src/calc.rs @ h1".into(),
        is_error: false,
        artifact_id: None,
        media: Vec::new(),
    }));
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "p2".into(),
                name: "write".into(),
                arguments: serde_json::json!({
                    "path": "tests/calc.rs",
                    "content": "#[test]\nfn subtracts() {}",
                }),
            },
        },
    ))));
    app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::FileChanged {
                created: false,
                before: None,
                after: latch_protocol::FileVersion {
                    path: "tests/calc.rs".into(),
                    content_hash: "h2".into(),
                    size: 1,
                },
                owner: latch_protocol::ChangeOwner::Latch,
                undo_artifact: None,
                additions: 2,
                deletions: 0,
                preview: "diff --git a/tests/calc.rs b/tests/calc.rs\n--- /dev/null\n+++ b/tests/calc.rs\n@@ -0,0 +1,2 @@\n+#[test]\n+fn subtracts() {}\n".into(),
                call_id: Some("p2".into()),
            },
        ))));
    app.output(Output::ToolResult(ToolResult {
        call_id: "p2".into(),
        name: "write".into(),
        output: "updated tests/calc.rs @ h2".into(),
        is_error: false,
        artifact_id: None,
        media: Vec::new(),
    }));
}

#[test]
fn snapshot_inline_edit_preview() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    patch_preview_fixture(&mut app);
    assert_snapshot(
        "v4_inline_edit_preview.txt",
        &render_to_text(&mut app, 100, 24),
    );
}

#[test]
fn welcome_wordmark_uses_five_distinct_muted_letter_colors() {
    let lines = wordmark_lines();
    assert_eq!(lines.len(), 5);
    let mut colors = Vec::new();
    for line in &lines {
        let letters: Vec<&Span<'static>> = line
            .spans
            .iter()
            .filter(|span| span.content.contains('█'))
            .collect();
        assert_eq!(letters.len(), 5, "each row shows five letter regions");
        for span in letters {
            colors.push(span.style.fg.expect("letter color"));
        }
    }
    let distinct: std::collections::BTreeSet<String> =
        colors.iter().map(|color| format!("{color:?}")).collect();
    assert_eq!(distinct.len(), 5, "all five letters use different colors");
    for color in &colors {
        let Color::Rgb(red, green, blue) = color else {
            panic!("expected rgb wordmark color, got {color:?}");
        };
        let max = *red.max(green).max(blue) as i32;
        let min = *red.min(green).min(blue) as i32;
        assert!(max - min <= 90, "muted tone expected: {color:?}");
    }
}

#[test]
fn idle_composer_body_is_roomier_but_stays_bounded() {
    assert_eq!(ComposerChrome::responsive(30, 1).body, 3);
    assert_eq!(ComposerChrome::responsive(24, 1).body, 3);
    assert_eq!(ComposerChrome::responsive(30, 20).body, 8);
    assert_eq!(ComposerChrome::responsive(12, 1).body, 1);
}

fn markdown_table_fixture(app: &mut App, text: &str) {
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::AssistantMessageCompleted {
            text: text.into(),
            tool_calls: vec![],
            reasoning_content: None,

            reasoning: vec![],
        },
    ))));
}

#[test]
fn snapshot_markdown_table_normal() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    markdown_table_fixture(
        &mut app,
        "Here is the plan:\n\n\
             | Step | Owner | Status |\n\
             |:-----|:------|-------:|\n\
             | Inspect the failing tests | Latch | done |\n\
             | Patch the parser | Latch | active |\n\
             | Verify with cargo test | Kernel | pending |",
    );
    assert_snapshot(
        "v4_markdown_table_normal.txt",
        &render_to_text(&mut app, 100, 24),
    );
}

#[test]
fn snapshot_markdown_table_narrow() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    markdown_table_fixture(
        &mut app,
        "| Module | Responsibility | Notes |\n\
             |:-------|:---------------|:------|\n\
             | composer | scrollable multiline editor viewport | keeps the cursor visible while wrapping |\n\
             | sidebar | responsive session state | hidden below 110 columns |",
    );
    assert_snapshot(
        "v4_markdown_table_narrow.txt",
        &render_to_text(&mut app, 56, 24),
    );
}

#[test]
fn snapshot_markdown_table_wide() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    markdown_table_fixture(
        &mut app,
        "| Mode | Mutation | Validation | Notes |\n\
             |:-----|:---------|:-----------|:------|\n\
             | ASK | denied | not run | read-only inspection |\n\
             | PLAN | denied | not run | produces a plan |\n\
             | WORK | policy-approved | required | implements and verifies |",
    );
    assert_snapshot(
        "v4_markdown_table_wide.txt",
        &render_to_text(&mut app, 160, 24),
    );
}

#[test]
fn snapshot_markdown_table_cjk() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    markdown_table_fixture(
        &mut app,
        "| 模块 | 职责 | 状态 |\n\
             |:-----|:-----|-----:|\n\
             | 编辑器 | 可滚动的多行输入视口 | 完成 |\n\
             | 侧边栏 | 响应式会话状态 | 进行中 |",
    );
    assert_snapshot(
        "v4_markdown_table_cjk.txt",
        &render_to_text(&mut app, 100, 24),
    );
}

#[test]
fn snapshot_welcome_state() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    assert_snapshot("v4_welcome.txt", &render_to_text(&mut app, 100, 24));
}

#[test]
fn snapshot_single_line_composer() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input.insert_text("fix the failing test");
    assert_snapshot("v4_composer_single.txt", &render_to_text(&mut app, 100, 24));
}

#[test]
fn snapshot_user_and_assistant_message_hierarchy() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    for payload in [
        latch_protocol::EventPayload::UserMessage {
            text: "Fix the failing test without changing the public API.".into(),
        media: vec![],
        },
        latch_protocol::EventPayload::AssistantMessageCompleted {
            text: "I'll inspect the failing case and patch it.\n\n- read the parser\n- keep the API stable".into(),
            tool_calls: vec![],
            reasoning_content: None,

            reasoning: vec![],},
    ] {
        app.output(Output::Event(Box::new(presentation_event(payload))));
    }
    assert_snapshot(
        "v5_message_hierarchy.txt",
        &render_to_text(&mut app, 100, 24),
    );
}

#[test]
fn snapshot_narrow_user_message_keeps_the_band() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::UserMessage {
            text: "Please wrap this long user message across several narrow visual rows and keep the surface intact."
                .into(),
        media: vec![],
        },
    ))));
    assert_snapshot("v5_message_narrow.txt", &render_to_text(&mut app, 48, 16));
}

#[test]
fn snapshot_approval_surface_above_composer() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    let request_id = uuid::Uuid::new_v4();
    app.output(Output::Event(Box::new(permission_event(request_id, None))));
    assert_snapshot(
        "v5_approval_surface.txt",
        &render_to_text(&mut app, 100, 30),
    );
}

#[test]
fn snapshot_approval_request_inspector() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    let request_id = uuid::Uuid::new_v4();
    app.output(Output::Event(Box::new(permission_event(request_id, None))));
    app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_snapshot(
        "v5_approval_inspector.txt",
        &render_to_text(&mut app, 100, 30),
    );
}

#[test]
fn snapshot_permission_mode_selector() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    for ch in "/permissions".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_snapshot(
        "v5_permission_modes.txt",
        &render_to_text(&mut app, 100, 30),
    );
}

#[test]
fn snapshot_active_running_status() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.busy = true;
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "t1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cargo test --workspace"}),
            },
        },
    ))));
    assert_snapshot("v5_active_status.txt", &render_to_text(&mut app, 100, 20));
}

#[test]
fn snapshot_subagent_status_and_sidebar() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "s1".into(),
                name: "spawn_agent".into(),
                arguments: serde_json::json!({"task_name":"audit-locks","message":"Review the lock ordering in the cache."}),
            },
        },
    ))));
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolCompleted {
            result: latch_protocol::ToolResult {
                call_id: "s1".into(),
                name: "spawn_agent".into(),
                output: "{\"agent_id\":\"00000000-0000-0000-0000-000000000001\",\"task_name\":\"audit-locks\",\"agent_type\":\"explorer\",\"status\":\"running\"}"
                    .into(),
                is_error: false,
                artifact_id: None,
            media: Vec::new(),
            },
        },
    ))));
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::AgentNotificationDelivered {
            report: latch_protocol::AgentReport {
                report_id: uuid::Uuid::new_v4(),
                agent_id: uuid::Uuid::from_u128(1),
                task_name: "audit-locks".into(),
                status: latch_protocol::AgentStatus::Completed,
                completion: latch_protocol::CompletionState::InProgress,
                summary: "Found two unsynchronized locks.\nDetails omitted".into(),
                findings: vec![],
                touched_files: vec!["src/locks.rs".into()],
                evidence: vec![],
                unresolved_questions: vec![],
            },
        },
    ))));
    assert_snapshot("v5_subagent_status.txt", &render_to_text(&mut app, 200, 30));
}

#[test]
fn snapshot_workspace_diff_cell() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "d1".into(),
                name: "git_diff".into(),
                arguments: serde_json::json!({}),
            },
        },
    ))));
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolCompleted {
            result: latch_protocol::ToolResult {
                call_id: "d1".into(),
                name: "git_diff".into(),
                output: "diff --git a/src/calc.rs b/src/calc.rs\nindex 1111111..2222222 100644\n--- a/src/calc.rs\n+++ b/src/calc.rs\n@@ -1,5 +1,5 @@\n pub fn add(a: i32, b: i32) -> i32 {\n     let base = 10;\n-    base + a + b\n+    base + a - b\n }\n"
                    .into(),
                is_error: false,
                artifact_id: None,
                media: Vec::new(),
            },
        },
    ))));
    assert_snapshot("v5_workspace_diff.txt", &render_to_text(&mut app, 100, 24));
}

#[test]
fn snapshot_multiline_composer() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input
        .insert_text("line one\nline two\nline three\nline four\nline five\nline six\nline seven");
    assert_snapshot(
        "v4_composer_multiline.txt",
        &render_to_text(&mut app, 100, 24),
    );
}

#[test]
fn snapshot_large_paste_scrolled_to_top_middle_and_bottom() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    let text = (0..80)
        .map(|index| format!("pasted line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.input.insert_text(&text);
    let _ = render_to_text(&mut app, 100, 30);
    // Bottom (cursor pinned).
    assert_snapshot("v4_paste_bottom.txt", &render_to_text(&mut app, 100, 30));
    // Middle: scroll the viewport without touching the buffer.
    app.input
        .scroll_lines(-30, app.last_input_width, app.last_input_height);
    assert_snapshot("v4_paste_middle.txt", &render_to_text(&mut app, 100, 30));
    // Top.
    app.input
        .scroll_lines(-1000, app.last_input_width, app.last_input_height);
    assert_snapshot("v4_paste_top.txt", &render_to_text(&mut app, 100, 30));
    assert_eq!(app.input.text(), text, "scrolling never edits the buffer");
}

#[test]
fn snapshot_narrow_terminal() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input.insert_text("a narrow but usable composer");
    assert_snapshot("v4_narrow.txt", &render_to_text(&mut app, 48, 16));
}

#[test]
fn snapshot_wide_terminal_with_sidebar() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.sidebar.apply_event(&latch_protocol::Event {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        parent_id: None,
        payload: latch_protocol::EventPayload::ContextMaterialized {
            stats: latch_protocol::ContextStats {
                instructions_tokens: 3_000,
                state_tokens: 3_300,
                recent_tokens: 37_900,
                recall_tokens: 1_500,
                tools_tokens: 2_400,
                total_tokens: 48_100,
                budget_tokens: 243_808,
                window_tokens: 256_000,
                reserve_tokens: 12_192,
                headroom_tokens: 195_708,
                durable_events: 503,
                episodes: 11,
                estimated: true,
                status: "bounded".into(),
                ..latch_protocol::ContextStats::default()
            },
        },
    });
    transcript_fixture(&mut app);
    assert_snapshot("v4_wide_sidebar.txt", &render_to_text(&mut app, 200, 40));
}

#[test]
fn snapshot_working_and_interrupted_states() {
    let mut working = app_with_header("deepseek-flash", "/tmp/latch-ui");
    working.busy = true;
    assert_snapshot("v4_working.txt", &render_to_text(&mut working, 100, 20));
    let mut interrupted = app_with_header("deepseek-flash", "/tmp/latch-ui");
    interrupted.interrupted = true;
    assert_snapshot(
        "v4_interrupted.txt",
        &render_to_text(&mut interrupted, 100, 20),
    );
}

#[test]
fn snapshot_long_model_and_workspace_metadata() {
    let mut app = app_with_header(
        "a-very-long-model-name-that-keeps-going-and-going-flash",
        "/home/someone/very/deeply/nested/workspace/path/that/is/long",
    );
    app.input.insert_text("check the long metadata handling");
    assert_snapshot("v4_long_metadata.txt", &render_to_text(&mut app, 160, 20));
}

#[test]
fn snapshot_cjk_prompt() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input.insert_text("请修复失败的测试并运行验证");
    assert_snapshot("v4_cjk_prompt.txt", &render_to_text(&mut app, 100, 20));
}

#[test]
fn composer_home_end_and_ctrl_variants() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input.insert_text("one\ntwo\nthree");
    app.on_key(key(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.input.cursor(), (2, 0), "Home is line start");
    app.on_key(key(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.input.cursor(), (2, 5), "End is line end");
    app.on_key(key(KeyCode::Home, KeyModifiers::CONTROL));
    assert_eq!(app.input.cursor(), (0, 0), "Ctrl+Home is buffer start");
    app.on_key(key(KeyCode::End, KeyModifiers::CONTROL));
    assert_eq!(app.input.cursor(), (2, 5), "Ctrl+End is buffer end");
}

#[test]
fn page_keys_scroll_the_composer_only_when_it_overflows() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input.insert_text(
        &(0..60)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let _ = render_to_text(&mut app, 80, 24);
    let before = app.input.viewport();
    app.on_key(key(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(
        app.input.viewport() < before,
        "overflowing composer pages upward"
    );
    // A short composer leaves PageUp with its transcript role.
    let mut short = app_with_header("deepseek-flash", "/tmp/latch-ui");
    short.sync_viewport(100, 10);
    short.scroll_up(30);
    let before = short.scroll;
    short.on_key(key(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(short.scroll, before - 10, "transcript still pages");
}

#[test]
fn mouse_wheel_routing_targets_the_overflowing_composer() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.input.insert_text(
        &(0..40)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let _ = render_to_text(&mut app, 80, 24);
    assert!(app.composer_body.height > 0);
    let before = app.input.viewport();
    app.composer_scroll_up(WHEEL_ROWS);
    assert!(app.input.viewport() < before);
    app.composer_scroll_down(1);
    assert!(app.input.viewport() <= before);
}

#[test]
fn resize_while_editing_preserves_the_buffer_and_cursor() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    let text = (0..40)
        .map(|index| format!("resize line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.input.insert_text(&text);
    let _ = render_to_text(&mut app, 120, 30);
    let wide_height = app.composer_body.height;
    let _ = render_to_text(&mut app, 56, 16);
    let narrow_height = app.composer_body.height;
    assert!(narrow_height <= wide_height);
    assert_eq!(app.input.text(), text, "resize never edits the buffer");
    assert!(
        app.last_cursor.is_some(),
        "cursor stays visible after resize"
    );
    let _ = render_to_text(&mut app, 180, 44);
    assert_eq!(app.input.text(), text);
    assert!(app.last_cursor.is_some());
}

#[test]
fn working_and_interrupted_states_are_visible_above_the_composer() {
    let mut working = app_with_header("deepseek-flash", "/tmp/latch-ui");
    working.busy = true;
    let text = render_to_text(&mut working, 100, 20);
    assert!(text.contains("• Working"), "{text}");
    let mut interrupted = app_with_header("deepseek-flash", "/tmp/latch-ui");
    interrupted.interrupted = true;
    let text = render_to_text(&mut interrupted, 100, 20);
    assert!(text.contains("Interrupted"), "{text}");
}

#[test]
fn active_status_row_reports_semantic_running_state() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.busy = true;
    let payload = latch_protocol::EventPayload::ToolRequested {
        call: latch_protocol::ToolCall {
            id: "t1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "cargo test --workspace"}),
        },
    };
    app.output(Output::Event(Box::new(presentation_event(payload))));
    let text = render_to_text(&mut app, 100, 20);
    assert!(text.contains("Running tests"), "{text}");
    assert!(text.contains("cargo test --workspace"), "{text}");

    // An approval request takes precedence and names the actual wait state.
    app.output(Output::Event(Box::new(permission_event(
        uuid::Uuid::new_v4(),
        None,
    ))));
    let text = render_to_text(&mut app, 100, 20);
    assert!(text.contains("Waiting for approval"), "{text}");
}

#[test]
fn child_agent_activity_reaches_the_status_row_and_sidebar() {
    let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolRequested {
            call: latch_protocol::ToolCall {
                id: "s1".into(),
                name: "spawn_agent".into(),
                arguments: serde_json::json!({"task_name":"audit-locks","message":"Review locking"}),
            },
        },
    ))));
    let text = render_to_text(&mut app, 100, 20);
    assert!(text.contains("Spawned `audit-locks`"), "{text}");
    assert!(text.contains("child `audit-locks` starting"), "{text}");

    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::ToolCompleted {
            result: latch_protocol::ToolResult {
                call_id: "s1".into(),
                name: "spawn_agent".into(),
                output: "{\"agent_id\":\"00000000-0000-0000-0000-000000000001\",\"task_name\":\"audit-locks\",\"agent_type\":\"explorer\",\"status\":\"running\"}"
                    .into(),
                is_error: false,
                artifact_id: None,
            media: Vec::new(),
            },
        },
    ))));
    let text = render_to_text(&mut app, 200, 30);
    assert!(text.contains("child `audit-locks` running"), "{text}");
    assert!(text.contains("CHILDREN"), "sidebar lists the child: {text}");
    assert!(text.contains("audit-locks"), "{text}");
}

#[test]
fn terminal_screen_commands_toggle_bracketed_paste_symmetrically() {
    let mut entered = Vec::new();
    let mut left = Vec::new();
    enter_screen(&mut entered).unwrap();
    leave_screen(&mut left).unwrap();
    let entered = String::from_utf8(entered).unwrap();
    let left = String::from_utf8(left).unwrap();
    assert_eq!(entered.matches("\u{1b}[?2004h").count(), 1);
    assert_eq!(left.matches("\u{1b}[?2004l").count(), 1);
    assert!(!entered.contains("\u{1b}[?2004l"));
    assert!(!left.contains("\u{1b}[?2004h"));
}

fn profile_catalog() -> InferenceCatalog {
    InferenceCatalog {
        providers: vec![
            CatalogProvider {
                id: "opencode-go".into(),
                display_name: "OpenCode Go".into(),
                default_model: String::new(),
                models: vec![CatalogModel {
                    id: "deepseek-v4.1-flash".into(),
                    display_name: "DeepSeek V4.1 Flash".into(),
                    efforts: vec![
                        latch_protocol::ReasoningEffort::Low,
                        latch_protocol::ReasoningEffort::High,
                        latch_protocol::ReasoningEffort::Max,
                    ],
                    default_effort: latch_protocol::ReasoningEffort::Low,
                    input_modalities: vec![],
                }],
            },
            CatalogProvider {
                id: "anthropic".into(),
                display_name: "Anthropic".into(),
                default_model: String::new(),
                models: vec![CatalogModel {
                    id: "claude-sonnet-4-5".into(),
                    display_name: "Claude Sonnet 4.5".into(),
                    efforts: vec![],
                    default_effort: latch_protocol::ReasoningEffort::ProviderDefault,
                    input_modalities: vec![],
                }],
            },
        ],
    }
}

fn profile_app() -> App {
    let mut app = App::default();
    app.output(Output::InferenceCatalog(profile_catalog()));
    app.output(Output::Header {
        model: "deepseek-v4.1-flash".into(),
        provider: "OpenCode Go".into(),
        provider_id: "opencode-go".into(),
        effort: latch_protocol::ReasoningEffort::Low,
        workspace: "/tmp/latch".into(),
        branch: "main".into(),
        resumed: false,
        pricing: None,
    });
    app
}

#[test]
fn model_command_opens_a_provider_model_effort_selector() {
    let mut app = profile_app();
    app.input.set_text("/model");
    assert!(app.submit_action().is_none());
    assert!(app.profile_selector.is_some());
    // Provider step: choose the highlighted current provider.
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    // Model step: choose the highlighted model.
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    // Effort step preserves the current effort, so confirming immediately
    // keeps it.
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        action,
        Some(Action::SetInferenceProfile {
            ref provider,
            ref model,
            effort: latch_protocol::ReasoningEffort::Low,
        }) if provider == "opencode-go" && model == "deepseek-v4.1-flash"
    ));
    assert!(app.profile_selector.is_none());
}

fn many_model_catalog(count: usize) -> InferenceCatalog {
    InferenceCatalog {
        providers: vec![CatalogProvider {
            id: "opencode-go".into(),
            display_name: "OpenCode Go".into(),
            default_model: String::new(),
            models: (0..count)
                .map(|index| CatalogModel {
                    id: format!("model-{index:02}"),
                    display_name: format!("Model {index:02}"),
                    efforts: vec![],
                    default_effort: latch_protocol::ReasoningEffort::ProviderDefault,
                    input_modalities: vec![],
                })
                .collect(),
        }],
    }
}

/// Parse a `↑ N more` / `↓ N more` window marker from a rendered surface.
fn overflow_marker(text: &str, arrow: char) -> Option<usize> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(arrow)?;
        rest.strip_suffix(" more")?.trim().parse().ok()
    })
}

#[test]
fn model_picker_scrolls_and_never_hides_the_selected_model() {
    let count = 33;
    let mut app = App::default();
    app.output(Output::InferenceCatalog(many_model_catalog(count)));
    app.output(Output::Header {
        model: "model-00".into(),
        provider: "OpenCode Go".into(),
        provider_id: "opencode-go".into(),
        effort: latch_protocol::ReasoningEffort::ProviderDefault,
        workspace: "/tmp/latch".into(),
        branch: "main".into(),
        resumed: false,
        pricing: None,
    });
    app.input.set_text("/model");
    assert!(app.submit_action().is_none());
    // Provider step, then the model step.
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));

    // At the top the pointer is visible and overflow below is named.
    let text = render_to_text(&mut app, 120, 30);
    assert!(text.contains("› Model 00"), "{text}");
    assert!(overflow_marker(&text, '↓').is_some(), "{text}");
    assert!(overflow_marker(&text, '↑').is_none(), "{text}");

    // In the middle both overflow markers appear and the pointer follows.
    for _ in 0..20 {
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    }
    let text = render_to_text(&mut app, 120, 30);
    assert!(text.contains("› Model 20"), "{text}");
    assert!(overflow_marker(&text, '↑').is_some(), "{text}");
    assert!(overflow_marker(&text, '↓').is_some(), "{text}");

    // At the last row ("Change provider…") only the top marker remains.
    for _ in 0..(count - 20) {
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    }
    let text = render_to_text(&mut app, 120, 30);
    assert!(text.contains("› Change provider…"), "{text}");
    assert!(overflow_marker(&text, '↑').is_some(), "{text}");
    assert!(overflow_marker(&text, '↓').is_none(), "{text}");

    // Down wraps to the first model; the pointer is visible again.
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    let text = render_to_text(&mut app, 120, 30);
    assert!(text.contains("› Model 00"), "wrap-around works: {text}");

    // Every model can be brought into view with the pointer on it.
    for index in 0..count {
        let text = render_to_text(&mut app, 120, 30);
        let label = format!("Model {index:02}");
        assert!(
            text.contains(&format!("› {label}")),
            "model {label} is not visible when selected:\n{text}"
        );
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    }
}

#[test]
fn escape_cancels_the_model_selector_without_changing_the_profile() {
    let mut app = profile_app();
    app.input.set_text("/model");
    app.submit_action();
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    let action = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(action.is_none());
    assert!(app.profile_selector.is_none());
    assert_eq!(app.model, "deepseek-v4.1-flash");
    assert_eq!(app.provider_id, "opencode-go");
    assert_eq!(app.effort, latch_protocol::ReasoningEffort::Low);
}

#[test]
fn model_and_setup_commands_are_unavailable_during_a_live_turn() {
    let mut app = profile_app();
    app.busy = true;
    app.input.set_text("/model");
    assert!(app.submit_action().is_none());
    assert!(app.profile_selector.is_none());
    app.input.set_text("/setup");
    assert!(app.submit_action().is_none());
    assert!(app.setup.is_none());
}

#[test]
fn setup_flow_masks_the_secret_and_emits_a_secret_plan() {
    let mut app = App::default();
    app.output(Output::SetupCatalog(vec![SetupKind {
        kind: "openai".into(),
        label: "OpenAI".into(),
        default_base_url: "https://api.openai.com/v1".into(),
        credential_label: "env:OPENAI_API_KEY".into(),
        default_model: "gpt-5.5".into(),
        models: vec![CatalogModel {
            id: "gpt-5.5".into(),
            display_name: "GPT-5.5".into(),
            efforts: vec![latch_protocol::ReasoningEffort::High],
            default_effort: latch_protocol::ReasoningEffort::ProviderDefault,
            input_modalities: vec![],
        }],
    }]));
    app.input.set_text("/setup");
    assert!(app.submit_action().is_none());
    // Kind -> provider name capture, prefilled with the kind id.
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.capture.is_some());
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    // Name -> endpoint capture, prefilled with the catalog default.
    assert!(matches!(
        app.setup.as_ref().map(SetupFlow::step),
        Some(SetupStep::Endpoint)
    ));
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.capture.is_some());
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    // Credential list: choose "Enter API key securely".
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.capture
            .as_ref()
            .is_some_and(|capture| capture.spec.masked)
    );
    for ch in "sk-live-secret".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let rendered = render_to_text(&mut app, 100, 30);
    assert!(
        !rendered.contains("sk-live-secret"),
        "a secret value must never render: {rendered}"
    );
    assert!(rendered.contains("••"), "masked capture renders bullets");
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    // Model and effort steps.
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    let rendered = render_to_text(&mut app, 100, 30);
    assert!(
        rendered.contains("secure local storage (value hidden)"),
        "{rendered}"
    );
    assert!(!rendered.contains("sk-live-secret"));
    let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        action,
        Some(Action::SetupApply(SetupPlan::Apply {
            ref provider_kind,
            credential: SetupCredential::Secret(ref secret),
            ..
        })) if provider_kind == "openai" && secret == "sk-live-secret"
    ));
    // The secret never entered the durable-ish transcript or the composer
    // buffer; it travels only in the action payload.
    assert!(app.capture.is_none());
    assert!(app.input.text().is_empty());
    let visible = format!("{:?}", app.items);
    assert!(!visible.contains("sk-live-secret"), "{visible}");
}

#[test]
fn composer_metadata_always_shows_model_and_effort_adjacent() {
    let mut app = profile_app();
    let text = render_to_text(&mut app, 120, 24);
    assert!(
        text.contains("deepseek-v4.1-flash/low"),
        "footer shows model and effort together: {text}"
    );
}

#[test]
fn snapshot_model_selector_surface() {
    let mut app = profile_app();
    app.input.set_text("/model");
    app.submit_action();
    // Provider step.
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_snapshot("v6_model_selector.txt", &render_to_text(&mut app, 120, 30));
}

#[test]
fn snapshot_setup_review_surface_masks_the_credential() {
    let mut app = App::default();
    app.output(Output::SetupCatalog(vec![SetupKind {
        kind: "openai".into(),
        label: "OpenAI".into(),
        default_base_url: "https://api.openai.com/v1".into(),
        credential_label: "env:OPENAI_API_KEY".into(),
        default_model: "gpt-5.5".into(),
        models: vec![CatalogModel {
            id: "gpt-5.5".into(),
            display_name: "GPT-5.5".into(),
            efforts: vec![latch_protocol::ReasoningEffort::High],
            default_effort: latch_protocol::ReasoningEffort::ProviderDefault,
            input_modalities: vec![],
        }],
    }]));
    app.input.set_text("/setup");
    app.submit_action();
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // kind -> name capture
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // accept default name
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // endpoint capture
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // accept default endpoint
    app.on_key(key(KeyCode::Down, KeyModifiers::NONE)); // choose secure entry
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
    for ch in "sk-hidden".chars() {
        app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // secret submitted
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // model
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // effort
    let text = render_to_text(&mut app, 120, 34);
    assert!(!text.contains("sk-hidden"), "{text}");
    assert_snapshot("v6_setup_review.txt", &text);
}

// ---- multimodal image attachments ----

fn media_ref() -> latch_protocol::MediaRef {
    latch_protocol::MediaRef {
        id: "abcdef".into(),
        kind: latch_protocol::MediaKind::Image,
        mime_type: "image/png".into(),
        artifact_path: "media/abcdef.png".into(),
        sha256: "abcdef".into(),
        byte_len: 2048,
        width: Some(1440),
        height: Some(900),
        display_name: Some("screenshot.png".into()),
    }
}

fn catalog_for(model: &str, image: bool) -> InferenceCatalog {
    let modalities = if image {
        vec![
            latch_protocol::InputModality::Text,
            latch_protocol::InputModality::Image,
        ]
    } else {
        vec![latch_protocol::InputModality::Text]
    };
    InferenceCatalog {
        providers: vec![CatalogProvider {
            id: "custom".into(),
            display_name: "Custom".into(),
            default_model: model.into(),
            models: vec![CatalogModel {
                id: model.into(),
                display_name: model.into(),
                efforts: vec![],
                default_effort: ReasoningEffort::ProviderDefault,
                input_modalities: modalities,
            }],
        }],
    }
}

fn submit_text(app: &mut App, text: &str) -> Option<Action> {
    app.input.set_text(text);
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
}

#[test]
fn attach_command_delegates_ingestion_to_the_kernel() {
    let mut app = App::default();
    let action = submit_text(&mut app, "/attach shots/regression.png");
    assert!(
        matches!(action, Some(Action::Attach(ref path)) if path == "shots/regression.png"),
        "{action:?}"
    );
    assert!(
        app.attachments.is_empty(),
        "the kernel confirms ingestion before the attachment is pending"
    );
    // Quoted paths are accepted.
    let action = submit_text(&mut app, "/attach \"my shot.png\"");
    assert!(matches!(action, Some(Action::Attach(ref path)) if path == "my shot.png"));
    // A bare /attach explains usage instead of sending it to the model.
    let action = submit_text(&mut app, "/attach");
    assert!(action.is_none());
}

#[test]
fn attachments_lists_and_detaches_pending_images() {
    let mut app = App::default();
    app.output(Output::Attachment(media_ref()));
    assert_eq!(
        app.attachment_summary().as_deref(),
        Some("[image: screenshot.png · 1440×900]")
    );

    // /attachments reports the compact metadata without rendering bytes.
    app.output(Output::Attachment(media_ref()));
    assert!(submit_text(&mut app, "/attachments").is_none());
    let notices = format!("{:?}", app.presentation.cells());
    assert_eq!(notices.matches("screenshot.png").count(), 2, "{notices}");
    assert!(!notices.contains("iVBOR"), "no encoded bytes in the UI");

    // /detach removes by 1-based index; invalid indexes are rejected.
    assert!(submit_text(&mut app, "/detach 1").is_none());
    assert_eq!(app.attachments.len(), 1);
    assert!(submit_text(&mut app, "/detach 9").is_none());
    assert_eq!(app.attachments.len(), 1);
    assert!(submit_text(&mut app, "/detach all").is_none());
    assert!(app.attachments.is_empty());
    assert!(app.attachment_summary().is_none());
}

#[test]
fn submitting_carries_pending_attachments_and_clears_them() {
    let mut app = App::default();
    app.output(Output::Attachment(media_ref()));
    let action = submit_text(&mut app, "inspect this UI regression");
    match action {
        Some(Action::Submit { text, media }) => {
            assert_eq!(text, "inspect this UI regression");
            assert_eq!(media.len(), 1);
            assert_eq!(
                media[0].compact_label(),
                "[image: screenshot.png · 1440×900]"
            );
        }
        other => panic!("unexpected action {other:?}"),
    }
    assert!(app.attachments.is_empty(), "attachments move with the turn");

    // An image can be sent without text.
    app.output(Output::Attachment(media_ref()));
    let action = submit_text(&mut app, "");
    assert!(
        matches!(action, Some(Action::Submit { ref text, ref media })
        if text.is_empty() && media.len() == 1)
    );
}

#[test]
fn text_only_model_keeps_pending_attachments_until_the_model_changes() {
    let mut app = App {
        provider_id: "custom".into(),
        model: "text-only".into(),
        inference_catalog: catalog_for("text-only", false),
        ..Default::default()
    };
    app.output(Output::Attachment(media_ref()));
    let action = submit_text(&mut app, "look");
    assert!(
        action.is_none(),
        "a known text-only model rejects locally instead of dropping the image"
    );
    assert_eq!(app.attachments.len(), 1);
    assert!(
        format!("{:?}", app.presentation.cells()).contains("does not accept image input"),
        "the composer explains the rejection"
    );
    // Selecting a vision model allows the same pending attachment to send.
    app.model = "vision-model".into();
    app.inference_catalog = catalog_for("vision-model", true);
    let action = submit_text(&mut app, "look");
    assert!(matches!(action, Some(Action::Submit { ref media, .. }) if media.len() == 1));
}

#[test]
fn unknown_model_capability_does_not_block_submission() {
    let mut app = App {
        provider_id: "custom".into(),
        model: "mystery".into(),
        ..Default::default()
    };
    app.output(Output::Attachment(media_ref()));
    // The kernel re-checks authoritatively, so an unknown catalog entry is
    // passed through rather than guessed at.
    let action = submit_text(&mut app, "look");
    assert!(matches!(action, Some(Action::Submit { ref media, .. }) if media.len() == 1));
}

#[test]
fn user_transcript_renders_compact_attachment_metadata_without_bytes() {
    let mut app = App::default();
    app.output(Output::Event(Box::new(presentation_event(
        latch_protocol::EventPayload::UserMessage {
            text: "inspect this UI regression".into(),
            media: vec![media_ref()],
        },
    ))));
    let rendered = render_to_text(&mut app, 80, 16);
    assert!(
        rendered.contains("inspect this UI regression"),
        "{rendered}"
    );
    assert!(
        rendered.contains("[image: screenshot.png · 1440×900]"),
        "{rendered}"
    );
    assert!(!rendered.contains("iVBOR"), "no base64 in the transcript");

    // The palette advertises the new commands and /help lists them.
    for command in ["/attach", "/attachments", "/detach", "/group"] {
        assert!(
            SLASH_COMMANDS.iter().any(|entry| entry.name == command),
            "missing {command}"
        );
    }
}
