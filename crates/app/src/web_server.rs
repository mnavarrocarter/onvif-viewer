//! HTTP and signaling run off the GTK thread. Camera data is copied at refresh.
use crate::{storage::Source, web_config::Config, web_ptz::PtzLeases, web_security};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    net::TcpListener,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, oneshot};

type Sources = Arc<RwLock<Vec<Source>>>;
fn same_web_source(a: &Source, b: &Source) -> bool {
    a.id == b.id
        && a.address == b.address
        && a.username == b.username
        && a.password == b.password
        && a.profiles.len() == b.profiles.len()
        && a.profiles.iter().zip(&b.profiles).all(|(a, b)| {
            a.token == b.token && a.codec == b.codec && a.audio_codec == b.audio_codec
        })
}
struct Session {
    source: Source,
    camera_id: u64,
    token: String,
    last_seen: Instant,
    _stream: web_media::Stream,
    _peer: web_media::Peer,
    _permit: OwnedSemaphorePermit,
}
struct App {
    config: Config,
    sources: Sources,
    extra_hosts: Vec<String>,
    media: Arc<web_media::MediaManager>,
    capacity: Arc<Semaphore>,
    versions: std::sync::Mutex<HashMap<u64, (Source, uuid::Uuid)>>,
    sessions: Mutex<HashMap<String, Session>>,
    ptz: PtzLeases,
}

pub struct Server {
    stop: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(config: Config, sources: Sources) -> Result<Server, String> {
    crate::web_config::validate(&config).map_err(|e| e.to_string())?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, config.port))
        .map_err(|e| format!("Cannot bind LAN viewer port {}: {e}", config.port))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "Cannot configure LAN listener".to_owned())?;
    let media = web_media::MediaManager::new(4, 4).map_err(|e| e.to_string())?;
    let app = Arc::new(App {
        config,
        sources,
        extra_hosts: std::env::var("ONVIF_WEB_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        media: Arc::new(media),
        capacity: Arc::new(Semaphore::new(16)),
        versions: std::sync::Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        ptz: PtzLeases::default(),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Cannot initialize LAN server runtime: {e}"))?;
    let listener = {
        let _guard = runtime.enter();
        tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("Cannot start LAN listener: {e}"))?
    };
    let (stop, stopped) = oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("lan-web-server".into())
        .spawn(move || {
            runtime.block_on(async move {
                let router = Router::new()
                    .route("/", get(index))
                    .route("/style.css", get(style))
                    .route("/app.js", get(script))
                    .route("/api/cameras", get(cameras))
                    .route("/api/session", post(open_session))
                    .route("/api/session/{id}", delete(close_session))
                    .route("/api/session/{id}/heartbeat", post(heartbeat))
                    .route("/api/ptz", post(ptz))
                    .layer(middleware::from_fn_with_state(app.clone(), security))
                    .with_state(app.clone());
                let cleanup = app.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let expired: Vec<String> = {
                            let sources = cleanup.sources.read().unwrap().clone();
                            let sessions = cleanup.sessions.lock().await;
                            sessions
                                .iter()
                                .filter(|(_, session)| {
                                    session.last_seen.elapsed() > Duration::from_secs(35)
                                        || !sources
                                            .iter()
                                            .any(|source| same_web_source(source, &session.source))
                                        || session._peer.is_closed()
                                        || session._stream.check().is_err()
                                })
                                .map(|(id, _)| id.clone())
                                .collect()
                        };
                        for id in expired {
                            remove_session(&cleanup, &id).await;
                        }
                    }
                });
                tokio::select! { _ = axum::serve(listener, router) => {}, _ = stopped => {} }
                let ids: Vec<String> = app.sessions.lock().await.keys().cloned().collect();
                for id in ids {
                    remove_session(&app, &id).await;
                }
            });
        })
        .map_err(|_| "Cannot start LAN server worker".to_owned())?;
    Ok(Server {
        stop: Some(stop),
        worker: Some(worker),
    })
}

