use serde::Serialize;
use std::{
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder, webview::PageLoadEvent};
mod bootstrap;
mod copilot;
mod updates;

static WORKBENCH_URL: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("http://127.0.0.1:17860/phone").expect("valid fixed workbench URL")
});

#[derive(Default)]
struct Startup(Mutex<StartupProgress>);

#[derive(Default)]
struct Exit(Mutex<ExitPhase>);

#[derive(Default)]
enum ExitPhase {
    #[default]
    Open,
    Stopping,
    Ready,
}

#[derive(Clone, Default, Serialize)]
struct StartupProgress {
    phase: &'static str,
    error: String,
}

#[derive(Serialize)]
struct DeploymentStatus {
    #[serde(flatten)]
    deployment: Option<bootstrap::Snapshot>,
    startup: StartupProgress,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    activity: Vec<bootstrap::Activity>,
}

fn is_setup_url(url: &Url) -> bool {
    let origin = (url.scheme(), url.host_str(), url.port());
    matches!(
        origin,
        ("tauri", Some("localhost"), None) | ("http" | "https", Some("tauri.localhost"), None)
    ) && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/" | "/index.html")
        && url.query().is_none()
        && url.fragment().is_none()
}

impl Startup {
    fn progress(&self) -> StartupProgress {
        self.0.lock().unwrap().clone()
    }

    fn set(&self, phase: &'static str, error: String) {
        *self.0.lock().unwrap() = StartupProgress { phase, error };
    }

    fn begin(&self) -> Result<(), String> {
        let mut progress = self.0.lock().unwrap();
        if !matches!(progress.phase, "" | "failed") {
            return Err("startup_in_progress".into());
        }
        *progress = StartupProgress {
            phase: "checking",
            error: String::new(),
        };
        Ok(())
    }
}

fn is_workbench_origin(url: &Url) -> bool {
    url.scheme() == WORKBENCH_URL.scheme()
        && url.host() == WORKBENCH_URL.host()
        && url.port() == WORKBENCH_URL.port()
        && url.username().is_empty()
        && url.password().is_none()
}

fn workbench_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .build()
}

async fn probe_workbench(url: Url) -> bool {
    let Ok(client) = workbench_client() else {
        return false;
    };
    probe_workbench_with(&client, url).await
}

async fn probe_workbench_with(client: &reqwest::Client, url: Url) -> bool {
    client.head(url).send().await.is_ok_and(|response| {
        response.status().is_success()
            && response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/html"))
    })
}

fn show_setup(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("setup") {
        return window.set_focus().map_err(|e| e.to_string());
    }
    let startup = !app.state::<Startup>().progress().phase.is_empty();
    WebviewWindowBuilder::new(app, "setup", WebviewUrl::App("index.html".into()))
        .title("")
        .inner_size(680., 600.)
        .resizable(false)
        .on_navigation(is_setup_url)
        .initialization_script(if startup {
            "window.__AIMANITY_STARTUP__ = true;"
        } else {
            ""
        })
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn validate_locale(locale: &str) -> Result<(), String> {
    match locale {
        "zh-CN" | "en" | "zh-TW" | "ja" | "ko" | "de" | "fr" | "es" | "ru" | "ar" => Ok(()),
        _ => Err("Unsupported interface language".into()),
    }
}

fn workbench_url(locale: Option<&str>) -> Url {
    let mut url = WORKBENCH_URL.clone();
    if let Some(locale) = locale {
        url.query_pairs_mut().append_pair("language", locale);
    }
    url
}

fn show_workbench(app: &tauri::AppHandle, locale: Option<&str>) -> Result<(), String> {
    let url = workbench_url(locale);
    let label = "workbench";
    if let Some(window) = app.get_webview_window(label) {
        window
            .navigate(url)
            .and_then(|_| window.set_focus())
            .map_err(|e| e.to_string())?;
    } else {
        let (width, height) = app
            .primary_monitor()
            .ok()
            .flatten()
            .map(|monitor| {
                let area = monitor.work_area();
                let scale = monitor.scale_factor();
                (
                    (f64::from(area.size.width) / scale - 48.).clamp(700., 1440.),
                    (f64::from(area.size.height) / scale - 64.).clamp(600., 980.),
                )
            })
            .unwrap_or((1440., 980.));
        let builder = WebviewWindowBuilder::new(app, label, WebviewUrl::External(url))
            .title("AImanity")
            .inner_size(width, height)
            .min_inner_size(700., 600.)
            .on_navigation(is_workbench_origin)
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny);
        if app.state::<Startup>().progress().phase == "opening" {
            let (loaded, receiver) = tokio::sync::oneshot::channel();
            let loaded = Mutex::new(Some(loaded));
            let window = builder
                .visible(false)
                .on_page_load(move |_, page| {
                    if page.event() == PageLoadEvent::Finished
                        && is_workbench_origin(page.url())
                        && page.url().path() == WORKBENCH_URL.path()
                        && let Some(loaded) = loaded.lock().unwrap().take()
                    {
                        let _ = loaded.send(());
                    }
                })
                .build()
                .map_err(|e| e.to_string())?;
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let loaded = tokio::time::timeout(Duration::from_secs(15), receiver)
                    .await
                    .is_ok_and(|value| value.is_ok());
                if loaded && window.show().is_ok() {
                    let _ = window.set_focus();
                    app.state::<Startup>().set("", String::new());
                    close_setup(&app);
                } else {
                    let _ = window.destroy();
                    app.state::<Startup>()
                        .set("failed", "workbench_unavailable".into());
                }
            });
            return Ok(());
        }
        builder.build().map_err(|e| e.to_string())?;
    }
    close_setup(app);
    Ok(())
}

