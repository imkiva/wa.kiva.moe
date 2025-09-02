use anyhow::{anyhow, bail};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Router, response::Html, routing::get};
use clap::Parser;
use serde::Deserialize;
use w_kiva_moe::AppOpts;

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
  fn into_response(self) -> Response {
    (
      StatusCode::INTERNAL_SERVER_ERROR,
      self.0.to_string(),
    )
      .into_response()
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

async fn bv_resolver(bv: String, p: usize) -> anyhow::Result<Redirect> {
  let client = reqwest::Client::builder()
    .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
    .build()
    .map_err(|e| anyhow!(e))?;

  let cid = match client.get(format!(
    "https://api.bilibili.com/x/player/pagelist?bvid={}",
    bv
  ))
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
    .send().await {
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

  let cid = match cid
    .as_object()
    .and_then(|x| x.get("data"))
    .and_then(|x| x.as_array())
    .and_then(|x| x.get(p - 1))
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
  let quality = 116;
  let playurl = match client.get(format!(
    "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn={}&type=&otype=json&platform=html5&high_quality=1",
    bv, cid, quality,
  ))
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
    .send().await {
    Ok(x) => x,
    Err(e) => bail!("Failed to get playurl: {}", e),
  };
  let strings = match playurl.text().await {
    Ok(x) => x,
    Err(e) => bail!("Failed to parse playurl response as UTF8: {}", e),
  };
  let json = match serde_json::from_str::<serde_json::Value>(&strings) {
    Ok(x) => x,
    Err(_) => bail!("Failed to parse playurl response: {}", &strings),
  };
  let url = json["data"]["durl"][0]["url"]
    .as_str()
    .ok_or_else(|| anyhow!("Failed to parse .data.durl[0].url"))?
    .to_string();
  Ok(Redirect::temporary(url.as_str()))
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

  // build our application with a route
  let app = Router::new()
    .route("/", get(async move || Html("Hello from W")))
    .route("/health", get(async move || Html("OK")))
    .route(
      "/{bvid}",
      get(
        async move |params: Path<BvResolverParam>| -> Result<Redirect, AppError> {
          Ok(bv_resolver(params.bvid.clone(), 1).await?)
        },
      ),
    )
    .route(
      "/{bvid}/{p}",
      get(
        async move |params: Path<BvResolverParam>| -> Result<Redirect, AppError> {
          let p = params
            .p
            .unwrap_or(1usize);
          Ok(bv_resolver(params.bvid.clone(), p).await?)
        },
      ),
    );

  // run it
  let listener = tokio::net::TcpListener::bind(opts.listen).await.unwrap();
  log::info!("Listening on {}", listener.local_addr().unwrap());
  axum::serve(listener, app).await.unwrap();
}
