use ai_phone_api::wire;
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use prost::Message;
use sqlx::PgPool;
#[cfg(test)]
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod agent_loop;
mod anthropic_messages;
mod app_learning;
mod application_identity;
mod auth;
mod backup;
mod builtin_guides;
mod channels;
mod chat_completions;
mod completion_audit;
mod database;
mod desktop;
mod device_exchange;
mod device_network;
mod device_setup;
mod exploration_policy;
mod files;
mod harness_proxy;
mod harness_proxy_devices;
mod history_window;

mod http;
mod location;
mod location_devices;
mod location_motion;
mod model_adapter;
mod model_context;
mod model_sse;
mod model_timing;
mod notification_policy;
mod phone_system;
mod private_file;
mod proactive;
mod redis_events;
mod registry;
mod request_digest;
mod run_notice;
mod run_queue;
mod run_signals;
mod settings;
mod setup;
mod speech;
mod tool_catalog;
mod voice_input;
mod workbench;
mod worker;
mod workspace_runtime;

const MEDIA: &str = "application/x-protobuf";
struct App {
    db: PgPool,
    root: PathBuf,
    token: String,
    phone_token: String,
    settings: settings::SettingsStore,
    local: reqwest::Client,
    remote: reqwest::Client,
    active: Mutex<std::collections::HashMap<Uuid, CancellationToken>>,
    run_signals: run_signals::RunSignals,
    login_attempts: Mutex<Vec<std::time::Instant>>,
    workspace_init: Mutex<Option<String>>,
    location_mutation: Mutex<()>,
    network_mutation: Mutex<()>,
}
type Shared = Arc<App>;
fn pb(status: StatusCode, value: impl Message) -> Response {
    (
        status,
        [("content-type", MEDIA), ("cache-control", "no-store")],
        value.encode_to_vec(),
    )
        .into_response()
}
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    pb(
        status,
        wire::ApiError {
            code: code.into(),
            message: message.into(),
        },
    )
}
// Keep transport responses off the success-path Result stack.
struct Rejection(Box<Response>);
impl From<Rejection> for Response {
    fn from(rejection: Rejection) -> Self {
        *rejection.0
    }
}
fn rejected(status: StatusCode, code: &str, message: &str) -> Rejection {
    Rejection(Box::new(error(status, code, message)))
}
fn auth(app: &App, headers: &HeaderMap) -> Result<(), Rejection> {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(format!("Bearer {}", app.token).as_str())
    {
        return Err(rejected(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "未配对的工作台",
        ));
    }
    Ok(())
}
fn decode<T: Message + Default>(
    app: &App,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<T, Rejection> {
    auth(app, headers)?;
    if headers.get("content-type").and_then(|v| v.to_str().ok()) != Some(MEDIA) {
        return Err(rejected(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            MEDIA,
        ));
    }
    T::decode(body.map_err(|e| rejected(e.status(), "body_rejected", "请求过大或无法读取"))?)
        .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalid_protobuf", "请求格式错误"))
}
fn db_error(_: impl std::fmt::Display) -> Response {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage_error",
        "保存失败，请重试",
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if let Some(result) = ai_phone_host_runtime::component::run(&arguments) {
        return result.map_err(Into::into);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(arguments))
}

async fn serve(arguments: Vec<std::ffi::OsString>) -> Result<(), Box<dyn std::error::Error>> {
    if arguments
        .first()
        .is_some_and(|argument| argument == "--state-backup")
    {
        println!("{}", backup::run(&arguments[1..])?);
        return Ok(());
    }
    if arguments.as_slice() == [std::ffi::OsString::from("--channels")] {
        let root = PathBuf::from(
            std::env::var_os("AI_PHONE_PROJECT_ROOT").ok_or("channel_root_required")?,
        );
        return channels::run(&root)
            .await
            .map_err(|error| error as Box<dyn std::error::Error>);
    }
    #[cfg(debug_assertions)]
    if arguments.len() == 2 && arguments[0] == "--qualify-workspace-runtime" {
        workspace_runtime::qualify(
            arguments[1]
                .to_str()
                .ok_or("workspace image is not UTF-8")?,
        )
        .await?;
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if arguments.as_slice() == [std::ffi::OsStr::new("--qualify-file-manager-devices")] {
        files::qualify_devices().await;
        return Ok(());
    }
    if arguments.iter().any(|arg| {
        arg == "--device-setup-contract" || arg == "--setup-devices" || arg == "--setup-wsl-devices"
    }) {
        if arguments.len() != 1 {
            return Err("device_setup_arguments".into());
        }
        if arguments[0] == "--device-setup-contract" {
            println!("{}", device_setup::CONTRACT);
            return Ok(());
        }
        let wsl = arguments[0] == "--setup-wsl-devices";
        let root = if wsl {
            None
        } else {
            Some(
                std::env::var_os("AI_PHONE_PROJECT_ROOT")
                    .map(PathBuf::from)
                    .ok_or("device_setup_owned_root_required")?,
            )
        };
        let cancelled = CancellationToken::new();
        let signal = tokio::spawn(device_setup::interrupt(cancelled.clone()));
        let result = if let Some(root) = root {
            device_setup::run(&root, cancelled, false).await.map(|_| ())
        } else {
            device_setup::wsl::run(cancelled).await
        };
        signal.abort();
        let _ = signal.await;
        result?;
        if !wsl {
            println!(
                "Owned phone frame/root and desktop frame verified; user conversations unchanged"
            );
        }
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--database-prepare-contract")
    {
        if arguments.len() != 1 {
            return Err("database_prepare_arguments".into());
        }
        println!("{}", database::bootstrap::CONTRACT);
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--version") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let setup_admin = std::env::args().any(|arg| arg == "--set-admin");
    let migrate_database = std::env::args().any(|arg| arg == "--migrate");
    let check_database = std::env::args().any(|arg| arg == "--database-check");
    let sync_skills = std::env::args().any(|arg| arg == "--sync-builtin-skills");
    let root = std::env::var_os("AI_PHONE_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    if arguments
        .first()
        .is_some_and(|argument| argument == "--prepare-database")
    {
        if arguments.len() != 1 {
            return Err("database_prepare_arguments".into());
        }
        return database::bootstrap::prepare(&root).await;
    }
    if !setup_admin
        && !sync_skills
        && !migrate_database
        && !check_database
        && root.join("data/restore-quarantine.json").exists()
    {
        return Err("Restored state is quarantined; verify recovery and stop the original instance before activation".into());
    }
    let listener = if setup_admin || sync_skills || migrate_database || check_database {
        None
    } else {
        Some(tokio::net::TcpListener::bind("127.0.0.1:17863").await?)
    };
    let db = database::connect(&root, migrate_database).await?;
    if migrate_database {
        println!("Database migrations applied by dedicated migration login");
        return Ok(());
    }
    if check_database {
        let user: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&db)
            .await?;
        println!(
            "{}",
            serde_json::json!({"runtime":user,"privileged":false,"ddl":false})
        );
        return Ok(());
    }
    let settings = settings::SettingsStore::open(&root)?;
    if setup_admin {
        use std::io::Read;
        let mut password = String::new();
        std::io::stdin().read_to_string(&mut password)?;
        return auth::set_admin(&db, password.trim_end_matches(['\r', '\n'])).await;
    }
    if sync_skills {
        workbench::initialize(&db, &root).await?;
        println!("Built-in skills synchronized; running tasks were not changed.");
        return Ok(());
    }
    worker::recover_interrupted_runs(&db).await?;
    sqlx::query("UPDATE runs SET model_revision=$1 WHERE model_revision=0")
        .bind(settings.current() as i64)
        .execute(&db)
        .await?;
    registry::initialize(&db, &settings).await?;
    setup::initialize(&db, &root).await?;
    let host_proxy = std::env::var("AI_PHONE_HOST_PROXY").ok();
    let app = Arc::new(App {
        db,
        token: std::fs::read_to_string(root.join("data/assistant/token"))?
            .trim()
            .into(),
        phone_token: std::fs::read_to_string(root.join("data/phone-runner/token"))?
            .trim()
            .into(),
        root,
        settings,
        local: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()?,
        remote: harness_proxy::download_client(host_proxy.as_deref())?,
        active: Mutex::new(Default::default()),
        run_signals: Default::default(),
        login_attempts: Mutex::new(Vec::new()),
        workspace_init: Mutex::new(None),
        location_mutation: Mutex::new(()),
        network_mutation: Mutex::new(()),
    });
    workbench::initialize(&app.db, &app.root).await?;
    app_learning::backfill(&app).await?;
    desktop::recover(app.clone()).await;
    if let Some(redis) = redis_events::client() {
        let app = app.clone();
        tokio::spawn(
            async move { redis_events::publish(redis, &app.run_signals.channel_update).await },
        );
    }
    tokio::spawn(worker::work(app.clone()));
    tokio::spawn(proactive::work(app.clone()));
    tokio::spawn(location::work(app.clone()));
    tokio::spawn(device_exchange::work(app.clone()));
    tokio::spawn(device_network::recover(app.clone()));
    tokio::spawn(harness_proxy::resume(app.clone()));

    let routes = Router::new()
        .route("/v1/registry/{operation}", post(registry::route))
        .route("/v1/location/{operation}", post(location::route))
        .route("/v1/proxy/{operation}", post(harness_proxy::route))
        .route(
            "/v1/device-network/{operation}",
            post(device_network::route),
        )
        .route("/v1/desktop/{operation}", post(desktop::route))
        .route("/v1/files/{operation}", post(files::route))
        .route("/v1/settings/{operation}", post(settings::route))
        .route("/v1/proactive/{operation}", post(proactive::route))
        .route("/v1/setup/{operation}", post(setup::route))
        .route("/v1/login", post(auth::login))
        .route("/v1/enter", post(auth::enter))
        .route("/v1/session", post(auth::session))
        .route("/v1/logout", post(auth::logout))
        .route("/v1/history", post(http::history))
        .route("/v1/chat/events", axum::routing::get(run_signals::events))
        .route("/v1/send", post(http::send))
        .route("/v1/run", post(http::run_state))
        .route("/v1/workbench/{operation}", post(workbench::route))
        .route("/v1/workbench/file", axum::routing::get(workbench::file))
        .route("/v1/cancel", post(http::cancel))
        .route("/v1/resume", post(http::resume))
        .layer(DefaultBodyLimit::max(65536));
    let routes = routes
        .route(
            "/v1/voice/playback",
            axum::routing::get(speech::playback::route),
        )
        .route(
            "/v1/voice/stream",
            axum::routing::get(speech::realtime::route),
        )
        .route(
            "/v1/voice/{operation}",
            post(speech::route).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route(
            "/v1/files/upload",
            post(files::upload).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/v1/workbench/upload-file",
            post(workbench::upload_file).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        .route(
            "/v1/workbench/knowledge-image-upload",
            post(workbench::upload_knowledge_image).layer(DefaultBodyLimit::max(
                workbench::knowledge_images::LIMIT + 16,
            )),
        )
        .route(
            "/v1/workbench/knowledge-image/{id}",
            axum::routing::get(workbench::knowledge_images::read),
        )
        .route(
            "/v1/upload",
            post(http::upload).layer(DefaultBodyLimit::max(20 * 1024 * 1024)),
        )
        .route("/v1/attachment/{id}", axum::routing::get(http::attachment))
        .with_state(app);
    println!("AI Phone assistant listening on 127.0.0.1:17863");
    axum::serve(listener.ok_or("Missing listener")?, routes)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod app_guidance_tests;

#[cfg(test)]
mod run_activity_tests;
