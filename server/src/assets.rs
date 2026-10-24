//! 把前端构建产物编译进二进制，避免部署时还要单独分发静态文件目录。

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// 目录由 `web/vite.config.ts` 的 `build.outDir` 指定为 `server/web/dist`。
/// 构建 Server 之前需要先 `bun run build`，否则这里会是空的。
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct Assets;

/// 返回内嵌资源；找不到时回落到 `index.html` 交给前端路由处理。
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(response) = serve(path) {
        return response;
    }
    // 带扩展名的请求（js/css/图标）找不到就如实 404，不要回 index.html。
    if path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'))
    {
        return (StatusCode::NOT_FOUND, "resource not found").into_response();
    }
    serve("index.html").unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "前端资源未打包：请先在 web/ 下执行 `bun run build` 后重新编译服务端",
        )
            .into_response()
    })
}

fn serve(path: &str) -> Option<Response> {
    let asset = Assets::get(path)?;
    let mime = mime_guess(path);
    Some(
        (
            [
                (header::CONTENT_TYPE, mime),
                // Vite 产物带内容哈希，可长缓存；index.html 必须每次校验。
                (
                    header::CACHE_CONTROL,
                    if path == "index.html" {
                        "no-cache"
                    } else {
                        "public, max-age=31536000, immutable"
                    },
                ),
            ],
            asset.data,
        )
            .into_response(),
    )
}

fn mime_guess(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types_are_mapped() {
        assert_eq!(mime_guess("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            mime_guess("assets/app-abc123.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_guess("assets/app.css"), "text/css; charset=utf-8");
        assert_eq!(mime_guess("logo.woff2"), "font/woff2");
        assert_eq!(mime_guess("no-extension"), "application/octet-stream");
    }

    #[test]
    fn placeholder_or_built_index_is_served() {
        // 未执行前端构建时目录里只有占位 index.html；此时仍应能正常返回，
        // 而不是 panic 或 503。
        match serve("index.html") {
            Some(response) => assert_eq!(response.status(), StatusCode::OK),
            None => panic!("index.html 应当总是可服务（构建前由占位文件兜底）"),
        }
    }

    #[test]
    fn missing_hashed_asset_is_not_index() {
        // 带扩展名的资源缺失时不能回落成 index.html，否则浏览器会把 HTML 当 JS 执行。
        assert!(serve("assets/does-not-exist.js").is_none());
    }
}
