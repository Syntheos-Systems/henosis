//! The skills-backend seam and its feature-gated HTTP implementation.
//!
//! The [`SkillsBridge`] trait lets in-process consumers supply a skills backend. The optional
//! [`HttpSkillsBridge`] implements the same contract through the Kleos HTTP skills API.

use serde_json::Value;

/// The skills backend the forge tools talk to. Methods mirror the Kleos skills API surface;
/// errors are plain strings because every caller folds them into the CLI's `Output` envelope.
pub trait SkillsBridge: Send + Sync {
    /// Search for skills matching `query`, optionally capped at `limit` results.
    fn search_skills(&self, query: &str, limit: Option<usize>) -> Result<Value, String>;

    /// Submit a new skill description, optionally tagged with the originating `agent`.
    fn capture_skill(&self, description: &str, agent: Option<&str>) -> Result<Value, String>;

    /// Record one execution attempt for `skill_id`.
    fn record_execution(
        &self,
        skill_id: i64,
        success: bool,
        duration_ms: Option<f64>,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<Value, String>;

    /// Request a corrected version of `skill_id`, optionally guided by a free-text `hint`.
    fn fix_skill(&self, skill_id: i64, hint: Option<&str>) -> Result<Value, String>;

    /// Derive a new skill from `parent_ids` following the natural-language `direction`.
    fn derive_skill(
        &self,
        parent_ids: &[i64],
        direction: &str,
        agent: Option<&str>,
    ) -> Result<Value, String>;

    /// Retrieve the derivation lineage for `skill_id`.
    fn get_lineage(&self, skill_id: i64) -> Result<Value, String>;
}

/// Blocking HTTP [`SkillsBridge`] against the Kleos skills API. Authentication uses the
/// optional `KLEOS_API_KEY` environment variable.
#[cfg(feature = "http-bridge")]
pub mod http {
    use super::SkillsBridge;
    use serde_json::{json, Value};
    use std::env;
    use std::time::Duration;

    /// Blocking reqwest client over `KLEOS_URL` (default `http://localhost:4200`).
    pub struct HttpSkillsBridge {
        /// The shared connection pool.
        http: reqwest::blocking::Client,
        /// Base URL, no trailing slash.
        base_url: String,
        /// Optional bearer token from `KLEOS_API_KEY`.
        api_key: Option<String>,
    }

    /// Constructs authenticated requests and handles JSON responses for the skills API.
    impl HttpSkillsBridge {
        /// Build from the environment. `Err` only when the HTTP client itself cannot be
        /// constructed; an unset URL falls back to `http://localhost:4200`.
        pub fn from_env() -> Result<Self, String> {
            let base_url =
                env::var("KLEOS_URL").unwrap_or_else(|_| "http://localhost:4200".to_string());
            let base_url = base_url.trim_end_matches('/').to_string();
            let api_key = env::var("KLEOS_API_KEY").ok().filter(|k| !k.is_empty());
            let http = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| e.to_string())?;
            Ok(Self {
                http,
                base_url,
                api_key,
            })
        }

        /// Attach the bearer token when configured.
        fn apply_auth(
            &self,
            req: reqwest::blocking::RequestBuilder,
        ) -> reqwest::blocking::RequestBuilder {
            match &self.api_key {
                Some(key) => req.bearer_auth(key),
                None => req,
            }
        }

        /// GET `path` (relative to the base URL) and parse the JSON body.
        fn get(&self, path: &str) -> Result<Value, String> {
            let url = format!("{}{}", self.base_url, path);
            let resp = self
                .apply_auth(self.http.get(&url))
                .send()
                .map_err(|e| format!("{url}: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("{url}: HTTP {}", resp.status()));
            }
            resp.json::<Value>().map_err(|e| e.to_string())
        }

        /// POST a JSON `body` to `path` and parse the JSON response.
        fn post(&self, path: &str, body: Value) -> Result<Value, String> {
            let url = format!("{}{}", self.base_url, path);
            let resp = self
                .apply_auth(self.http.post(&url).json(&body))
                .send()
                .map_err(|e| format!("{url}: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                return Err(format!("{url}: HTTP {status} -- {text}"));
            }
            resp.json::<Value>().map_err(|e| e.to_string())
        }
    }

    /// Implements every skills operation through the blocking HTTP transport.
    impl SkillsBridge for HttpSkillsBridge {
        /// Search the remote skills catalog.
        fn search_skills(&self, query: &str, limit: Option<usize>) -> Result<Value, String> {
            let mut body = json!({ "query": query });
            if let Some(l) = limit {
                body["limit"] = json!(l);
            }
            self.post("/skills/search", body)
        }

        /// Capture a new skill from a natural-language description.
        fn capture_skill(&self, description: &str, agent: Option<&str>) -> Result<Value, String> {
            let mut body = json!({ "description": description });
            if let Some(a) = agent {
                body["agent"] = json!(a);
            }
            self.post("/skills/capture", body)
        }

        /// Record whether a skill execution succeeded and how long it took.
        fn record_execution(
            &self,
            skill_id: i64,
            success: bool,
            duration_ms: Option<f64>,
            error_type: Option<&str>,
            error_message: Option<&str>,
        ) -> Result<Value, String> {
            let mut body = json!({ "success": success });
            if let Some(d) = duration_ms {
                body["duration_ms"] = json!(d);
            }
            if let Some(et) = error_type {
                body["error_type"] = json!(et);
            }
            if let Some(em) = error_message {
                body["error_message"] = json!(em);
            }
            self.post(&format!("/skills/{skill_id}/execute"), body)
        }

        /// Request a repair pass for an existing skill.
        fn fix_skill(&self, skill_id: i64, hint: Option<&str>) -> Result<Value, String> {
            let mut body = json!({});
            if let Some(h) = hint {
                body["hint"] = json!(h);
            }
            self.post(&format!("/skills/{skill_id}/fix"), body)
        }

        /// Derive a new skill from one or more parent skills.
        fn derive_skill(
            &self,
            parent_ids: &[i64],
            direction: &str,
            agent: Option<&str>,
        ) -> Result<Value, String> {
            let mut body = json!({ "parent_ids": parent_ids, "direction": direction });
            if let Some(a) = agent {
                body["agent"] = json!(a);
            }
            self.post("/skills/derive", body)
        }

        /// Fetch the ancestry and descendants of a skill.
        fn get_lineage(&self, skill_id: i64) -> Result<Value, String> {
            self.get(&format!("/skills/{skill_id}/lineage"))
        }
    }
}

#[cfg(feature = "http-bridge")]
pub use http::HttpSkillsBridge;
