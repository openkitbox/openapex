mod support;

use ox_client::Client;
use ox_client::rest::ShellReadMode;
use support::MockOx;

#[tokio::test]
async fn location_echoes_directory_and_parses_project() {
    let mock = MockOx::default();
    mock.reply(
        "/api/location",
        r#"{"directory":"/proj","project":{"id":"prj_1","directory":"/proj","canonical":"/proj"}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let loc = client.location().await.unwrap();
    assert_eq!(loc.project.id, "prj_1");

    let requests = mock.requests();
    let first = requests.first().unwrap();
    assert!(first.starts_with("GET /api/location?location%5Bdirectory%5D=%2Fproj"));
    assert!(first.to_lowercase().contains("x-opencode-directory: /proj"));
}

#[tokio::test]
async fn session_directory_reads_the_stored_location() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session/ses_1",
        r#"{"data":{"id":"ses_1","location":{"directory":"/tmp/stored"}}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let directory = client.session_directory("ses_1").await.unwrap();
    assert_eq!(directory.as_deref(), Some("/tmp/stored"));
    assert!(mock.requests()[0].starts_with("GET /api/session/ses_1"));
}

#[tokio::test]
async fn session_info_reads_agent_and_model() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session/ses_1",
        r#"{"data":{"id":"ses_1","agent":"build","model":{"id":"k3-256k","providerID":"cardinal","variant":"high"}}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let info = client.session_info("ses_1").await.unwrap();
    assert_eq!(info.agent.as_deref(), Some("build"));
    assert_eq!(info.model_id.as_deref(), Some("k3-256k"));
    assert_eq!(info.provider_id.as_deref(), Some("cardinal"));
    assert_eq!(info.variant.as_deref(), Some("high"));
    assert_eq!(info.directory, None);
}

#[tokio::test]
async fn session_info_reads_the_busy_and_idle_times() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session/ses_c",
        r#"{"data":{"id":"ses_c","time":{"created":1,"updated":9,"busy":5}}}"#,
    );
    mock.reply(
        "/api/session/ses_d",
        r#"{"data":{"id":"ses_d","time":{"created":1,"updated":9,"busy":5,"idle":6}}}"#,
    );
    mock.reply("/api/session/ses_e", r#"{"data":{"id":"ses_e"}}"#);
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    assert!(
        client
            .session_info("ses_c")
            .await
            .unwrap()
            .time
            .is_running()
    );
    assert!(
        !client
            .session_info("ses_d")
            .await
            .unwrap()
            .time
            .is_running()
    );
    assert!(
        !client
            .session_info("ses_e")
            .await
            .unwrap()
            .time
            .is_running()
    );
}

#[tokio::test]
async fn create_session_posts_directory() {
    let mock = MockOx::default();
    mock.reply("/api/session", r#"{"data":{"id":"ses_1"}}"#);
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let id = client.create_session().await.unwrap();
    assert_eq!(id, "ses_1");

    let requests = mock.requests();
    let first = requests.first().unwrap();
    assert!(first.starts_with("POST /api/session"));
    assert!(first.contains(r#"{"location":{"directory":"/proj"}}"#));
}

#[tokio::test]
async fn prompt_posts_text_and_interrupt_uses_session_path() {
    let mock = MockOx::default();
    mock.reply("/api/session/ses_1/prompt", r#"{"id":"msg_1"}"#);
    mock.reply("/api/session/ses_1/interrupt", r#"{}"#);
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let _ = client.prompt("ses_1", "hi", &[]).await.unwrap();
    client.interrupt("ses_1").await.unwrap();

    let requests = mock.requests();
    assert!(requests.iter().any(|request| {
        request.starts_with("POST /api/session/ses_1/prompt") && request.contains(r#""text":"hi""#)
    }));
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("POST /api/session/ses_1/interrupt"))
    );
}

#[tokio::test]
async fn messages_first_page_uses_order_and_next_page_uses_only_cursor() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session/ses_1/message",
        r#"{"data":[],"cursor":{"previous":null,"next":"cur_2"}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let page = client.messages("ses_1", None).await.unwrap();
    assert_eq!(page.cursor.next.as_deref(), Some("cur_2"));

    let _ = client.messages("ses_1", Some("cur_2")).await.unwrap();
    let requests = mock.requests();
    let second = requests.last().unwrap();
    assert!(second.contains("cursor=cur_2"));
    assert!(!second.contains("order="));
}

#[tokio::test]
async fn forms_lists_the_session_pending_forms() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session/ses_1/form",
        r#"{"data":[{"id":"frm_1","sessionID":"ses_1","title":"Pick","fields":[{"type":"boolean","key":"ok","default":true}]}]}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let forms = client.forms("ses_1").await.unwrap();
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0].id, "frm_1");
    assert!(mock.requests()[0].starts_with("GET /api/session/ses_1/form "));
}

