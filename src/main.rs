use axum::error_handling::HandleErrorLayer;
use axum::BoxError;
use axum::{
    extract::Path,
    response::IntoResponse,
    routing::get,
    Extension, Json, Router,
    extract::MatchedPath,
    http::Request,
    middleware::{self, Next},
};
use hyper::StatusCode;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
    future::ready,
};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use uuid::Uuid;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use utoipa::{OpenApi, Component};

#[derive(OpenApi)]
#[openapi(handlers(get_todo_by_id, save_todo, get_todos), components(Todo, CreateTodo))]
struct ApiDoc;

type Db = Arc<RwLock<HashMap<Uuid, Todo>>>;

fn app(db: Db) -> Router {
    let recorder_handle = setup_metrics_recorder();
    Router::new()
        .route("/todos", get(get_todos).post(save_todo))
        .route("/todos/:id", get(get_todo_by_id))
        .route("/metrics", get(move || ready(recorder_handle.render())))
        .route("/api-doc/openapi.json", get(openapi))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|error: BoxError| async move {
                    if error.is::<tower::timeout::error::Elapsed>() {
                        Ok(StatusCode::REQUEST_TIMEOUT)
                    } else {
                        Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Unhandled internal error: {}", error),
                        ))
                    }
                }))
                .timeout(Duration::from_secs(10))
                .layer(TraceLayer::new_for_http())
                .layer(Extension(db))
                .into_inner(),
        )
        .route_layer(middleware::from_fn(track_metrics))
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let app = app(Db::default());

    // Address that server will bind to.
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    // Use `hyper::server::Server` which is re-exported through `axum::Server` to serve the app.
    axum::Server::bind(&addr)
        // Hyper server takes a make service.
        .serve(app.into_make_service())
        .await
        .unwrap();
    Ok(())
}

#[derive(Debug, Serialize, Clone, Deserialize, Component)]
pub struct Todo {
    id: Uuid,
    user: Option<String>,
    text: String,
    completed: bool,
}

#[derive(Debug, Serialize, Clone, Deserialize, Component)]
pub struct CreateTodo {
    text: String,
    user: Option<String>,
}

#[utoipa::path(
    get,
    path = "/todos",
    responses(
        (status = 200, description = "List of todos", body = [Todo])
    )
)]
pub async fn get_todos(todos: Extension<Db>) -> impl IntoResponse {
    Json(todos.read().unwrap().values().cloned().collect::<Vec<_>>())
}

#[utoipa::path(
    get,
    path = "/todos/{id}",
    responses(
        (status = 200, description = "Todo found succesfully", body = Todo),
        (status = 404, description = "Todo was not found")
    ),
    params(
        ("id" = Uuid, path, description = "Todo id to get Todo"),
    )
)]
pub async fn get_todo_by_id(Path(id): Path<Uuid>, todos: Extension<Db>) -> impl IntoResponse {
    Json(
        todos
            .read()
            .unwrap()
            .get(&id)
            .cloned()
    )
}

#[utoipa::path(
    post,
    path = "/todos",
    responses(
        (status = 201, description = "Todo saved succesfully", body = Todo)
    ),
    request_body = CreateTodo,
)]
pub async fn save_todo(Json(input): Json<CreateTodo>, todos: Extension<Db>) -> impl IntoResponse {
    let todo = Todo {
        id: Uuid::new_v4(),
        user: input.user,
        text: input.text,
        completed: false,
    };

    todos.write().unwrap().insert(todo.id, todo.clone());

    (StatusCode::CREATED, Json(todo))
}

async fn openapi() -> impl IntoResponse {
    Json(ApiDoc::openapi())
}

fn setup_metrics_recorder() -> PrometheusHandle {
    const EXPONENTIAL_SECONDS: &[f64] = &[
        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("http_requests_duration_seconds".to_string()),
            EXPONENTIAL_SECONDS,
        )
        .unwrap()
        .install_recorder()
        .unwrap()
}

async fn track_metrics<B>(req: Request<B>, next: Next<B>) -> impl IntoResponse {
    let start = Instant::now();
    let path = if let Some(matched_path) = req.extensions().get::<MatchedPath>() {
        matched_path.as_str().to_owned()
    } else {
        req.uri().path().to_owned()
    };
    let method = req.method().clone();

    let response = next.run(req).await;

    let latency = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let labels = [
        ("method", method.to_string()),
        ("path", path),
        ("status", status),
    ];

    metrics::increment_counter!("http_requests_total", &labels);
    metrics::histogram!("http_requests_duration_seconds", latency, &labels);

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    #[tokio::test]
    async fn save_note() {
        let todo1 = Todo {
            id: Uuid::new_v4(),
            user: None,
            text: "todo 1".to_owned(),
            completed: false,
        };
        let todo2 = Todo {
            id: Uuid::new_v4(),
            user: Some("user".to_owned()),
            text: "todo 2".to_owned(),
            completed: true,
        };
        let app = app(Arc::new(RwLock::new(HashMap::from([
            (todo1.id, todo1),
            (todo2.id, todo2),
        ]))));

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/todos")
                    .header(http::header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref())
                    .body(Body::from(
                        serde_json::to_vec(&json!({"text": "test todo", "user": Some("user")}))
                            .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);

        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let body: Todo = serde_json::from_slice(&body).unwrap();
        assert_eq!(body.text, "test todo".to_owned());
        assert_eq!(body.user, Some("user".to_owned()));
    }

    #[tokio::test]
    async fn get_note_by_id() {
        let todo1 = Todo {
            id: Uuid::new_v4(),
            user: None,
            text: "todo 1".to_owned(),
            completed: false,
        };
        let todo2 = Todo {
            id: Uuid::new_v4(),
            user: Some("user".to_owned()),
            text: "todo 2".to_owned(),
            completed: true,
        };
        let search_todo = todo1.clone();
        let app = app(Arc::new(RwLock::new(HashMap::from([
            (todo1.id, todo1),
            (todo2.id, todo2),
        ]))));

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::GET)
                    .uri(format!("/todos/{}", search_todo.id))
                    .header(http::header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let body: Todo = serde_json::from_slice(&body).unwrap();
        assert_eq!(body.text, search_todo.text);
        assert_eq!(body.user, search_todo.user);
        assert_eq!(body.id, search_todo.id);
    }

    #[tokio::test]
    async fn empty_list() {
        let app = app(Db::default());

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::GET)
                    .uri("/todos")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body.is_array());
        assert!(body.as_array().is_some());
        assert!(body.as_array().unwrap().is_empty());
    }
}
