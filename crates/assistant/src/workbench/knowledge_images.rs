use crate::{App, Shared, auth, error};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use sqlx::{Executor, Postgres};
use uuid::Uuid;

pub(crate) const LIMIT: usize = 6 * 1024 * 1024;

pub(super) struct Image {
    id: Uuid,
    mime_type: &'static str,
    bytes: Vec<u8>,
}

impl Image {
    pub(super) fn new(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.is_empty() || bytes.len() > LIMIT {
            return Err("图片为空或超过 6 MB".into());
        }
        let mime_type = match image::guess_format(&bytes) {
            Ok(image::ImageFormat::Png) => "image/png",
            Ok(image::ImageFormat::Jpeg) => "image/jpeg",
            Ok(image::ImageFormat::WebP) => "image/webp",
            _ => return Err("请选择 PNG、JPEG 或 WebP 图片".into()),
        };
        Ok(Self {
            id: Uuid::now_v7(),
            mime_type,
            bytes,
        })
    }

    pub(super) fn markdown(&self, alt: &str) -> String {
        let alt = alt.replace(['[', ']', '\r', '\n'], " ");
        format!(
            "\n\n![{}](/api/workbench/knowledge-image/{})",
            alt.trim(),
            self.id
        )
    }

    pub(super) async fn insert<'a>(
        self,
        db: impl Executor<'a, Database = Postgres>,
    ) -> Result<Uuid, String> {
        sqlx::query("INSERT INTO knowledge_images(id,mime_type,image) VALUES($1,$2,$3)")
            .bind(self.id)
            .bind(self.mime_type)
            .bind(self.bytes)
            .execute(db)
            .await
            .map_err(|error| error.to_string())?;
        Ok(self.id)
    }
}

pub(super) async fn observation(app: &App, run: Uuid, observation: &str) -> Result<Image, String> {
    let result: Value = sqlx::query_scalar(
        "SELECT e.result FROM app_learning_observations o
         JOIN harness_events e ON e.id=o.event_id AND e.run_id=o.run_id
         JOIN runs r ON r.id=o.run_id
         WHERE o.run_id=$1 AND o.observation_id=$2 AND e.status='completed'
           AND e.created_at>=COALESCE(r.input_at,r.created_at)",
    )
    .bind(run)
    .bind(observation)
    .fetch_optional(&app.db)
    .await
    .map_err(|error| error.to_string())?
    .ok_or("请引用本轮真实设备观察 ID")?;
    let encoded = result
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|part| {
            part["image_url"]
                .as_str()?
                .strip_prefix("data:image/png;base64,")
        })
        .ok_or("该观察没有可用截图")?;
    if encoded.len() > LIMIT.div_ceil(3) * 4 {
        return Err("截图超过 6 MB".into());
    }
    Image::new(STANDARD.decode(encoded).map_err(|_| "截图格式无效")?)
}

pub(crate) async fn read(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if let Err(rejection) = auth(&app, &headers) {
        return rejection.into();
    }
    match sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT mime_type,image FROM knowledge_images WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&app.db)
    .await
    {
        Ok(Some((mime, image))) => (
            [
                ("content-type", mime.as_str()),
                ("cache-control", "private, max-age=3600"),
                ("x-content-type-options", "nosniff"),
            ],
            image,
        )
            .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "not_found", "图片不存在"),
        Err(error) => crate::db_error(error),
    }
}
