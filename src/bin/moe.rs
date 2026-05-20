use anyhow::{anyhow, bail};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Json, Router, response::Html, routing::get};
use clap::Parser;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use w_kiva_moe::AppOpts;
use w_kiva_moe::video_gw::VideoGateway;

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
  fn into_response(self) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
  }
}

impl<E> From<E> for AppError
where
  E: Into<anyhow::Error>,
{
  fn from(err: E) -> Self {
    Self(err.into())
  }
}

#[derive(Deserialize)]
struct BvResolverParam {
  pub bvid: String,
  pub p: Option<usize>,
}

const DEFAULT_QUALITY: u64 = 116;
const RESOLVE_FAILURE_MESSAGE: &str = "小袜子无法解析喵";
const QUALITY_CACHE_CAPACITY: u64 = 1024;
const RESOLVE_CACHE_CAPACITY: u64 = 1024;
const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResolveCacheKey {
  bvid: String,
  p: usize,
}

#[derive(Clone)]
struct ResolvedPlayurl {
  url: String,
  quality: u64,
}

#[derive(Debug)]
struct UncachedPlayurl {
  url: String,
  reason: String,
}

impl std::fmt::Display for UncachedPlayurl {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.reason)
  }
}

impl Error for UncachedPlayurl {}

#[derive(Serialize)]
struct DebugBvResolverEntry {
  p: usize,
  url: String,
  quality: u64,
}

#[derive(Clone)]
struct BvResolver {
  client: reqwest::Client,
  quality_cache: Cache<String, u64>,
  resolve_cache: Cache<ResolveCacheKey, ResolvedPlayurl>,
}

impl BvResolver {
  fn new() -> anyhow::Result<Arc<Self>> {
    let client = reqwest::Client::builder()
      .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36")
      .build()
      .map_err(|e| anyhow!(e))?;

    Ok(Arc::new(Self {
      client,
      quality_cache: Cache::builder()
        .max_capacity(QUALITY_CACHE_CAPACITY)
        .build(),
      resolve_cache: Cache::builder()
        .max_capacity(RESOLVE_CACHE_CAPACITY)
        .time_to_live(RESOLVE_CACHE_TTL)
        .build(),
    }))
  }

  async fn resolve(&self, bv: String, p: usize) -> anyhow::Result<Response> {
    let cache_key = ResolveCacheKey { bvid: bv, p };
    let init_key = cache_key.clone();

    let resolved = match self
      .resolve_cache
      .try_get_with(
        cache_key,
        async move { self.resolve_uncached(&init_key).await },
      )
      .await
    {
      Ok(resolved) => resolved,
      Err(e) => {
        if let Some(uncached) = e.downcast_ref::<UncachedPlayurl>() {
          log::warn!(
            "All playurl HEAD checks failed; returning uncached first playurl: {}",
            uncached.reason
          );
          return Ok(Redirect::temporary(uncached.url.as_str()).into_response());
        }

        return Ok(format!("{}: {}", RESOLVE_FAILURE_MESSAGE, e.as_ref()).into_response());
      }
    };

    Ok(Redirect::temporary(resolved.url.as_str()).into_response())
  }

  async fn resolve_uncached(&self, cache_key: &ResolveCacheKey) -> anyhow::Result<ResolvedPlayurl> {
    let cid = match with_bilibili_headers(self.client.get(format!(
      "https://api.bilibili.com/x/player/pagelist?bvid={}",
      cache_key.bvid
    )))
    .send()
    .await
    {
      Ok(x) => x,
      Err(e) => bail!("Failed to get cid: {}", e),
    };
    let strings = match cid.text().await {
      Ok(x) => x,
      Err(e) => bail!("Failed to parse cid response as UTF8: {}", e),
    };
    let cid = match serde_json::from_str::<serde_json::Value>(&strings) {
      Ok(x) => x,
      Err(_) => bail!("Failed to parse cid response: {}", &strings),
    };

    let page_index = cache_key
      .p
      .checked_sub(1)
      .ok_or_else(|| anyhow!("Failed to get cid from response: {}", cid))?;
    let cid = match cid
      .as_object()
      .and_then(|x| x.get("data"))
      .and_then(|x| x.as_array())
      .and_then(|x| x.get(page_index))
      .and_then(|x| x.as_object())
      .and_then(|x| x.get("cid"))
      .and_then(|x| x.as_number())
    {
      Some(x) => x.to_string(),
      None => bail!("Failed to get cid from response: {}", cid),
    };

    // https://www.bilibili.com/opus/400555526268551002
    // quality 120 = 4K
    // quality 116 = 1080P60
    // quality 112 = 1080P+
    // quality 80 = 1080P
    // quality 74 = 720P60
    // quality 64 = 720P
    // quality 32 = 480P
    // quality 16 = 360P
    self.resolve_playurl(&cache_key.bvid, &cid).await
  }

