use serde::Deserialize;
use serde_json::Value;

use crate::{Client, Error, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub directory: String,
    pub canonical: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocationInfo {
    pub directory: String,
    pub project: ProjectRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Cursor {
    pub previous: Option<String>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    #[serde(default)]
    pub cursor: Cursor,
}

#[derive(Deserialize)]
struct CreateEnvelope {
    data: CreatedSession,
}

#[derive(Deserialize)]
struct CreatedSession {
    id: String,
}

impl Client {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .header("x-opencode-directory", &self.directory)
    }

    pub async fn location(&self) -> Result<LocationInfo> {
        let resp = self
            .request(reqwest::Method::GET, "/api/location")
            .query(&[("location[directory]", self.directory.as_str())])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn create_session(&self) -> Result<String> {
        let resp = self
            .request(reqwest::Method::POST, "/api/session")
            .json(&serde_json::json!({
                "location": { "directory": self.directory }
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json::<CreateEnvelope>().await?.data.id)
    }

    /// First page: `order=desc&limit=100`. Later pages MUST NOT send `order`
    /// (the server rejects cursor+order; the order lives inside the cursor).
    pub async fn messages(
        &self,
        session: &str,
        cursor: Option<&str>,
    ) -> Result<Page<ox_schema::message::Message>> {
        let mut req = self.request(
            reqwest::Method::GET,
            &format!("/api/session/{session}/message"),
        );
        req = match cursor {
            Some(cursor) => req.query(&[("cursor", cursor)]),
            None => req.query(&[("order", "desc"), ("limit", "100")]),
        };
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn prompt(&self, session: &str, text: &str) -> Result<Value> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/prompt"),
            )
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn interrupt(&self, session: &str) -> Result<()> {
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/api/session/{session}/interrupt"),
            )
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        Ok(())
    }
}
