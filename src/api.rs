use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use reqwest::{
    Client, Url,
    cookie::{CookieStore, Jar},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub const BASE: &str = "https://animecix.tv";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Video {
    pub id: Option<u64>,
    pub name: Option<String>,
    pub episode_num: Option<f64>,
    pub season_num: Option<u32>,
    pub url: String,
}

pub fn title_name(title: &Value) -> String {
    ["name", "name_english", "name_romanji", "title", "titleName"]
        .iter()
        .find_map(|key| title[*key].as_str())
        .unwrap_or("Unknown title")
        .to_owned()
}

pub fn title_id(title: &Value) -> Result<String> {
    ["id", "_id", "title_id"]
        .iter()
        .find_map(|key| {
            let v = &title[*key];
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .context("Search result has no title ID")
}

pub struct Api {
    client: Client,
    jar: Arc<Jar>,
}

impl Api {
    pub async fn new() -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()?;
        let api = Self { client, jar };
        api.bootstrap().await?;
        Ok(api)
    }

    async fn bootstrap(&self) -> Result<()> {
        for attempt in 1..=3 {
            let response = self
                .client
                .get(format!("{BASE}/secure/bootstrap-data"))
                .query(&[("original_url", format!("{BASE}/"))])
                .send()
                .await?;
            if response.status().is_success() {
                return Ok(());
            }
            if !transient(response.status().as_u16()) || attempt == 3 {
                bail!("Animecix bootstrap returned HTTP {}", response.status());
            }
            tokio::time::sleep(Duration::from_millis(750 * attempt)).await;
        }
        unreachable!()
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut url = Url::parse(BASE)?.join(path)?;
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (*k, v)));
        for attempt in 1..=3 {
            let mut request = self
                .client
                .get(url.clone())
                .header("Origin", BASE)
                .header("Accept", "application/json")
                .header("X-E-H", signed_query(url.query().unwrap_or(""))?);
            if let Some(cookies) = self.jar.cookies(&Url::parse(BASE)?) {
                if let Some(token) = cookies
                    .to_str()?
                    .split("; ")
                    .find_map(|c| c.strip_prefix("XSRF-TOKEN="))
                {
                    request = request.header("X-XSRF-TOKEN", token);
                }
            }
            let response = request.send().await?;
            let status = response.status();
            if status.is_success() {
                return response
                    .json()
                    .await
                    .context("Animecix returned invalid JSON");
            }
            if matches!(status.as_u16(), 401 | 419) && attempt < 3 {
                self.bootstrap().await?;
            } else if !transient(status.as_u16()) || attempt == 3 {
                bail!(
                    "Animecix returned HTTP {status}; the site may be unavailable or its API may have changed"
                );
            }
            tokio::time::sleep(Duration::from_millis(750 * attempt)).await;
        }
        unreachable!()
    }

    pub async fn search(&self, term: &str) -> Result<Vec<Value>> {
        let mut path = Url::parse(&format!("{BASE}/secure/search/"))?;
        path.path_segments_mut()
            .expect("base URL supports paths")
            .pop_if_empty()
            .push(term);
        let data = self
            .get(
                path.path(),
                &[
                    ("type", "undefined".into()),
                    ("limit", "30".into()),
                    ("provider", "null".into()),
                ],
            )
            .await?;
        data["results"]
            .as_array()
            .cloned()
            .context("Animecix search response has no results array")
    }

    pub async fn title(&self, id: &str, name: &str) -> Result<Value> {
        let data = self
            .get(
                &format!("/secure/titles/{id}"),
                &[("titleId", id.into()), ("titleName", name.into())],
            )
            .await?;
        if !data["title"].is_object() {
            bail!("Animecix returned no title for ID {id}");
        }
        Ok(data["title"].clone())
    }

    pub async fn videos(&self, id: &str, season: u32) -> Result<Vec<Video>> {
        let data = self
            .get(
                &format!("/secure/titles/{id}"),
                &[("seasonNumber", season.to_string())],
            )
            .await?;
        serde_json::from_value(data["title"]["videos"].clone())
            .context("Animecix returned no episode videos")
    }
}

fn transient(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn signed_query(query: &str) -> Result<String> {
    // Same request signature as the existing Animecixing client (public website protocol).
    let key = concat!("i4C7R2", "fXGocdYg", "FLzCbDlsJ", "jukf8G58b");
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid signing key"))?;
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let encrypted = cipher
        .encrypt(
            Nonce::from_slice(&iv),
            format!("{{version}}{query}").as_bytes(),
        )
        .map_err(|_| anyhow::anyhow!("Request signing failed"))?;
    Ok(format!(
        "{}.{}",
        STANDARD.encode(encrypted),
        STANDARD.encode(iv)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_roundtrip() {
        let signature = signed_query("episode=1&titleName=One+Piece").unwrap();
        let (data, iv) = signature.split_once('.').unwrap();
        let iv = STANDARD.decode(iv).unwrap();
        let cipher = Aes256Gcm::new_from_slice(b"i4C7R2fXGocdYgFLzCbDlsJjukf8G58b").unwrap();
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&iv),
                STANDARD.decode(data).unwrap().as_ref(),
            )
            .unwrap();
        assert_eq!(plain, b"{version}episode=1&titleName=One+Piece");
    }
}