  async fn resolve_playurl(&self, bv: &str, cid: &str) -> anyhow::Result<ResolvedPlayurl> {
    let preferred_quality = self.quality_cache.get(bv).await.unwrap_or(DEFAULT_QUALITY);
    let mut tried_qualities = Vec::new();
    let mut errors = Vec::new();

    let (first_quality, first_playurl) =
      match request_playurl(&self.client, bv, cid, preferred_quality).await {
        Ok(x) => {
          tried_qualities.push(preferred_quality);
          (preferred_quality, x)
        }
        Err(e) if preferred_quality != DEFAULT_QUALITY => {
          errors.push(format!(
            "quality {} playurl request failed: {}",
            preferred_quality, e
          ));
          tried_qualities.push(preferred_quality);

          match request_playurl(&self.client, bv, cid, DEFAULT_QUALITY).await {
            Ok(x) => {
              tried_qualities.push(DEFAULT_QUALITY);
              (DEFAULT_QUALITY, x)
            }
            Err(e) => {
              errors.push(format!(
                "quality {} playurl request failed: {}",
                DEFAULT_QUALITY, e
              ));
              bail!("{}", errors.join("; "));
            }
          }
        }
        Err(e) => {
          bail!(
            "quality {} playurl request failed: {}",
            preferred_quality,
            e
          );
        }
      };

    let mut should_sleep_before_next_head = false;
    let mut uncached_first_url = None;

    if let Some(url) = extract_durl_url(&first_playurl) {
      uncached_first_url = Some(url.clone());
      match validate_video_url(&self.client, &url).await {
        Ok(()) => {
          self
            .quality_cache
            .insert(bv.to_string(), first_quality)
            .await;
          return Ok(ResolvedPlayurl {
            url,
            quality: first_quality,
          });
        }
        Err(e) => {
          errors.push(format!("quality {} HEAD failed: {}", first_quality, e));
        }
      }
      should_sleep_before_next_head = true;
    } else {
      errors.push(format!(
        "quality {} response missing .data.durl[0].url",
        first_quality
      ));
    }

    let mut retry_qualities = extract_accept_quality(&first_playurl);
    if retry_qualities.is_empty() {
      errors.push(format!(
        "quality {} response missing .data.accept_quality",
        first_quality
      ));
    }

    if first_quality != DEFAULT_QUALITY && !retry_qualities.contains(&DEFAULT_QUALITY) {
      retry_qualities.insert(0, DEFAULT_QUALITY);
    }

    for quality in retry_qualities {
      if tried_qualities.contains(&quality) {
        continue;
      }
      tried_qualities.push(quality);

      let playurl = match request_playurl(&self.client, bv, cid, quality).await {
        Ok(x) => x,
        Err(e) => {
          errors.push(format!("quality {} playurl request failed: {}", quality, e));
          continue;
        }
      };
      let Some(url) = extract_durl_url(&playurl) else {
        errors.push(format!(
          "quality {} response missing .data.durl[0].url",
          quality
        ));
        continue;
      };

      if should_sleep_before_next_head {
        tokio::time::sleep(Duration::from_secs(1)).await;
      }

      match validate_video_url(&self.client, &url).await {
        Ok(()) => {
          self.quality_cache.insert(bv.to_string(), quality).await;
          return Ok(ResolvedPlayurl { url, quality });
        }
        Err(e) => {
          errors.push(format!("quality {} HEAD failed: {}", quality, e));
        }
      }
      should_sleep_before_next_head = true;
    }

    if errors.is_empty() {
      bail!("no quality candidates available");
    }

    if let Some(url) = uncached_first_url {
      return Err(
        UncachedPlayurl {
          url,
          reason: errors.join("; "),
        }
        .into(),
      );
    }

    bail!("{}", errors.join("; "))
  }

  async fn debug_entries(&self) -> BTreeMap<String, Vec<DebugBvResolverEntry>> {
    self.resolve_cache.run_pending_tasks().await;

    let mut entries = BTreeMap::<String, Vec<DebugBvResolverEntry>>::new();

    for (key, resolved) in self.resolve_cache.iter() {
      entries
        .entry(key.bvid.clone())
        .or_default()
        .push(DebugBvResolverEntry {
          p: key.p,
          url: resolved.url.clone(),
          quality: resolved.quality,
        });
    }

    for value in entries.values_mut() {
      value.sort_by_key(|entry| entry.p);
    }
    entries
  }

  async fn clear_caches(&self) {
    self.quality_cache.invalidate_all();
    self.resolve_cache.invalidate_all();
    self.quality_cache.run_pending_tasks().await;
    self.resolve_cache.run_pending_tasks().await;
  }
}

