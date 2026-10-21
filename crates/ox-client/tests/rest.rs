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

    let _ = client.prompt("ses_1", "hi").await.unwrap();
    client.interrupt("ses_1").await.unwrap();

    let requests = mock.requests();
    assert!(requests.iter().any(|request| {
        request.starts_with("POST /api/session/ses_1/prompt")
            && request.contains(r#""text":"hi""#)
    }));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("POST /api/session/ses_1/interrupt")));
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