#[tokio::test]
async fn reply_form_posts_the_answer_map() {
    let mock = MockOx::default();
    mock.reply_raw(
        "/api/session/ses_1/form/frm_1/reply",
        "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n",
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let mut answer = ox_client::form::Answer::new();
    answer.insert("q0".into(), ox_client::form::FormValue::String("a".into()));
    answer.insert(
        "q1".into(),
        ox_client::form::FormValue::Multiple(vec!["x".into(), "y".into()]),
    );
    client.reply_form("ses_1", "frm_1", &answer).await.unwrap();

    let request = &mock.requests()[0];
    assert!(request.starts_with("POST /api/session/ses_1/form/frm_1/reply "));
    assert!(request.contains(r#""answer":{"#));
    assert!(request.contains(r#""q0":"a""#));
    assert!(request.contains(r#""q1":["x","y"]"#));
}

#[tokio::test]
async fn cancel_form_deletes_and_surfaces_a_settled_conflict() {
    let mock = MockOx::default();
    mock.reply_raw(
        "/api/session/ses_1/form/frm_1",
        "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n",
    );
    mock.reply_raw(
        "/api/session/ses_1/form/frm_2",
        "HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    client.cancel_form("ses_1", "frm_1").await.unwrap();
    assert!(mock.requests()[0].starts_with("DELETE /api/session/ses_1/form/frm_1 "));

    let err = client.cancel_form("ses_1", "frm_2").await.unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 409));
}

#[tokio::test]
async fn models_lists_the_catalogue() {
    let mock = MockOx::default();
    mock.reply(
        "/api/model",
        r#"{"location":{"directory":"/proj"},"data":[{"id":"m","modelID":"m","providerID":"p","name":"M","limit":{"context":200000}}]}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let models = client.models().await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].limit.context, 200_000);
    assert_eq!(models[0].provider_id, "p");
    assert!(mock.requests()[0].starts_with("GET /api/model "));
}

#[tokio::test]
async fn switch_model_and_agent_post_the_wire_payloads() {
    let mock = MockOx::default();
    let no_content = "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n";
    mock.reply_raw("/api/session/ses_1/model", no_content);
    mock.reply_raw("/api/session/ses_1/agent", no_content);
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    client
        .switch_model("ses_1", "cardinal", "k3")
        .await
        .unwrap();
    client.switch_agent("ses_1", "plan").await.unwrap();

    let requests = mock.requests();
    assert!(requests[0].starts_with("POST /api/session/ses_1/model "));
    assert!(requests[0].contains(r#"{"model":{"id":"k3","providerID":"cardinal"}}"#));
    assert!(requests[1].starts_with("POST /api/session/ses_1/agent "));
    assert!(requests[1].contains(r#"{"agent":"plan"}"#));
}

#[tokio::test]
async fn switch_model_surfaces_an_unknown_session() {
    let mock = MockOx::default(); // unregistered path answers 404
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let err = client.switch_model("ses_x", "p", "m").await.unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 404));
}

#[tokio::test]
async fn sessions_lists_root_sessions_newest_first() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session",
        r#"{"data":[{"id":"ses_2","title":"Fix bug","time":{"created":1,"updated":9},"location":{"directory":"/proj/sub"}},{"id":"ses_1","time":{"created":1,"updated":2}}],"cursor":{"previous":null,"next":null}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let sessions = client.sessions().await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].title.as_deref(), Some("Fix bug"));
    assert_eq!(sessions[0].time.updated, 9);
    assert_eq!(
        sessions[0].location.as_ref().unwrap().directory,
        "/proj/sub"
    );
    assert_eq!(sessions[1].title, None);
    assert!(sessions[1].location.is_none());

    let request = &mock.requests()[0];
    assert!(request.starts_with("GET /api/session?"));
    for part in ["limit=50", "order=desc", "parentID=null"] {
        assert!(request.contains(part), "{part} missing in {request}");
    }
}

#[tokio::test]
async fn rename_session_patches_the_title() {
    let mock = MockOx::default();
    mock.reply_raw(
        "/api/session/ses_1",
        "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n",
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    client.rename_session("ses_1", "New name").await.unwrap();
    let request = &mock.requests()[0];
    assert!(request.starts_with("PATCH /api/session/ses_1 "));
    assert!(request.contains(r#"{"title":"New name"}"#));
}

#[tokio::test]
async fn delete_session_deletes_and_surfaces_not_found() {
    let mock = MockOx::default();
    mock.reply_raw(
        "/api/session/ses_1",
        "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n",
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    client.delete_session("ses_1").await.unwrap();
    assert!(mock.requests()[0].starts_with("DELETE /api/session/ses_1 "));

    let err = client.delete_session("ses_gone").await.unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 404));
}

#[tokio::test]
async fn fork_session_posts_and_returns_the_new_id() {
    let mock = MockOx::default();
    mock.reply("/api/session/ses_1/fork", r#"{"data":{"id":"ses_2"}}"#);
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    assert_eq!(client.fork_session("ses_1").await.unwrap(), "ses_2");
    assert!(mock.requests()[0].starts_with("POST /api/session/ses_1/fork "));
}

#[tokio::test]
async fn find_files_queries_fs_find_and_parses_entries() {
    let mock = MockOx::default();
    mock.reply(
        "/api/fs/find",
        r#"{"location":{"directory":"/proj"},"data":[{"path":"src","type":"directory"},{"path":"src/main.rs","type":"file"}]}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let entries = client.find_files("ma in").await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "src");
    assert!(entries[0].is_dir());
    assert_eq!(entries[1].path, "src/main.rs");
    assert!(!entries[1].is_dir());

    let request = &mock.requests()[0];
    assert!(request.starts_with("GET /api/fs/find?"), "{request}");
    assert!(request.contains("query=ma%20in") || request.contains("query=ma+in"));
    assert!(request.contains("limit=20"));
    assert!(
        request
            .to_lowercase()
            .contains("x-opencode-directory: /proj")
    );
}

#[tokio::test]
async fn session_info_reads_the_parent_id() {
    let mock = MockOx::default();
    mock.reply(
        "/api/session/ses_c",
        r#"{"data":{"id":"ses_c","parentID":"ses_p"}}"#,
    );
    mock.reply("/api/session/ses_r", r#"{"data":{"id":"ses_r"}}"#);
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    assert_eq!(
        client
            .session_info("ses_c")
            .await
            .unwrap()
            .parent_id
            .as_deref(),
        Some("ses_p")
    );
    assert_eq!(client.session_info("ses_r").await.unwrap().parent_id, None);
}

#[tokio::test]
async fn session_family_is_one_request_to_the_ox_endpoint() {
    let mock = MockOx::default();
    mock.reply(
        "/ox/session/ses_c/family",
        r#"{"location":{"directory":"/proj"},"data":[
            {"id":"ses_root","title":"main","agent":"build","parentID":null,"time":{"created":1,"updated":5},"running":false},
            {"id":"ses_c","title":null,"agent":"explore","parentID":"ses_root","time":{"created":2,"updated":3},"running":true}
        ]}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let family = client.session_family("ses_c").await.unwrap();
    assert_eq!(family.len(), 2);
    assert_eq!(family[0].id, "ses_root");
    assert_eq!(family[0].title.as_deref(), Some("main"));
    assert_eq!(family[0].agent.as_deref(), Some("build"));
    assert_eq!(family[0].parent_id, None);
    assert!(!family[0].running);
    assert_eq!(family[0].time.updated, 5);
    assert_eq!(family[1].title, None);
    assert_eq!(family[1].agent.as_deref(), Some("explore"));
    assert_eq!(family[1].parent_id.as_deref(), Some("ses_root"));
    assert!(family[1].running);

    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].starts_with("GET /ox/session/ses_c/family "),
        "{}",
        requests[0]
    );
    assert!(
        requests[0]
            .to_lowercase()
            .contains("x-opencode-directory: /proj"),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn session_family_surfaces_a_failed_status() {
    let mock = MockOx::default(); // unregistered path answers 404
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let err = client.session_family("ses_x").await.unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 404));
}

#[tokio::test]
async fn shells_parse_the_wire_shape() {
    let mock = MockOx::default();
    mock.reply(
        "/api/shell",
        r#"{"location":{"directory":"/proj"},"data":[
            {"id":"shl_1","status":"running","command":"sleep 99","cwd":"/proj","shell":"zsh","file":"ox-shell://shl_1","pid":42,"metadata":{"sessionID":"ses_1"},"time":{"started":7}},
            {"id":"shl_2","status":"exited","command":"true","cwd":"/proj","shell":"zsh","file":"ox-shell://shl_2","pid":43,"exit":0,"metadata":{},"time":{"started":8,"completed":9}}
        ]}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let shells = client.shells().await.unwrap();
    assert_eq!(shells.len(), 2);
    assert_eq!(shells[0].status, "running");
    assert_eq!(shells[0].command, "sleep 99");
    assert_eq!(shells[0].pid, Some(42));
    assert_eq!(shells[0].exit, None);
    assert_eq!(shells[0].signal, None);
    assert_eq!(shells[0].session_id(), Some("ses_1"));
    assert_eq!(shells[0].time.started, 7);
    assert_eq!(shells[0].time.completed, None);
    assert_eq!(shells[1].exit, Some(0));
    assert_eq!(shells[1].session_id(), None);
    assert_eq!(shells[1].time.completed, Some(9));
    assert!(mock.requests()[0].starts_with("GET /api/shell"));
}

#[tokio::test]
async fn shell_output_reads_a_page_from_the_cursor() {
    let mock = MockOx::default();
    mock.reply(
        "/api/shell/shl_1/output",
        r#"{"location":{"directory":"/proj"},"data":{"output":"hi\n","cursor":3,"size":10,"truncated":true}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let page = client.shell_output("shl_1", 0).await.unwrap();
    assert_eq!(page.output, "hi\n");
    assert_eq!(page.cursor, 3);
    assert_eq!(page.size, 10);
    assert!(page.truncated);
    let request = &mock.requests()[0];
    assert!(
        request.starts_with("GET /api/shell/shl_1/output?"),
        "{request}"
    );
    assert!(request.contains("cursor=0"), "{request}");
}

#[tokio::test]
async fn kill_shell_sends_a_delete() {
    let mock = MockOx::default();
    mock.reply_raw(
        "/api/shell/shl_1",
        "HTTP/1.1 204 No Content\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    client.kill_shell("shl_1").await.unwrap();
    assert!(mock.requests()[0].starts_with("DELETE /api/shell/shl_1"));

    let err = client.kill_shell("shl_missing").await.unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 404));
}

fn shell_page(output: &str, cursor: u64, size: u64) -> String {
    format!(
        r#"{{"location":{{"directory":"/proj"}},"data":{{"output":{},"cursor":{cursor},"size":{size},"truncated":false}}}}"#,
        serde_json::to_string(output).expect("json string")
    )
}

#[tokio::test]
async fn shell_tail_probes_the_size_then_reads_from_size_minus_max() {
    let mock = MockOx::default();
    mock.reply("/api/shell/shl_1/output", &shell_page("tail", 10, 10));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let tail = client.shell_tail("shl_1", 4).await.unwrap();
    assert_eq!(tail.text, "tail");
    assert!(tail.omitted, "bytes before the start cursor were skipped");
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[0].contains("cursor=9007199254740991"),
        "{requests:?}"
    );
    assert!(requests[1].contains("cursor=6"), "{requests:?}");
}

#[tokio::test]
async fn shell_tail_of_a_small_capture_starts_at_zero_and_omits_nothing() {
    let mock = MockOx::default();
    mock.reply("/api/shell/shl_1/output", &shell_page("hi\n", 3, 3));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let tail = client.shell_tail("shl_1", 64 * 1024).await.unwrap();
    assert_eq!(tail.text, "hi\n");
    assert!(!tail.omitted);
    assert!(
        mock.requests()[1].contains("cursor=0"),
        "{:?}",
        mock.requests()
    );
}

#[tokio::test]
async fn shell_tail_drops_the_split_character_at_the_start() {
    let mock = MockOx::default();
    mock.reply(
        "/api/shell/shl_1/output",
        &shell_page("\u{fffd}\u{fffd}ok", 10, 10),
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let tail = client.shell_tail("shl_1", 4).await.unwrap();
    assert_eq!(tail.text, "ok", "a char cut by the cursor is not shown");
    assert!(tail.omitted);
}

#[tokio::test]
async fn shell_tail_keeps_only_the_newest_max_bytes_on_a_char_boundary() {
    let mock = MockOx::default();
    mock.reply("/api/shell/shl_1/output", &shell_page("aé世b", 7, 7));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    // The newest 3 bytes start inside "世" (bytes 3..6): the cut moves
    // forward to the next char boundary.
    let tail = client.shell_tail("shl_1", 3).await.unwrap();
    assert_eq!(tail.text, "b");
    assert!(tail.omitted);
}

#[tokio::test]
async fn shell_tail_stops_when_the_server_makes_no_progress() {
    let mock = MockOx::default();
    // The cursor never reaches `size`: the loop must still end.
    mock.reply("/api/shell/shl_1/output", &shell_page("x", 3, 10));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let tail = client.shell_tail("shl_1", 4).await.unwrap();
    assert_eq!(tail.text, "x");
    assert!(mock.request_count() <= 3, "{:?}", mock.requests());
}

#[tokio::test]
async fn shell_tail_of_a_missing_shell_is_a_status_error() {
    let mock = MockOx::default();
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let err = client.shell_tail("shl_gone", 4).await.unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 404));
}

#[tokio::test]
async fn shell_tail_reports_omitted_when_the_server_dropped_the_head() {
    let mock = MockOx::default();
    mock.reply(
        "/api/shell/shl_1/output",
        r#"{"data":{"output":"late","cursor":4,"size":4,"truncated":true}}"#,
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let tail = client.shell_tail("shl_1", 64 * 1024).await.unwrap();
    assert!(tail.omitted, "a capture that lost its head is partial");
}

fn shell_page_truncated(output: &str, cursor: u64, size: u64) -> String {
    format!(
        r#"{{"data":{{"output":{},"cursor":{cursor},"size":{size},"truncated":true}}}}"#,
        serde_json::to_string(output).expect("json string")
    )
}

#[tokio::test]
async fn shell_read_page_mode_reads_one_limited_page_from_the_cursor() {
    let mock = MockOx::default();
    mock.reply("/api/shell/shl_1/output", &shell_page("more", 7, 20));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let read = client
        .shell_read("shl_1", 3, ShellReadMode::Page, 1024)
        .await
        .unwrap();
    assert_eq!(read.text, "more");
    assert_eq!(read.end, 7);
    assert!(!read.skipped);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "a poll reads one page: {requests:?}");
    assert!(requests[0].contains("cursor=3"), "{requests:?}");
    assert!(requests[0].contains("limit=1024"), "{requests:?}");
}

#[tokio::test]
async fn shell_read_drain_pages_up_to_the_size() {
    let mock = MockOx::default();
    mock.reply("/api/shell/shl_1/output?cursor=0&", &shell_page("ab", 2, 4));
    mock.reply("/api/shell/shl_1/output?cursor=2&", &shell_page("cd", 4, 4));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let read = client
        .shell_read("shl_1", 0, ShellReadMode::Drain, 4)
        .await
        .unwrap();
    assert_eq!(read.text, "abcd");
    assert_eq!(read.end, 4);
    assert!(!read.skipped);
    assert_eq!(mock.request_count(), 2, "{:?}", mock.requests());
}

#[tokio::test]
async fn shell_read_drain_stops_at_the_size_seen_first_while_the_shell_keeps_writing() {
    let mock = MockOx::default();
    // Every page reports two more bytes than it returns: a shell writing as
    // fast as it is read. The drain targets the first size (4) and stops.
    for cursor in (0..100u64).step_by(2) {
        mock.reply(
            format!("/api/shell/shl_1/output?cursor={cursor}&"),
            &shell_page("xx", cursor + 2, cursor + 4),
        );
    }
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let read = client
        .shell_read("shl_1", 0, ShellReadMode::Drain, 2)
        .await
        .unwrap();
    assert_eq!(read.end, 4, "drained to the first size seen");
    assert_eq!(read.text, "xxxx");
    assert_eq!(mock.request_count(), 2, "{:?}", mock.requests());
}

#[tokio::test]
async fn shell_read_drain_jumps_to_the_newest_max_bytes() {
    let mock = MockOx::default();
    // 10 bytes exist and the cap is 4: after the first page the read jumps
    // to `size - 4` and keeps only what it read from there.
    mock.reply(
        "/api/shell/shl_1/output?cursor=0&",
        &shell_page("0123", 4, 10),
    );
    mock.reply(
        "/api/shell/shl_1/output?cursor=6&",
        &shell_page("6789", 10, 10),
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let read = client
        .shell_read("shl_1", 0, ShellReadMode::Drain, 4)
        .await
        .unwrap();
    assert_eq!(read.text, "6789");
    assert_eq!(read.end, 10);
    assert!(read.skipped, "bytes 4..6 were never read");
}

#[tokio::test]
async fn shell_read_reports_a_skip_when_the_server_dropped_the_cursor() {
    let mock = MockOx::default();
    mock.reply(
        "/api/shell/shl_1/output",
        &shell_page_truncated("late", 12, 12),
    );
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let read = client
        .shell_read("shl_1", 2, ShellReadMode::Page, 1024)
        .await
        .unwrap();
    assert_eq!(read.text, "late");
    assert!(read.skipped, "the server no longer holds bytes from 2");
}

#[tokio::test]
async fn shell_read_stops_when_the_cursor_does_not_move() {
    let mock = MockOx::default();
    mock.reply("/api/shell/shl_1/output", &shell_page("", 5, 9));
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let read = client
        .shell_read("shl_1", 5, ShellReadMode::Drain, 1024)
        .await
        .unwrap();
    assert_eq!(read.text, "");
    assert_eq!(read.end, 5);
    assert_eq!(mock.request_count(), 1, "{:?}", mock.requests());
}

#[tokio::test]
async fn shell_read_of_a_missing_shell_is_a_status_error() {
    let mock = MockOx::default();
    let base = mock.start().await;
    let client = Client::new(base, "/proj");

    let err = client
        .shell_read("shl_gone", 0, ShellReadMode::Page, 4)
        .await
        .unwrap_err();
    assert!(matches!(err, ox_client::Error::Status(status) if status.as_u16() == 404));
}