async fn security(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let authority = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !web_security::allowed_host(authority, app.config.port, &app.extra_hosts) {
        return (StatusCode::BAD_REQUEST, "Invalid Host").into_response();
    }
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !web_security::authenticate(auth, &app.config.username, &app.config.password) {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"ONVIF LAN viewer\", charset=\"UTF-8\"",
            )],
            "Authentication required",
        )
            .into_response();
    }
    if req.method() != axum::http::Method::GET && req.method() != axum::http::Method::HEAD {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        if !web_security::same_origin(origin, authority) {
            return (StatusCode::FORBIDDEN, "Origin rejected").into_response();
        }
    }
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; media-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'".parse().unwrap());
    response
}
async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../web/index.html"),
    )
}
async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}
fn failure(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":message.into()})),
    )
        .into_response()
}
async fn cameras(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let sources = app.sources.read().unwrap().clone();
    let mut result = Vec::new();
    for source in sources {
        let camera = onvif::connect(&source.address, &source.username, &source.password)
            .await
            .ok();
        let mut profiles = Vec::new();
        for p in &source.profiles {
            let supported = match &camera {
                Some(camera) => camera.ptz_supported(&p.token).await.unwrap_or(false),
                None => false,
            };
            profiles.push(json!({"token":p.token,"name":Source::profile_label(p),"ptz":supported}));
        }
        result.push(json!({"id":source.id,"name":source.name,"profiles":profiles}));
    }
    Json(json!(result))
}
#[derive(Deserialize)]
struct Open {
    camera_id: u64,
    profile_token: String,
    offer: String,
}
async fn open_session(State(app): State<Arc<App>>, Json(body): Json<Open>) -> Response {
    if body.offer.len() > 128_000 {
        return failure("SDP offer is too large");
    }
    let Ok(permit) = app.capacity.clone().try_acquire_owned() else {
        return failure("The viewer limit has been reached");
    };
    let source = app
        .sources
        .read()
        .unwrap()
        .iter()
        .find(|s| {
            s.id == body.camera_id && s.profiles.iter().any(|p| p.token == body.profile_token)
        })
        .cloned();
    let Some(source) = source else {
        return failure("Camera or profile not found");
    };
    let camera = match onvif::connect(&source.address, &source.username, &source.password).await {
        Ok(c) => c,
        Err(e) => return failure(e.to_string()),
    };
    let uri = match camera.stream_uri(&body.profile_token).await {
        Ok(uri) => uri,
        Err(e) => return failure(e.to_string()),
    };
    let Ok(mut uri) = url::Url::parse(&uri) else {
        return failure("Camera returned an invalid stream URI");
    };
    if uri.set_password(None).is_err() || uri.set_username("").is_err() {
        return failure("Camera returned an invalid stream URI");
    }
    let uri = uri.to_string();
    let version = {
        let mut versions = app.versions.lock().unwrap();
        let entry = versions
            .entry(source.id)
            .or_insert_with(|| (source.clone(), uuid::Uuid::new_v4()));
        if !same_web_source(&entry.0, &source) {
            *entry = (source.clone(), uuid::Uuid::new_v4());
        }
        entry.1
    };
    let key = format!("{}:{}", version, body.profile_token);
    let manager = app.media.clone();
    let token = body.profile_token;
    let saved_source = source.clone();
    let result = tokio::task::spawn_blocking(move || {
        let stream = manager.open(
            &key,
            web_media::Request {
                uri,
                username: source.username,
                password: source.password,
            },
        )?;
        let (peer, answer) = stream.add_peer(&body.offer)?;
        Ok::<_, web_media::Error>((stream, peer, answer, permit))
    })
    .await;
    let (stream, peer, answer, permit) = match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => return failure(e.to_string()),
        Err(_) => return failure("Media worker failed"),
    };
    let id = uuid::Uuid::new_v4().to_string();
    if !app
        .sources
        .read()
        .unwrap()
        .iter()
        .any(|source| same_web_source(source, &saved_source))
    {
        return failure("Camera configuration changed while connecting");
    }
    app.sessions.lock().await.insert(
        id.clone(),
        Session {
            source: saved_source,
            camera_id: body.camera_id,
            token,
            last_seen: Instant::now(),
            _stream: stream,
            _peer: peer,
            _permit: permit,
        },
    );
    Json(json!({"id":id,"answer":answer})).into_response()
}
async fn remove_session(app: &App, id: &str) {
    let session = app.sessions.lock().await.remove(id);
    app.ptz.stop_session(id).await;
    if let Some(session) = session {
        let _ = tokio::task::spawn_blocking(move || drop(session)).await;
    }
}
async fn close_session(State(app): State<Arc<App>>, Path(id): Path<String>) -> StatusCode {
    remove_session(&app, &id).await;
    StatusCode::NO_CONTENT
}
async fn heartbeat(State(app): State<Arc<App>>, Path(id): Path<String>) -> StatusCode {
    if let Some(session) = app.sessions.lock().await.get_mut(&id) {
        session.last_seen = Instant::now();
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
#[derive(Deserialize)]
struct Move {
    camera_id: u64,
    profile_token: String,
    session_id: String,
    x: f32,
    y: f32,
}
async fn ptz(State(app): State<Arc<App>>, Json(body): Json<Move>) -> Response {
    let sessions = app.sessions.lock().await;
    let valid = sessions
        .get(&body.session_id)
        .filter(|s| s.camera_id == body.camera_id && s.token == body.profile_token);
    let Some(session) = valid else {
        return failure("Session not found");
    };
    let source = app
        .sources
        .read()
        .unwrap()
        .iter()
        .find(|s| s.id == body.camera_id)
        .cloned();
    let Some(source) = source else {
        return failure("Camera not found");
    };
    if !same_web_source(&source, &session.source) {
        return failure("Camera configuration changed");
    }
    let response = match app
        .ptz
        .command(
            &source,
            &body.session_id,
            &body.profile_token,
            body.x,
            body.y,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => failure(e),
    };
    drop(sessions);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn request(port: u16, request: String) -> String {
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    }
    #[test]
    fn native_profile_selection_does_not_invalidate_web_sessions() {
        let source = Source {
            id: 1,
            name: "Camera".into(),
            address: "http://camera".into(),
            username: "user".into(),
            password: "password".into(),
            model: String::new(),
            serial: String::new(),
            profiles: vec![onvif::Profile {
                token: "profile".into(),
                name: "Profile".into(),
                width: Some(640),
                height: Some(480),
                codec: Some("H264".into()),
                audio_codec: None,
            }],
            selected_profile: None,
        };
        let mut changed = source.clone();
        changed.selected_profile = Some("profile".into());
        assert!(same_web_source(&source, &changed));
        changed.password = "new password".into();
        assert!(!same_web_source(&source, &changed));
        changed.password = source.password.clone();
        changed.profiles[0].token = "replacement".into();
        assert!(!same_web_source(&source, &changed));
    }

    #[test]
    fn listener_authenticates_assets_and_rejects_rebinding_and_cross_origin() {
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let server = start(
            Config {
                enabled: true,
                port,
                username: "user".into(),
                password: "pass".into(),
            },
            Arc::new(RwLock::new(Vec::new())),
        )
        .unwrap();
        let get = |host: String, auth: &str| {
            format!("GET /app.js HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n")
        };
        let host = format!("127.0.0.1:{port}");
        assert!(request(port, get(host.clone(), "")).starts_with("HTTP/1.1 401"));
        assert!(
            request(
                port,
                get(host.clone(), "Authorization: Basic dXNlcjpwYXNz\r\n")
            )
            .starts_with("HTTP/1.1 200")
        );
        assert!(
            request(
                port,
                get(
                    format!("attacker.example:{port}"),
                    "Authorization: Basic dXNlcjpwYXNz\r\n"
                )
            )
            .starts_with("HTTP/1.1 400")
        );
        let cross = format!(
            "POST /api/session HTTP/1.1\r\nHost: {host}\r\nOrigin: http://attacker.example:{port}\r\nAuthorization: Basic dXNlcjpwYXNz\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        assert!(request(port, cross).starts_with("HTTP/1.1 403"));
        drop(server);
    }
}
