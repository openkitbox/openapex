use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ox_client::sse::StreamMsg;
use ox_schema::event::EventEnvelope;
use ox_tui::app::App;
use ox_tui::app::command::AppCommand;
use ox_tui::app::event::AppEvent;
use ox_tui::app::state::Route;
use ox_tui::ui::commands;
use ox_tui::ui::prompt::Chrome;
use tokio::sync::{mpsc, watch};

fn app(route: Route) -> (App, mpsc::UnboundedReceiver<AppCommand>) {
    let (commands, rx) = mpsc::unbounded_channel();
    let (seq, _) = watch::channel(None);
    let session = (route == Route::Session).then(|| "session".to_owned());
    let chrome = Chrome {
        model: Some("chosen".into()),
        provider_id: Some("provider".into()),
        ..Chrome::default()
    };
    (
        App::new(route, session, chrome, "/project".into(), commands, seq),
        rx,
    )
}

fn submit(app: &mut App, text: &str) {
    app.handle(AppEvent::Paste(text.into()));
    app.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

#[test]
fn refresh_and_reload_are_registered_as_the_same_command() {
    let rows = commands::rows("");
    let refresh = rows.iter().find(|row| row.name == "refresh");
    let reload = rows.iter().find(|row| row.name == "reload");
    assert!(refresh.is_some(), "/refresh must be discoverable");
    assert!(reload.is_some(), "/reload must be discoverable");
    assert_eq!(refresh.map(|row| row.action), reload.map(|row| row.action));
}

#[test]
fn refresh_runs_on_home_and_session_and_refreshes_home_data() {
    for route in [Route::Home, Route::Session] {
        let (mut app, mut rx) = app(route);
        submit(&mut app, "/reload");
        let Some(AppCommand::ReloadConfig { request_id }) = rx.try_recv().ok() else {
            panic!("reload must be sent on {route:?}");
        };
        app.handle(AppEvent::ConfigReloaded {
            request_id,
            result: Ok(()),
        });
        assert!(
            matches!(rx.try_recv(), Ok(AppCommand::McpList { .. })),
            "MCP status must be refreshed"
        );
        assert!(
            matches!(rx.try_recv(), Ok(AppCommand::ModelList { .. })),
            "model catalog must be refreshed"
        );
        assert_eq!(
            app.prompt.chrome().model.as_deref(),
            Some("chosen"),
            "reload must retain selection"
        );
    }
}

#[test]
fn model_updates_bypass_session_filter_and_stale_catalog_is_ignored() {
    let (mut app, mut rx) = app(Route::Session);
    for kind in ["provider.updated", "model.updated"] {
        let envelope = EventEnvelope {
            id: "event".into(),
            created: 0,
            kind: kind.into(),
            data: serde_json::json!({}),
            durable: None,
            location: None,
            metadata: None,
        };
        app.handle(AppEvent::Stream(StreamMsg::Event(Box::new(envelope))));
    }
    let Ok(AppCommand::ModelList { request_id: old }) = rx.try_recv() else {
        panic!("provider update must fetch catalog");
    };
    assert!(
        rx.try_recv().is_err(),
        "concurrent update frames must share an in-flight request"
    );
    app.handle(AppEvent::Models {
        request_id: old,
        result: Err("temporary failure".into()),
    });
    let Ok(AppCommand::ModelList { request_id: new }) = rx.try_recv() else {
        panic!("update received during an in-flight request must be reconciled");
    };
    assert!(new > old, "requests must be ordered");
    app.handle(AppEvent::Models {
        request_id: old,
        result: Ok((vec![], vec![])),
    });
    assert_eq!(
        app.prompt.chrome().model.as_deref(),
        Some("chosen"),
        "old catalog must not alter selection"
    );
}

#[test]
fn reconnect_replaces_inflight_catalog_even_if_old_request_never_finishes() {
    let (mut app, mut rx) = app(Route::Session);
    submit(&mut app, "/models");
    let Ok(AppCommand::ModelList { request_id: old }) = rx.try_recv() else {
        panic!("model picker must fetch catalog");
    };
    app.handle(AppEvent::Stream(StreamMsg::Reconnected));
    let new = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|command| match command {
            AppCommand::ModelList { request_id } => Some(request_id),
            _ => None,
        })
        .expect("reconnection must fetch catalog despite the old request");
    assert!(new > old, "reconnect needs a fresh request");
    app.handle(AppEvent::Models {
        request_id: old,
        result: Err("stale".into()),
    });
    app.handle(AppEvent::Models {
        request_id: new,
        result: Ok((vec![], vec![])),
    });
    assert_eq!(app.prompt.chrome().model.as_deref(), Some("chosen"));
}

#[test]
fn reload_callback_and_update_frame_coalesce_catalog_refreshes() {
    let (mut app, mut rx) = app(Route::Session);
    submit(&mut app, "/reload");
    let Some(AppCommand::ReloadConfig { request_id }) = rx.try_recv().ok() else {
        panic!("reload must be sent");
    };
    app.handle(AppEvent::ConfigReloaded {
        request_id,
        result: Ok(()),
    });
    assert!(matches!(rx.try_recv(), Ok(AppCommand::McpList { .. })));
    let Ok(AppCommand::ModelList {
        request_id: catalog,
    }) = rx.try_recv()
    else {
        panic!("reload must refresh the catalog");
    };
    let envelope = EventEnvelope {
        id: "event".into(),
        created: 0,
        kind: "provider.updated".into(),
        data: serde_json::json!({}),
        durable: None,
        location: None,
        metadata: None,
    };
    app.handle(AppEvent::Stream(StreamMsg::Event(Box::new(envelope))));
    assert!(
        rx.try_recv().is_err(),
        "provider update must coalesce with the callback request"
    );
    app.handle(AppEvent::Models {
        request_id: catalog,
        result: Ok((vec![], vec![])),
    });
    assert!(matches!(rx.try_recv(), Ok(AppCommand::ModelList { .. })));
}