fn close_setup(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("setup") {
        let _ = window.close();
    }
}

fn require_setup_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|e| e.to_string())?;
    if window.label() != "setup" || !is_setup_url(&url) {
        return Err("Native configuration is available only in the setup window".into());
    }
    Ok(())
}

fn require_workbench_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|e| e.to_string())?;
    if window.label() == "workbench" && is_workbench_origin(&url) && url.path() == "/phone" {
        Ok(())
    } else {
        Err("This command is available only in the local workbench".into())
    }
}

fn require_update_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    match window.label() {
        "setup" => require_setup_window(window),
        _ => require_workbench_window(window),
    }
}

#[tauri::command]
async fn check_client_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<Option<updates::UpdateDetails>, String> {
    require_update_window(&window)?;
    updates::refresh(&app, false)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn install_client_update(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    version: String,
) -> Result<(), String> {
    require_update_window(&window)?;
    let version = semver::Version::parse(&version).map_err(|error| error.to_string())?;
    updates::apply(&app, Some(version))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn deployment_status(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<DeploymentStatus, String> {
    require_setup_window(&window)?;
    let startup = app.state::<Startup>().progress();
    let deployment = if matches!(startup.phase, "" | "repairing") {
        Some(bootstrap::snapshot(&app)?)
    } else {
        None
    };
    Ok(DeploymentStatus {
        deployment,
        startup,
        activity: bootstrap::activity(&app),
    })
}
#[tauri::command]
async fn retry_startup(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    require_setup_window(&window)?;
    if !bootstrap::installed(&app)? {
        return Err("invalid_install_checkpoint".into());
    }
    start_startup(&app)
}
#[tauri::command]
fn start_deployment(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    request: bootstrap::Request,
) -> Result<(), String> {
    require_setup_window(&window)?;
    bootstrap::start(app, request)
}
#[tauri::command]
fn cancel_deployment(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    require_setup_window(&window)?;
    bootstrap::cancel(&app);
    Ok(())
}
#[tauri::command]
fn accept_deployment_license(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    require_setup_window(&window)?;
    bootstrap::accept_license(&app)
}

#[tauri::command]
async fn open_workbench(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    locale: Option<String>,
) -> Result<(), String> {
    require_setup_window(&window)?;
    let locale = locale.as_deref();
    if let Some(locale) = locale {
        validate_locale(locale)?;
    }
    if !bootstrap::ready(&app) || !probe_workbench(workbench_url(None)).await {
        return Err("workbench_unavailable".into());
    }
    show_workbench(&app, locale)
}

async fn validate_or_repair(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if bootstrap::same_release(app)? {
        let client = workbench_client().map_err(|_| "workbench_unavailable")?;
        let (workbench, services) = tokio::join!(
            probe_workbench_with(&client, workbench_url(None)),
            bootstrap::installed_services_ready(app, &client),
        );
        if workbench && services {
            return Ok(());
        }
    }
    app.state::<Startup>().set("repairing", String::new());
    bootstrap::resume_installed(app)?;
    let waiting = app.clone();
    tauri::async_runtime::spawn_blocking(move || bootstrap::wait_until_idle(&waiting))
        .await
        .map_err(|_| "installer_state")?;
    bootstrap::startup_result(app)?;
    probe_workbench(workbench_url(None))
        .await
        .then_some(())
        .ok_or_else(|| "workbench_unavailable".into())
}

fn start_startup(app: &tauri::AppHandle) -> Result<(), String> {
    app.state::<Startup>().begin()?;
    show_setup(app)?;
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = validate_or_repair(&handle).await {
            handle.state::<Startup>().set("failed", error);
            return;
        }
        handle.state::<Startup>().set("opening", String::new());
        let ui = handle.clone();
        if handle
            .run_on_main_thread(move || {
                if let Err(error) = show_workbench(&ui, None) {
                    ui.state::<Startup>().set("failed", error);
                }
            })
            .is_err()
        {
            handle
                .state::<Startup>()
                .set("failed", "workbench_unavailable".into());
        }
    });
    Ok(())
}

pub fn run() {
    {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        if let Some(code) = updates::run(&arguments) {
            std::process::exit(code);
        }
        if let [command, folder] = arguments.as_slice()
            && command == "--verify-bundled-host"
        {
            if let Err(error) = bootstrap::verify_bundled_host(std::path::Path::new(folder)) {
                eprintln!("{error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        #[cfg(windows)]
        if arguments.len() == 1 && arguments[0] == "--stop-windows-backend" {
            let result = std::env::var_os("APPDATA")
                .map(std::path::PathBuf::from)
                .ok_or("windows_app_data")
                .and_then(|home| {
                    let home = home.join("dev.aiphone.client/host");
                    tauri::async_runtime::block_on(
                        ai_phone_host_runtime::qemu_host::stop_installed(&home),
                    )
                    .map_err(|error| error.code())
                });
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
        #[cfg(windows)]
        if arguments.len() == 1 && arguments[0] == "--prepare-windows-host" {
            match bootstrap::prepare_windows_host() {
                Ok(code) => std::process::exit(code),
                Err(error) => {
                    println!("{error}");
                    std::process::exit(1);
                }
            }
        }
        if let Some(result) = ai_phone_host_runtime::component::run(&arguments) {
            match result {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
    }
    let builder = tauri::Builder::default()
        .manage(Startup::default())
        .manage(Exit::default())
        .manage(bootstrap::Bootstrap::default())
        .manage(copilot::Voice::default())
        .invoke_handler(tauri::generate_handler![
            open_workbench,
            deployment_status,
            retry_startup,
            start_deployment,
            cancel_deployment,
            accept_deployment_license,
            check_client_update,
            install_client_update,
            copilot::native_voice
        ])
        .setup(|app| {
            updates::cleanup_current(app.handle());
            {
                use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
                let setup = MenuItem::with_id(app, "setup", "Setup…", true, Some("CmdOrCtrl+,"))?;
                let update = MenuItem::with_id(
                    app,
                    updates::ID,
                    updates::CHECK,
                    updates::platform().is_some(),
                    None::<&str>,
                )?;
                app.manage(updates::Menu::new(update.clone()));
                let app_menu = Submenu::with_items(
                    app,
                    "AImanity",
                    true,
                    &[
                        &setup,
                        &update,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::quit(app, Some("Quit AImanity"))?,
                    ],
                )?;
                let edit = Submenu::with_items(
                    app,
                    "Edit",
                    true,
                    &[
                        &PredefinedMenuItem::undo(app, None)?,
                        &PredefinedMenuItem::redo(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::cut(app, None)?,
                        &PredefinedMenuItem::copy(app, None)?,
                        &PredefinedMenuItem::paste(app, None)?,
                        &PredefinedMenuItem::select_all(app, None)?,
                    ],
                )?;
                app.set_menu(Menu::with_items(app, &[&app_menu, &edit])?)?;
            }
            let handle = app.handle();
            updates::check_on_launch(handle);
            match bootstrap::installed(handle) {
                Ok(true) => start_startup(handle)?,
                Ok(false) => show_setup(handle)?,
                Err(error) => {
                    handle.state::<Startup>().set("failed", error);
                    show_setup(handle)?;
                }
            }
            Ok(())
        });
    let builder = builder.on_menu_event(|app, event| match event.id().as_ref() {
        "setup" => {
            let _ = show_setup(app);
        }
        updates::ID => updates::select(app),
        _ => {}
    });
    builder
        .build(tauri::generate_context!())
        .expect("Unable to launch AImanity client")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                let state = app.state::<Exit>();
                let mut phase = state.0.lock().unwrap();
                match *phase {
                    ExitPhase::Ready => return,
                    ExitPhase::Stopping => {
                        api.prevent_exit();
                        return;
                    }
                    ExitPhase::Open => *phase = ExitPhase::Stopping,
                }
                api.prevent_exit();
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let result = bootstrap::shutdown(&app).await;
                    if let Err(error) = &result {
                        eprintln!("shutdown: {error}");
                    }
                    *app.state::<Exit>().0.lock().unwrap() = ExitPhase::Ready;
                    app.exit(if result.is_ok() {
                        code.unwrap_or_default()
                    } else {
                        1
                    });
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn startup_status_omits_setup_data() {
        let status = DeploymentStatus {
            deployment: None,
            activity: Vec::new(),
            startup: StartupProgress {
                phase: "checking",
                error: String::new(),
            },
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({"startup":{"phase":"checking","error":""}})
        );
    }

    #[test]
    fn language_handoff_is_bounded_and_preserves_local_workbench() {
        for locale in [
            "zh-CN", "en", "zh-TW", "ja", "ko", "de", "fr", "es", "ru", "ar",
        ] {
            validate_locale(locale).unwrap();
            let url = workbench_url(Some(locale));
            assert!(is_workbench_origin(&url));
            assert_eq!(url.path(), "/phone");
            assert_eq!(
                url.query_pairs().collect::<Vec<_>>(),
                vec![("language".into(), locale.into())]
            );
        }
        for invalid in ["", "de-DE", "en; Path=/", "../../en"] {
            assert!(validate_locale(invalid).is_err());
        }
        assert!(workbench_url(None).query().is_none());
    }
    #[test]
    fn native_configuration_rejects_lookalike_or_non_asset_urls() {
        for address in [
            "tauri://localhost",
            "tauri://localhost/index.html",
            "http://tauri.localhost/",
        ] {
            assert!(is_setup_url(&Url::parse(address).unwrap()));
        }
        for address in [
            "tauri://attacker/",
            "about:blank",
            "https://tauri.localhost.evil.example/",
            "http://tauri.localhost:17860/",
            "file://tauri.localhost/",
            "tauri://localhost/other.html",
            "tauri://localhost/?language=en",
            "http://user@tauri.localhost/",
        ] {
            assert!(!is_setup_url(&Url::parse(address).unwrap()), "{address}");
        }
    }

    #[test]
    fn workbench_navigation_keeps_the_fixed_local_origin() {
        assert!(is_workbench_origin(&workbench_url(None)));
        assert!(is_workbench_origin(
            &workbench_url(None).join("/login").unwrap()
        ));
        for address in [
            "http://10.1.2.3:17860/phone",
            "http://127.0.0.1:17861/phone",
            "https://127.0.0.1:17860/phone",
            "http://user@127.0.0.1:17860/phone",
        ] {
            assert!(!is_workbench_origin(&Url::parse(address).unwrap()));
        }
    }

    #[test]
    fn probe_checks_real_http_status_type_and_refuses_redirects() {
        for (status, content_type, expected) in [
            (200, "text/html; charset=utf-8", true),
            (200, "application/json", false),
            (503, "text/html", false),
            (302, "text/html", false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = [0; 4096];
                let length = stream.read(&mut bytes).unwrap();
                assert!(
                    String::from_utf8_lossy(&bytes[..length]).starts_with("HEAD /phone HTTP/1.1")
                );
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            });
            let url = Url::parse(&format!("http://{address}/phone")).unwrap();
            assert_eq!(
                tauri::async_runtime::block_on(probe_workbench(url)),
                expected
            );
            server.join().unwrap();
        }
    }
}
