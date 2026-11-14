use std::collections::HashMap;

use salvo::prelude::*;
use serde::Deserialize;

use super::response::{invalid_request, no_content, parse_body, session_not_found, unknown_error};
use super::session_row::load_session;
use crate::api::state;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentBody {
    #[serde(default)]
    variables: Option<HashMap<String, String>>,
}

/// Replace the environment inherited by shells owned by a session.
#[handler] // operation_id: session.environment
pub(crate) async fn set(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(body) = parse_body::<EnvironmentBody>(req, res).await else {
        return;
    };
    let Some(variables) = required_variables(body) else {
        invalid_request(res, "Request body must contain variables");
        return;
    };

    match load_session(&app.db, &session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            session_not_found(res, &session_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    }

    app.services()
        .tools
        .shells
        .set_environment(&session_id, variables);
    no_content(res);
}

fn required_variables(body: EnvironmentBody) -> Option<HashMap<String, String>> {
    body.variables
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::EnvironmentBody;

    #[test]
    fn environment_body_requires_variables_map() {
        assert_eq!(
            super::required_variables(
                serde_json::from_str::<EnvironmentBody>(r#"{"variables":{"A":"one"}}"#)
                    .expect("valid environment body"),
            ),
            Some(HashMap::from([(String::from("A"), String::from("one"))]))
        );
        assert_eq!(
            super::required_variables(
                serde_json::from_str::<EnvironmentBody>(r#"{}"#)
                    .expect("empty object is syntactically valid"),
            ),
            None
        );
        assert_eq!(
            serde_json::from_str::<EnvironmentBody>(r#"{"variables":null}"#)
                .ok()
                .and_then(super::required_variables),
            None
        );
        assert!(serde_json::from_str::<EnvironmentBody>(r#"{"variables":{"A":1}}"#).is_err());
        assert!(
            serde_json::from_str::<EnvironmentBody>(r#"{"variables":{},"extra":true}"#).is_err()
        );
        assert_eq!(
            serde_json::from_str::<EnvironmentBody>("[]")
                .ok()
                .and_then(super::required_variables),
            None
        );
        assert!(serde_json::from_str::<EnvironmentBody>("not json").is_err());
    }
}
