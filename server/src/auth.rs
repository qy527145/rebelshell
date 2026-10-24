//! 令牌鉴权。
//!
//! 所有 `/api/**` 与 `/ws` 都要求携带 `Authorization: Bearer <ADMIN_TOKEN>`。
//! 浏览器无法为 WebSocket 握手设置自定义头，因此 `/ws` 额外接受
//! `?token=<ADMIN_TOKEN>` 查询参数。

use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::state::AppState;

/// 常量时间比较，避免通过响应时间逐字节试探令牌。
fn secure_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // 长度不同直接判否；长度本身不是秘密（令牌长度固定用于部署约定）。
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 未通过鉴权时的统一响应。
pub struct Unauthorized(pub &'static str);

impl IntoResponse for Unauthorized {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, self.0).into_response()
    }
}

/// 校验请求携带的令牌（`Authorization: Bearer` 或 `?token=`）。
///
/// WebSocket 握手走的是 GET，浏览器端只能用查询参数，因此这里两种都接受。
pub fn authorize(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> Result<(), Unauthorized> {
    let header_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    let presented = header_token.or(query_token);
    match presented {
        Some(token) if secure_eq(token, &state.config.admin_token) => Ok(()),
        Some(_) => Err(Unauthorized("管理令牌不正确")),
        None => Err(Unauthorized("缺少管理令牌：请先登录")),
    }
}

/// 供 REST 路由使用的提取器；通过即在 handler 里拿到 [`Authed`]。
pub struct Authed;

impl FromRequestParts<AppState> for Authed {
    type Rejection = Unauthorized;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let query_token = Query::<TokenQuery>::try_from_uri(&parts.uri)
            .ok()
            .and_then(|Query(query)| query.token);
        authorize(state, &parts.headers, query_token.as_deref())?;
        Ok(Authed)
    }
}

#[derive(Deserialize)]
pub struct TokenQuery {
    pub token: Option<String>,
}

/// `POST /api/login`：校验令牌，成功仅返回 200，令牌由前端自行保存在 localStorage。
pub async fn login(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Option<axum::Json<LoginBody>>,
) -> Response {
    let from_body = body.and_then(|axum::Json(body)| body.token);
    match authorize(&state, &headers, from_body.as_deref()) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(unauthorized) => unauthorized.into_response(),
    }
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_eq_matches_only_identical_strings() {
        assert!(secure_eq("s3cret", "s3cret"));
        assert!(!secure_eq("s3cret", "s3cres"));
        assert!(!secure_eq("s3cret", "s3cret-longer"));
        assert!(!secure_eq("", "x"));
        assert!(secure_eq("", ""));
    }
}
