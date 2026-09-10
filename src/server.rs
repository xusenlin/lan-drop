use crate::store::{MAX_TEXT_BYTES, MAX_UPLOAD_BYTES, Store, validate_name};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

type ApiResult<T> = Result<T, ApiError>;
pub struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, err.to_string())
    }
}
impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("文件操作失败：{err}"),
        )
    }
}

pub fn router(store: Store) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        // 网页的品牌图标和 favicon，与桌面端、应用图标共用同一张图。
        .route("/icon.png", get(icon))
        .route("/api/files", get(list))
        .route(
            "/api/upload",
            post(upload).layer(DefaultBodyLimit::max(
                (MAX_UPLOAD_BYTES + 1024 * 1024) as usize,
            )),
        )
        .route(
            "/api/text",
            post(save_text).layer(DefaultBodyLimit::max(MAX_TEXT_BYTES * 6 + 1024)),
        )
        .route("/api/text/{name}", get(read_text))
        .route("/download/{name}", get(download))
        .layer(middleware::from_fn(protect_browser_requests))
        .with_state(store)
}

async fn protect_browser_requests(request: Request, next: Next) -> Response {
    // Browsers must use our same-origin client. No CORS permission is granted.
    if request.uri().path() != "/"
        && request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return ApiError(
            StatusCode::FORBIDDEN,
            "请直接打开 App 显示的局域网地址".into(),
        )
        .into_response();
    }
    if request.method() == axum::http::Method::POST {
        let headers = request.headers();
        let origin_valid = match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            Some(origin) => headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|host| origin == format!("http://{host}")),
            None => true,
        };
        if !origin_valid || headers.get("x-lan-drop").is_none_or(|v| v != "1") {
            return ApiError(
                StatusCode::FORBIDDEN,
                "上传请求来源无效，请刷新页面后重试".into(),
            )
            .into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}

async fn icon() -> Response {
    // 不设 Cache-Control：protect_browser_requests 中间件对所有响应统一
    // 覆盖成 no-store，这里写了也会被替换掉。
    ([(header::CONTENT_TYPE, "image/png")], crate::ICON_PNG).into_response()
}

async fn list(State(store): State<Store>) -> ApiResult<Json<Vec<crate::store::Entry>>> {
    let items = tokio::task::spawn_blocking(move || store.list())
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(items))
}

async fn upload(
    State(store): State<Store>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let mut names = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(e.status(), e.body_text()))?
    {
        let name = field
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("请选择文件"))?
            .to_owned();
        validate_name(&name)?;
        let temp = store.temporary()?;
        let mut output = tokio::fs::File::from_std(temp.reopen()?);
        let mut size = 0_u64;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| ApiError(e.status(), e.body_text()))?
        {
            size += chunk.len() as u64;
            if size > MAX_UPLOAD_BYTES {
                return Err(ApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "单个文件不能超过 10 GiB".into(),
                ));
            }
            output.write_all(&chunk).await?;
        }
        output.flush().await?;
        output.sync_all().await?;
        drop(output);
        let store = store.clone();
        let saved = tokio::task::spawn_blocking(move || store.commit(temp, &name))
            .await
            .map_err(|e| anyhow::anyhow!(e))??;
        names.push(saved);
    }
    if names.is_empty() {
        return Err(anyhow::anyhow!("请选择至少一个文件").into());
    }
    Ok(Json(serde_json::json!({"names": names})))
}

#[derive(Deserialize)]
struct TextInput {
    text: String,
}
async fn save_text(
    State(store): State<Store>,
    Json(input): Json<TextInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = tokio::task::spawn_blocking(move || store.save_text(&input.text))
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({"name": name})))
}
async fn read_text(
    State(store): State<Store>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let text = tokio::task::spawn_blocking(move || store.read_text(&name))
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    Ok(Json(serde_json::json!({"text": text})))
}
async fn download(State(store): State<Store>, Path(name): Path<String>) -> ApiResult<Response> {
    let requested_name = name.clone();
    let file = tokio::task::spawn_blocking(move || store.open(&requested_name))
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "文件不存在或不可访问".into()))?;
    let size = file.metadata()?.len();
    let encoded = percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC);
    let mut response =
        Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file))).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"download\"; filename*=UTF-8''{encoded}"
        ))
        .map_err(|e| anyhow::anyhow!(e))?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    #[tokio::test]
    async fn browser_text_download_and_origin_protection() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(Store::new(dir.path().to_owned()).unwrap());
        let request = |origin: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/text")
                .header("host", "localhost:8765")
                .header("origin", origin)
                .header("x-lan-drop", "1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hello 世界"}"#))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request("http://evil.example"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = app
            .clone()
            .oneshot(request("http://localhost:8765"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let name = body["name"].as_str().unwrap();
        let name = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/download/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("attachment")
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "hello 世界"
        );
    }
    #[tokio::test]
    async fn multipart_is_saved_and_traversal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned()).unwrap();
        let app = router(store.clone());
        for (name, status) in [
            ("你好.txt", StatusCode::OK),
            ("../escape.txt", StatusCode::BAD_REQUEST),
        ] {
            let body = format!(
                "--test\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--test--\r\n"
            );
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/upload")
                        .header("x-lan-drop", "1")
                        .header("content-type", "multipart/form-data; boundary=test")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
        assert_eq!(store.read_text("你好.txt").unwrap(), "hello");
        assert_eq!(store.list().unwrap().len(), 1);
        let incomplete = "--test\r\nContent-Disposition: form-data; name=\"files\"; filename=\"partial.txt\"\r\n\r\npartial";
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/upload")
                    .header("x-lan-drop", "1")
                    .header("content-type", "multipart/form-data; boundary=test")
                    .body(Body::from(incomplete))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!response.status().is_success());
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 1);
    }
}