async fn debug_bv_resolvers_handler(
  State(resolver): State<Arc<BvResolver>>,
) -> Json<BTreeMap<String, Vec<DebugBvResolverEntry>>> {
  Json(resolver.debug_entries().await)
}

async fn debug_bv_resolvers_clear_handler(State(resolver): State<Arc<BvResolver>>) -> StatusCode {
  resolver.clear_caches().await;
  StatusCode::OK
}

fn with_bilibili_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
  request
    .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7")
    .header("Accept-Language", "zh-CN,zh;q=0.9")
    .header("Cache-Control", "no-cache")
    .header("DNT", "1")
    .header("Pragma", "no-cache")
    .header("Priority", "u=0, i")
    .header("Sec-Fetch-Dest", "document")
    .header("Sec-Fetch-Mode", "navigate")
    .header("Sec-Fetch-Site", "none")
    .header("Sec-Fetch-User", "?1")
    .header("Upgrade-Insecure-Requests", "1")
}

async fn request_playurl(
  client: &reqwest::Client,
  bv: &str,
  cid: &str,
  quality: u64,
) -> anyhow::Result<serde_json::Value> {
  let playurl = match with_bilibili_headers(client.get(format!(
    "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn={}&type=&otype=json&platform=html5&high_quality=1",
    bv, cid, quality,
  )))
  .send()
  .await
  {
    Ok(x) => x,
    Err(e) => bail!("Failed to get playurl: {}", e),
  };
  let strings = match playurl.text().await {
    Ok(x) => x,
    Err(e) => bail!("Failed to parse playurl response as UTF8: {}", e),
  };
  match serde_json::from_str::<serde_json::Value>(&strings) {
    Ok(x) => Ok(x),
    Err(_) => bail!("Failed to parse playurl response: {}", &strings),
  }
}

fn extract_durl_url(json: &serde_json::Value) -> Option<String> {
  json["data"]["durl"][0]["url"]
    .as_str()
    .map(ToString::to_string)
}

fn extract_accept_quality(json: &serde_json::Value) -> Vec<u64> {
  json["data"]["accept_quality"]
    .as_array()
    .map(|qualities| {
      qualities
        .iter()
        .filter_map(|quality| {
          quality
            .as_u64()
            .or_else(|| quality.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
        .collect()
    })
    .unwrap_or_default()
}

async fn validate_video_url(client: &reqwest::Client, url: &str) -> anyhow::Result<()> {
  match client
    .head(url)
    .header("Accept", "*/*")
    .header("Referer", "https://www.bilibili.com/")
    .send()
    .await
  {
    Ok(response) => {
      let status = response.status();
      if status.is_success() {
        Ok(())
      } else {
        bail!("HEAD status {}", status)
      }
    }
    Err(e) => bail!("HEAD request failed: {}", e),
  }
}

#[tokio::main]
async fn main() {
  match dotenvy::dotenv() {
    Err(e) => log::warn!("dotenv(): failed to load .env file: {}", e),
    _ => {}
  }

  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    .filter(Some("warp::server"), log::LevelFilter::Off)
    .init();

  let opts = AppOpts::parse();
  let bv_resolver = BvResolver::new().unwrap();
  let video_gw = VideoGateway::new(10000, Duration::from_secs(1800));

  let bv_router = Router::new()
    .route("/debug/bvresolvers", get(debug_bv_resolvers_handler))
    .route(
      "/debug/bvresolvers-clear",
      get(debug_bv_resolvers_clear_handler),
    )
    .route(
      "/{bvid}",
      get(
        async move |State(resolver): State<Arc<BvResolver>>,
                    params: Path<BvResolverParam>|
                    -> Result<Response, AppError> {
          Ok(resolver.resolve(params.bvid.clone(), 1).await?)
        },
      ),
    )
    .route(
      "/{bvid}/{p}",
      get(
        async move |State(resolver): State<Arc<BvResolver>>,
                    params: Path<BvResolverParam>|
                    -> Result<Response, AppError> {
          let p = params.p.unwrap_or(1usize);
          Ok(resolver.resolve(params.bvid.clone(), p).await?)
        },
      ),
    )
    .with_state(bv_resolver);

  // build our application with a route
  let app = Router::new()
    .route("/", get(async move || Html("你好喵~这里是小袜子！")))
    .route("/health", get(async move || Html("OK")))
    .merge(bv_router)
    .nest("/video-gw", w_kiva_moe::video_gw::router(video_gw));

  // run it
  let listener = tokio::net::TcpListener::bind(opts.listen).await.unwrap();
  log::info!("Listening on {}", listener.local_addr().unwrap());
  axum::serve(listener, app).await.unwrap();
}
