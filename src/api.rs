use crate::network::Client;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use reqwest::{
    Url,
    cookie::{CookieStore, Jar},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, sync::Arc, time::Duration};

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

#[derive(Clone, Debug)]
pub struct TmdbSeries {
    pub name: String,
    pub first_air_date: Option<String>,
    pub runtime: Option<u64>,
    pub vote_average: Option<f64>,
    pub overview: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TmdbEpisode {
    pub episode_number: f64,
    pub name: String,
    pub air_date: Option<String>,
    pub runtime: Option<u64>,
    pub vote_average: Option<f64>,
    pub overview: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TmdbSeason {
    pub name: String,
    pub air_date: Option<String>,
    pub overview: Option<String>,
    pub episodes: Vec<TmdbEpisode>,
}

pub fn title_name(title: &Value) -> String {
    ["name", "name_english", "name_romanji", "title", "titleName"]
        .iter()
        .find_map(|key| title[*key].as_str())
        .map(crate::terminal::text)
        .unwrap_or_else(|| "Unknown title".to_owned())
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
        let client = Client::with_cookies(jar.clone())?;
        let api = Self { client, jar };
        api.bootstrap().await?;
        Ok(api)
    }

    async fn bootstrap(&self) -> Result<()> {
        for attempt in 1..=3 {
            let response = self
                .client
                .get(format!("{BASE}/secure/bootstrap-data"))?
                .timeout(Duration::from_secs(30))
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
                .get(url.as_str())?
                .timeout(Duration::from_secs(30))
                .header("Origin", BASE)
                .header("Accept", "application/json")
                .header("X-E-H", signed_query(url.query().unwrap_or(""))?);
            if let Some(cookies) = self.jar.cookies(&Url::parse(BASE)?)
                && let Some(token) = cookies
                    .to_str()?
                    .split("; ")
                    .find_map(|c| c.strip_prefix("XSRF-TOKEN="))
            {
                request = request.header("X-XSRF-TOKEN", token);
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

    pub async fn tmdb_series(&self, title: &Value) -> Result<Option<TmdbSeries>> {
        let Some(credential) = tmdb_credential() else {
            return Ok(None);
        };
        let Some(series_id) = tmdb_id(title) else {
            return Ok(None);
        };

        let is_movie = tmdb_is_movie(title);
        let data = self
            .tmdb_get(
                &format!("/{}/{series_id}", if is_movie { "movie" } else { "tv" }),
                &credential,
            )
            .await?;
        Ok(Some(TmdbSeries {
            name: data[if is_movie { "title" } else { "name" }]
                .as_str()
                .map(crate::terminal::text)
                .unwrap_or_else(|| "Unknown title".to_owned()),
            first_air_date: string_field(
                &data,
                if is_movie {
                    "release_date"
                } else {
                    "first_air_date"
                },
            ),
            runtime: data["runtime"]
                .as_u64()
                .or_else(|| data["episode_run_time"][0].as_u64()),
            vote_average: data["vote_average"].as_f64(),
            overview: string_field(&data, "overview"),
        }))
    }

    pub async fn tmdb_season(&self, title: &Value, season: u32) -> Result<Option<TmdbSeason>> {
        let Some(credential) = tmdb_credential() else {
            return Ok(None);
        };
        let Some(series_id) = tmdb_id(title) else {
            return Ok(None);
        };
        if tmdb_is_movie(title) {
            return Ok(None);
        }
        let data = self
            .tmdb_get(&format!("/tv/{series_id}/season/{season}"), &credential)
            .await?;
        let episodes = data["episodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|episode| {
                Some(TmdbEpisode {
                    episode_number: episode["episode_number"].as_f64()?,
                    name: crate::terminal::text(episode["name"].as_str()?.trim()),
                    air_date: string_field(episode, "air_date"),
                    runtime: episode["runtime"].as_u64(),
                    vote_average: episode["vote_average"].as_f64(),
                    overview: string_field(episode, "overview"),
                })
            })
            .filter(|episode| !episode.name.is_empty())
            .collect();
        Ok(Some(TmdbSeason {
            name: crate::terminal::text(data["name"].as_str().unwrap_or("Season")),
            air_date: string_field(&data, "air_date"),
            overview: string_field(&data, "overview"),
            episodes,
        }))
    }

    async fn tmdb_get(&self, path: &str, credential: &TmdbCredential) -> Result<Value> {
        let mut request = self
            .client
            .get(format!("https://api.themoviedb.org/3{path}"))?
            .timeout(Duration::from_secs(30))
            .query(&[("language", "en-US")])
            .header("Accept", "application/json");
        request = match credential {
            TmdbCredential::ApiKey(key) => request.query(&[("api_key", key)]),
            TmdbCredential::AccessToken(token) => request.bearer_auth(token),
        };
        request
            .send()
            .await?
            .error_for_status()
            .context("TMDB request failed")?
            .json()
            .await
            .context("TMDB returned invalid data")
    }
}

enum TmdbCredential {
    ApiKey(String),
    AccessToken(String),
}

fn tmdb_credential() -> Option<TmdbCredential> {
    env::var("TMDB_ACCESS_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(TmdbCredential::AccessToken)
        .or_else(|| {
            env::var("TMDB_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(TmdbCredential::ApiKey)
        })
}

fn tmdb_id(title: &Value) -> Option<String> {
    ["tmdb_id", "tmdbId", "tmdb"].iter().find_map(|key| {
        title[*key]
            .as_str()
            .map(str::to_owned)
            .or_else(|| title[*key].as_u64().map(|id| id.to_string()))
    })
}

fn tmdb_is_movie(title: &Value) -> bool {
    ["type", "title_type", "format", "kind"]
        .iter()
        .filter_map(|key| title[*key].as_str())
        .any(|value| value.eq_ignore_ascii_case("movie"))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::terminal::text)
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
