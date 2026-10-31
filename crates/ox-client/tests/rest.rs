mod support;

use ox_client::Client;
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
