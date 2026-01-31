use axum::{
  Json,
  Router,
  extract::{Path, State},
  http::StatusCode,
  routing::{get, post},
};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::{
  collections::VecDeque,
  sync::Arc,
  time::Duration,
};
use axum::response::Redirect;
use moka::notification::RemovalCause;
use tokio::sync::Mutex;

pub type SlotId = u32;

#[derive(Clone)]
pub struct VideoGateway {
  cache: Cache<SlotId, String>,
  free_slots: Arc<Mutex<VecDeque<SlotId>>>,
}

#[derive(Debug, Serialize)]
pub struct RedirectEntry {
  pub slot_id: SlotId,
  pub url: String,
}

impl VideoGateway {
  pub fn new(max_slots: SlotId, ttl: Duration) -> Arc<Self> {
    let free_slots = Arc::new(Mutex::new((1..=max_slots).collect::<VecDeque<_>>()));
    let free_slots_for_eviction = free_slots.clone();

    let cache = Cache::builder()
      .max_capacity(max_slots as u64)
      .time_to_live(ttl)
      .eviction_listener(move |slot_id, _value, cause| {
        match cause {
          RemovalCause::Replaced => return,
          RemovalCause::Expired => {}
          RemovalCause::Explicit => {}
          RemovalCause::Size => {}
        }
        let free_slots_for_eviction = free_slots_for_eviction.clone();
        tokio::spawn(async move {
          let mut guard = free_slots_for_eviction.lock().await;
          guard.push_back(*slot_id);
          log::info!("Slot {} evicted", slot_id);
        });
      })
      .build();

    let sv = Arc::new(Self {
      cache,
      free_slots,
    });

    {
      let sv = sv.clone();
      tokio::spawn(async move {
        loop {
          sv.tick().await;
          tokio::time::sleep(Duration::from_secs(3)).await;
        }
      });
    }

    sv
  }

  pub async fn tick(&self) {
    self.cache.run_pending_tasks().await;
  }

  pub async fn create_redirect(&self, url: String) -> Option<SlotId> {
    let mut guard = self.free_slots.lock().await;
    let slot_id = guard.pop_front()?;
    drop(guard);

    self.cache.insert(slot_id, url.clone()).await;
    log::info!("Slot {} created, {}", slot_id, url);
    Some(slot_id)
  }

  pub async fn touch_redirect_slot(&self, slot_id: SlotId) {
    if let Some(url) = self.cache.get(&slot_id).await {
      self.cache.insert(slot_id, url.clone()).await;
      log::info!("Slot {} touched, {}", slot_id, url);
    }
  }

  pub async fn get_redirect(&self, slot_id: SlotId) -> Option<String> {
    self.cache.get(&slot_id).await
  }

  pub async fn get_all_redirect(&self) -> Option<Vec<RedirectEntry>> {
    let map = self.cache.iter().map(|(slot_id, url)| RedirectEntry { slot_id: *slot_id, url: url.clone() }).collect();
    Some(map)
  }
}

#[derive(Debug, Deserialize)]
pub struct CreateRedirectRequest {
  pub url: String,
}

#[derive(Debug, Serialize)]
pub struct CreateRedirectResponse {
  pub slot_id: SlotId,
}

#[derive(Debug, Serialize)]
pub struct GetRedirectResponse {
  pub url: String,
}

#[derive(Debug, Serialize)]
pub struct GetAllRedirectResponse {
  pub map: Vec<RedirectEntry>,
}

#[derive(Debug, Deserialize)]
pub struct BatchTouchRequest {
  pub slot_ids: Vec<SlotId>,
}

pub async fn create_redirect_handler(
  State(gateway): State<Arc<VideoGateway>>,
  Json(req): Json<CreateRedirectRequest>,
) -> Result<Json<CreateRedirectResponse>, StatusCode> {
  match gateway.create_redirect(req.url).await {
    Some(slot_id) => Ok(Json(CreateRedirectResponse { slot_id })),
    None => Err(StatusCode::SERVICE_UNAVAILABLE),
  }
}

pub async fn get_redirect_handler(
  State(gateway): State<Arc<VideoGateway>>,
  Path(slot_id): Path<SlotId>,
) -> Result<Redirect, StatusCode> {
  match gateway.get_redirect(slot_id).await {
    Some(url) => Ok(Redirect::temporary(&url)),
    None => Err(StatusCode::NOT_FOUND),
  }
}

pub async fn get_all_redirect_handler(
  State(gateway): State<Arc<VideoGateway>>,
) -> Result<Json<GetAllRedirectResponse>, StatusCode> {
  match gateway.get_all_redirect().await {
    Some(map) => Ok(Json(GetAllRedirectResponse { map })),
    None => Err(StatusCode::NOT_FOUND),
  }
}

pub async fn batch_touch_redirect_handler(
  State(gateway): State<Arc<VideoGateway>>,
  Json(req): Json<BatchTouchRequest>,
) -> Result<StatusCode, StatusCode> {
  for slot_id in req.slot_ids {
    gateway.touch_redirect_slot(slot_id).await;
  }
  Ok(StatusCode::OK)
}

pub fn router(gateway: Arc<VideoGateway>) -> Router {
  Router::new()
    .route("/redirect", post(create_redirect_handler))
    .route("/redirect", get(get_all_redirect_handler))
    .route("/redirect/{slot_id}", get(get_redirect_handler))
    .route("/redirect/touch", post(batch_touch_redirect_handler))
    .with_state(gateway)
}
