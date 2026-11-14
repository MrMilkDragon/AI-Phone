//! Native local deployment lifecycle.
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::process::Command;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::ChildStdin,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tauri::Manager;
mod checkpoint;
#[cfg(any(windows, test))]
mod native_phone;
#[cfg(unix)]
mod process;
mod resources;
#[cfg(windows)]
mod windows_host;
#[cfg(unix)]
use process::execute;
use resources::{available_bytes, hash};
#[cfg(windows)]
pub(crate) use windows_host::prepare as prepare_windows_host;

static INTEGRITY: LazyLock<Result<BTreeMap<String, String>, serde_json::Error>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../gen/host/integrity.json")));
static POLICY: LazyLock<HostPolicy> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../../config/host-bootstrap.json"))
        .expect("bundled host policy")
});

#[derive(Deserialize)]
struct HostPolicy {
    minimum_ram_gb: u64,
    minimum_free_gb: u64,
    minimum_resume_free_gb: u64,
    hosts: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Phone {
    cpus: u8,
    memory_mb: u32,
    storage_gb: u8,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Desktop {
    cpus: u8,
    memory_mb: u32,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub locale: String,
    phone: Phone,
    desktop: Desktop,
}
impl Request {
    fn validate(&self) -> Result<(), String> {
        super::validate_locale(&self.locale)?;
        if ![1, 2, 4].contains(&self.phone.cpus)
            || ![1, 2, 4].contains(&self.desktop.cpus)
            || ![1536, 2048, 3072, 4096].contains(&self.phone.memory_mb)
            || ![2048, 2560, 3072, 4096].contains(&self.desktop.memory_mb)
            || ![16, 24, 32].contains(&self.phone.storage_gb)
        {
            return Err("invalid_device_resources".into());
        }
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub phase: String,
    pub state: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub license: String,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Activity {
    command: String,
    state: ActivityState,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ActivityState {
    Running,
    Completed,
    Failed,
}
#[derive(Serialize)]
pub struct Capability {
    pub platform: String,
    pub arch: String,
    pub accelerator: String,
    pub implemented: bool,
}
#[derive(Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub value: u64,
    pub required: u64,
}
#[derive(Serialize)]
pub struct Snapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<Capability>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    pub progress: Progress,
    pub previous_request: Option<Request>,
}

pub fn capability(os: &str, arch: &str) -> Capability {
    capability_from_resources(
        os,
        arch,
        INTEGRITY.as_ref().expect("bundled resource inventory"),
    )
}

fn capability_from_resources(
    os: &str,
    arch: &str,
    resources: &BTreeMap<String, String>,
) -> Capability {
    // Native WSL owns its runtime; it never uses the source installer's SDK/Lima
    // host table. Resource bytes and actual device readiness are checked on start.
    let has_device_bundle =
        resources.contains_key("source.payload") && resources.contains_key("android.payload");
    Capability {
        platform: os.into(),
        arch: arch.into(),
        accelerator: match os {
            "macos" => "HVF / Virtualization.framework",
            "windows" if resources.contains_key("qemu/manifest.json") => "WHPX",
            "windows" => "WHPX / WSL2",
            _ => "unsupported",
        }
        .into(),
        implemented: has_device_bundle
            && match (os, arch) {
                ("macos", "aarch64") => {
                    POLICY.hosts.contains_key("macos-arm64")
                        && resources.contains_key("images.payload")
                }
                ("windows", "x86_64") => {
                    resources.contains_key("wsl/manifest.json")
                        || resources.contains_key("qemu/manifest.json")
                }
                _ => false,
            },
    }
}

#[derive(Default)]
pub struct ManagerState {
    running: AtomicBool,
    cancelled: AtomicBool,
    closing: Mutex<bool>,
    progress: Mutex<Progress>,
    activity: Mutex<VecDeque<Activity>>,
    input: Mutex<Option<ChildStdin>>,
}
#[derive(Default)]
pub struct Bootstrap(pub Arc<ManagerState>);

impl ManagerState {
    fn record(&self, entry: Activity) {
        let mut entries = self.activity.lock().unwrap();
        if entry.state != ActivityState::Running
            && let Some(previous) = entries.iter_mut().rev().find(|previous| {
                previous.command == entry.command && previous.state == ActivityState::Running
            })
        {
            previous.state = entry.state;
            return;
        }
        if entries.len() == 12 {
            entries.pop_front();
        }
        entries.push_back(entry);
    }
    #[cfg(windows)]
    fn track<T>(
        &self,
        command: &str,
        run: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.record(Activity {
            command: command.into(),
            state: ActivityState::Running,
        });
        let result = run();
        self.record(Activity {
            command: command.into(),
            state: if result.is_ok() {
                ActivityState::Completed
            } else {
                ActivityState::Failed
            },
        });
        result
    }
    fn publish(&self, folder: &Path, next: Progress) -> Result<(), String> {
        checkpoint::save(folder, &next)?;
        *self.progress.lock().map_err(|_| "installer_state")? = next;
        Ok(())
    }
    fn set(&self, folder: &Path, phase: &str, state: &str, error: &str) -> Result<(), String> {
        self.publish(
            folder,
            Progress {
                phase: phase.into(),
                state: state.into(),
                error: error.into(),
                license: String::new(),
            },
        )
    }
    fn recover(&self, folder: &Path) -> Result<(), String> {
        let mut progress = self.progress.lock().map_err(|_| "installer_state")?;
        if progress.state.is_empty() && !self.running.load(Ordering::SeqCst) {
            *progress = checkpoint::restore(folder)?;
        }
        Ok(())
    }
    fn failed(&self, folder: &Path, error: &str) {
        for entry in self.activity.lock().unwrap().iter_mut() {
            if entry.state == ActivityState::Running {
                entry.state = ActivityState::Failed;
            }
        }
        let state = if error == "installation_cancelled" {
            "cancelled"
        } else {
            "failed"
        };
        if self.set(folder, "failed", state, error).is_err()
            && let Ok(mut progress) = self.progress.lock()
        {
            *progress = Progress {
                phase: "failed".into(),
                state: state.into(),
                error: "checkpoint_write_failed".into(),
                license: String::new(),
            };
        }
    }
    fn check_cancelled(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err("installation_cancelled".into())
        } else {
            Ok(())
        }
    }
    #[cfg(any(target_os = "windows", test))]
    fn complete(&self, folder: &Path) -> Result<(), String> {
        self.check_cancelled()?;
        self.set(folder, "ready", "ready", "")
    }
    #[cfg(unix)]
    fn process_error(&self) -> String {
        self.progress
            .lock()
            .ok()
            .filter(|progress| progress.state == "failed" && !progress.error.is_empty())
            .map(|progress| progress.error.clone())
            .unwrap_or_else(|| "installation_failed".into())
    }
}

pub fn activity(app: &tauri::AppHandle) -> Vec<Activity> {
    app.state::<Bootstrap>()
        .0
        .activity
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect()
}

fn home(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|p| p.join("host"))
        .map_err(|e| e.to_string())
}

struct Components {
    #[cfg(unix)]
    source: String,
    #[cfg(unix)]
    image_digest: Option<String>,
    android: Option<String>,
    wsl: Option<String>,
    qemu: bool,
}

fn verify_resources(folder: &Path, inspect_image_archive: bool) -> Result<Components, String> {
    let expected = INTEGRITY.as_ref().map_err(|e| e.to_string())?;
    if !expected.contains_key("source.payload") {
        return Err("invalid_payload".into());
    }
    for (name, digest) in expected {
        match name.as_str() {
            // These files are authenticated where they are staged or installed.
            "source.payload" | "android.payload" | "windows/wsl.msi" => continue,
            "images.payload" if !inspect_image_archive => continue,
            "images.payload"
            | "tools/manifest.json"
            | "wsl/manifest.json"
            | "qemu/manifest.json"
            | super::copilot::HOST_MODEL_RESOURCE => {}
            _ => return Err("invalid_payload".into()),
        }
        if hash(&folder.join(name)).map_err(|_| "payload_unreadable")? != *digest {
            return Err("invalid_payload".into());
        }
    }
    Ok(Components {
        #[cfg(unix)]
        source: expected["source.payload"].clone(),
        #[cfg(unix)]
        image_digest: expected.get("images.payload").cloned(),
        android: expected.get("android.payload").cloned(),
        wsl: expected.get("wsl/manifest.json").cloned(),
        qemu: expected.contains_key("qemu/manifest.json"),
    })
}

pub(super) fn verify_bundled_host(folder: &Path) -> Result<(), String> {
    verify_resources(folder, true).map(|_| ())
}

fn checks(path: &Path) -> Vec<Check> {
    let ram = resources::total_memory_bytes() / 1024_u64.pow(3);
    let disk = available_bytes(path) / 1024_u64.pow(3);
    let disk_required =
        if path.join("user/data/deployment/profile.json").is_file() || checkpoint::admitted(path) {
            POLICY.minimum_resume_free_gb
        } else {
            POLICY.minimum_free_gb
        };
    vec![
        Check {
            name: "memory".into(),
            passed: ram >= POLICY.minimum_ram_gb,
            value: ram,
            required: POLICY.minimum_ram_gb,
        },
        Check {
            name: "storage".into(),
            passed: disk >= disk_required,
            value: disk,
            required: disk_required,
        },
    ]
}

pub fn snapshot(app: &tauri::AppHandle) -> Result<Snapshot, String> {
    let manager = app.state::<Bootstrap>();
    let path = home(app)?;
    manager.0.recover(&path)?;
    let progress = manager
        .0
        .progress
        .lock()
        .map_err(|_| "installer_state")?
        .clone();
    let (host, checks) = if progress.state.is_empty() {
        (
            Some(capability(std::env::consts::OS, std::env::consts::ARCH)),
            checks(&path),
        )
    } else {
        (None, Vec::new())
    };
    Ok(Snapshot {
        host,
        checks,
        progress,
        previous_request: fs::read(path.join("request.json"))
            .ok()
            .and_then(|v| serde_json::from_slice::<Request>(&v).ok())
            .filter(|r| r.validate().is_ok()),
    })
}

fn prepare_folder(folder: &Path) -> Result<(), String> {
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(folder, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn run(
    app: &tauri::AppHandle,
    manager: &Arc<ManagerState>,
    folder: &Path,
    request: Request,
) -> Result<(), String> {
    let resource = app
        .path()
        .resource_dir()
        .map_err(|e| e.to_string())?
        .join("host");
    let components = verify_resources(&resource, false)?;
    let request_path = folder.join("request.json");
    fs::write(
        &request_path,
        serde_json::to_vec(&request).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    if components.wsl.is_some() || components.qemu {
        return start_windows_backend(manager, folder, &resource, components, &request);
    }
    #[cfg(not(target_os = "windows"))]
    if components.wsl.is_some() || components.qemu {
        return Err("windows_backend_host".into());
    }
    #[cfg(unix)]
    return run_native_installer(manager, folder, &resource, &request_path, components);
    #[cfg(not(unix))]
    Err("wsl_backend_missing".into())
}

#[cfg(target_os = "windows")]
fn start_windows_backend(
    manager: &Arc<ManagerState>,
    folder: &Path,
    resource: &Path,
    components: Components,
    request: &Request,
) -> Result<(), String> {
    use ai_phone_api::wire::NativeAndroidVmEndpoint;
    use ai_phone_host_runtime::{qemu_host, vm_control};

    let android = components
        .android
        .as_deref()
        .ok_or("android_runtime_missing")?;
    manager.set(folder, "backend", "running", "")?;
    let cancelled = || manager.cancelled.load(Ordering::SeqCst);
    let mut installation = ai_phone_host_runtime::Installation::lock(folder)
        .map_err(|error| error.code().to_string())?;
    if components.qemu
        && folder
            .join("wsl-owner.json")
            .try_exists()
            .map_err(|e| e.to_string())?
    {
        return Err("windows_backend_migration_required".into());
    }
    manager.set(folder, "android", "running", "")?;
    let cached = if checkpoint::same_release(folder)? {
        installation
            .resume_android(android)
            .map_err(|error| error.code().to_string())?
    } else {
        None
    };
    let phone = match cached {
        Some(prepared) => prepared,
        None => installation
            .prepare_android(
                &resource.join("android.payload"),
                android,
                ai_phone_host_runtime::android::Resources {
                    cpus: request.phone.cpus.into(),
                    memory_mb: request.phone.memory_mb,
                    storage_gb: request.phone.storage_gb.into(),
                },
                &available_bytes,
                &cancelled,
            )
            .map_err(|error| error.code().to_string())?,
    };
    if components.qemu {
        let mut profile = serde_json::to_value(request).map_err(|_| "invalid_device_resources")?;
        profile["android_qemu"] = serde_json::Value::Bool(
            ai_phone_host_runtime::android::Engine::installed(&phone.sdk_root)
                == ai_phone_host_runtime::android::Engine::Whpx,
        );
        let profile = serde_json::to_vec(&profile).map_err(|_| "invalid_device_resources")?;
        let bundle = resource.join("qemu");
        let home = folder.join("user/qemu");
        let control = vm_control::GUEST_PORT;
        manager.set(folder, "backend", "running", "")?;
        tauri::async_runtime::block_on(qemu_host::ensure(
            &bundle,
            &home,
            request.desktop.cpus.into(),
            ai_phone_host_runtime::desktop_host_memory_mib(request.desktop.memory_mb),
            &profile,
            &cancelled,
            |running| {
                manager.record(Activity {
                    command: "ai-phone-client --qemu-backend-owner".into(),
                    state: if running {
                        ActivityState::Running
                    } else {
                        ActivityState::Completed
                    },
                })
            },
        ))
        .map_err(|error| error.code().to_string())?;
        let key = qemu_host::key_path(&home);
        manager.set(folder, "android", "running", "")?;
        let boot = manager.track("ai-phone-client --android-session", || {
            native_phone::start(
                &phone,
                "",
                Some(NativeAndroidVmEndpoint {
                    port: control.into(),
                    key_file: key.to_str().ok_or("vm_control_key_path")?.into(),
                }),
                &cancelled,
            )
        })?;
        manager.set(folder, "devices", "running", "")?;
        tauri::async_runtime::block_on(async {
            tokio::select! { biased;
                _ = native_phone::cancelled(&cancelled) => Err("installation_cancelled".into()),
                result = vm_control::complete_devices(control, &key, &boot.backend_bridge_id) =>
                    result.map(|_| ()).map_err(|error| error.code().to_string()),
            }
        })?;
        return manager.complete(folder);
    }
    let identity = components.wsl.as_deref().ok_or("wsl_backend_missing")?;
    let profile = serde_json::to_vec(request).map_err(|_| "invalid_device_resources")?;
    let mut native = ai_phone_host_runtime::wsl::Native::new(&cancelled);
    let request = ai_phone_host_runtime::wsl::InitializeRequest::from_json(profile)
        .map_err(|error| error.code().to_string())?;
    let registered = installation
        .start_wsl_bundle(
            &resource.join("wsl"),
            identity,
            &request,
            &mut native,
            &available_bytes,
            &cancelled,
        )
        .map_err(|error| error.code().to_string())?;
    if !registered.backend_initialized {
        return Err("host_backend_pending".into());
    }
    manager.set(folder, "android", "running", "")?;
    // The detached worker owns the phone and bridge. Completion observes the
    // same pair and durable device setup; registration alone is never ready.
    let boot = manager.track("ai-phone-client --android-session", || {
        native_phone::start(&phone, &registered.distribution, None, &cancelled)
    })?;
    manager.set(folder, "devices", "running", "")?;
    native
        .complete_devices(&registered.distribution, &boot.backend_bridge_id)
        .map_err(|error| error.code().to_string())?;
    manager.complete(folder)
}

#[cfg(unix)]
fn run_native_installer(
    manager: &Arc<ManagerState>,
    folder: &Path,
    resource: &Path,
    request: &Path,
    components: Components,
) -> Result<(), String> {
    let android = components.android.ok_or("android_runtime_missing")?;
    let image_digest = components.image_digest.ok_or("invalid_payload")?;
    let mut command =
        Command::new(std::env::current_exe().map_err(|_| "native_initializer_program")?);
    command
        .arg("--initialize-unix-host")
        .arg(resource)
        .arg(folder)
        .arg(request)
        .arg(components.source)
        .arg(android)
        .arg(image_digest)
        .current_dir(folder);
    #[cfg(target_os = "macos")]
    if checkpoint::same_release(folder)? {
        command.arg("--resume");
    }
    execute(manager, command, folder)
}

pub fn start(app: tauri::AppHandle, request: Request) -> Result<(), String> {
    request.validate()?;
    if !capability(std::env::consts::OS, std::env::consts::ARCH).implemented {
        return Err("host_backend_pending".into());
    }
    let path = home(&app)?;
    if checks(&path).iter().any(|c| !c.passed) {
        return Err("host_resources".into());
    }
    prepare_folder(&path)?;
    let manager = app.state::<Bootstrap>().0.clone();
    let closing = manager.closing.lock().unwrap();
    if *closing {
        return Err("installation_cancelled".into());
    }
    if manager.running.swap(true, Ordering::SeqCst) {
        return Err("installation_running".into());
    }
    manager.cancelled.store(false, Ordering::SeqCst);
    manager.activity.lock().unwrap().clear();
    if let Err(error) = manager.set(&path, "checking", "running", "") {
        manager.running.store(false, Ordering::SeqCst);
        return Err(error);
    }
    drop(closing);
    std::thread::spawn(move || {
        if let Err(error) = run(&app, &manager, &path, request) {
            manager.failed(&path, &error);
        }
        manager.running.store(false, Ordering::SeqCst);
    });
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn same_release(app: &tauri::AppHandle) -> Result<bool, String> {
    checkpoint::same_release(&home(app)?)
}

pub fn installed(app: &tauri::AppHandle) -> Result<bool, String> {
    checkpoint::installed(&home(app)?)
}

pub fn resume_installed(app: &tauri::AppHandle) -> Result<(), String> {
    let folder = home(app)?;
    if !checkpoint::installed(&folder)? {
        return Err("invalid_install_checkpoint".into());
    }
    let request: Request = serde_json::from_slice(
        &fs::read(folder.join("request.json")).map_err(|_| "invalid_install_checkpoint")?,
    )
    .map_err(|_| "invalid_install_checkpoint")?;
    if let Err(error) = start(app.clone(), request) {
        app.state::<Bootstrap>().0.failed(&folder, &error);
        return Err(error);
    }
    Ok(())
}

pub fn startup_result(app: &tauri::AppHandle) -> Result<(), String> {
    let manager = app.state::<Bootstrap>();
    let progress = manager.0.progress.lock().map_err(|_| "installer_state")?;
    match progress.state.as_str() {
        "ready" => Ok(()),
        _ if !progress.error.is_empty() => Err(progress.error.clone()),
        _ => Err("workbench_unavailable".into()),
    }
}

#[cfg(target_os = "macos")]
pub async fn installed_services_ready(app: &tauri::AppHandle, client: &reqwest::Client) -> bool {
    use ai_phone_api::wire::DeviceRequest;
    use prost::Message;

    async fn protobuf(request: reqwest::RequestBuilder) -> bool {
        request
            .header("Content-Type", "application/x-protobuf")
            .send()
            .await
            .is_ok_and(|response| {
                response.status().is_success()
                    && response.headers().get("content-type").is_some_and(|value| {
                        value.as_bytes().starts_with(b"application/x-protobuf")
                    })
            })
    }

    let (Ok(home), Ok(payload)) = (home(app), checkpoint::payload()) else {
        return false;
    };
    let release = home.join("releases").join(payload);
    if checks(&home).iter().any(|check| !check.passed)
        || release.join("data/restore-quarantine.json").exists()
        || ![
            "HOST-MANIFEST.json",
            "runtime/bin/bun",
            "runtime/web/apps/web/server.js",
            "runtime/services/bin/assistant",
            "runtime/services/bin/runner",
            "data/deployment/profile.json",
        ]
        .iter()
        .all(|path| release.join(path).is_file())
    {
        return false;
    }
    let (Ok(assistant), Ok(runner)) = (
        fs::read_to_string(release.join("data/assistant/token")),
        fs::read_to_string(release.join("data/phone-runner/token")),
    ) else {
        return false;
    };
    let phone = DeviceRequest {
        phone_id: "phone-local-01".into(),
        owner_id: "local-user".into(),
        ..Default::default()
    };
    let (database, phone, media) = tokio::join!(
        protobuf(
            client
                .post("http://127.0.0.1:17863/v1/setup/status")
                .bearer_auth(assistant.trim())
                .body(Vec::new()),
        ),
        protobuf(
            client
                .post("http://127.0.0.1:17862/v1/orientation")
                .bearer_auth(runner.trim())
                .body(phone.encode_to_vec()),
        ),
        client
            .get("http://127.0.0.1:17865/status")
            .bearer_auth(runner.trim())
            .send(),
    );
    database && phone && media.is_ok_and(|response| response.status().is_success())
}

pub fn ready(app: &tauri::AppHandle) -> bool {
    app.state::<Bootstrap>()
        .0
        .progress
        .lock()
        .is_ok_and(|progress| progress.state == "ready")
}

pub fn cancel(app: &tauri::AppHandle) {
    app.state::<Bootstrap>()
        .0
        .cancelled
        .store(true, Ordering::SeqCst);
}
pub fn is_running(app: &tauri::AppHandle) -> bool {
    app.state::<Bootstrap>().0.running.load(Ordering::SeqCst)
}
pub fn wait_until_idle(app: &tauri::AppHandle) {
    while is_running(app) {
        std::thread::sleep(Duration::from_millis(150));
    }
}

pub async fn shutdown(app: &tauri::AppHandle) -> Result<(), String> {
    *app.state::<Bootstrap>().0.closing.lock().unwrap() = true;
    cancel(app);
    let waiting = app.clone();
    tauri::async_runtime::spawn_blocking(move || wait_until_idle(&waiting))
        .await
        .map_err(|_| "installer_state")?;
    let folder = home(app)?;
    #[cfg(target_os = "macos")]
    let result = ai_phone_host_runtime::stop_macos_installation(&folder).await;
    #[cfg(windows)]
    let result = ai_phone_host_runtime::qemu_host::stop_installed(&folder).await;
    #[cfg(not(any(target_os = "macos", windows)))]
    let result = Ok::<_, ai_phone_host_runtime::Error>(());
    result.map_err(|error| error.code().into())
}

pub fn accept_license(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<Bootstrap>();
    let mut progress = state.0.progress.lock().map_err(|_| "installer_state")?;
    if progress.state != "license" {
        return Err("no_pending_license".into());
    }
    let mut input = state.0.input.lock().map_err(|_| "installer_state")?;
    let input = input.as_mut().ok_or("no_pending_license")?;
    input
        .write_all(b"{\"accept\":true}\n")
        .and_then(|_| input.flush())
        .map_err(|_| "license_response_failed")?;
    progress.state = "running".into();
    progress.license.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_host_payload_can_start_installation() {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("gen/host");
        verify_bundled_host(&folder).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn installer_failure_reason_survives_the_process_boundary() {
        let folder = std::env::temp_dir().join(format!("ai-phone-reason-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let manager = Arc::new(ManagerState::default());
        let mut command = Command::new("/bin/sh");
        command.args(["-c","printf '%s\\n' 'AI_PHONE_SETUP {\"phase\":\"failed\",\"state\":\"failed\",\"error\":\"foreign_installation\"}'; exit 1"]);
        let result = execute(&manager, command, &folder);
        fs::remove_dir_all(folder).unwrap();
        assert_eq!(
            result,
            Err("foreign_installation".into()),
            "the UI needs the actual recovery reason"
        );
    }
    #[test]
    #[cfg(unix)]
    fn installer_events_are_durable_before_the_client_can_restart() {
        let folder = std::env::temp_dir().join(format!("ai-phone-progress-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let manager = Arc::new(ManagerState::default());
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s\\n' 'AI_PHONE_SETUP {\"phase\":\"dependencies\",\"state\":\"completed\"}'",
        ]);
        execute(&manager, command, &folder).unwrap();
        let checkpoint = folder.join("deployment.json");
        assert!(
            checkpoint.is_file(),
            "closing the app must not erase installation progress"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&checkpoint).unwrap()).unwrap();
        assert_eq!(saved["progress"]["phase"], "dependencies");
        assert_eq!(saved["progress"]["state"], "running");
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn ready_event_from_a_failed_installer_cannot_authorize_auto_resume() {
        let folder =
            std::env::temp_dir().join(format!("ai-phone-failed-ready-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let manager = Arc::new(ManagerState::default());
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s\\n' 'AI_PHONE_SETUP {\"phase\":\"ready\",\"state\":\"ready\"}'; exit 1",
        ]);
        assert!(execute(&manager, command, &folder).is_err());
        assert!(
            !checkpoint::completed(&folder).unwrap(),
            "the process must succeed before installation is complete"
        );
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn only_the_complete_native_mac_bundle_can_enter_device_configuration() {
        let mut resources = BTreeMap::from([("source.payload".into(), "a".repeat(64))]);
        assert!(!capability_from_resources("macos", "aarch64", &resources).implemented);
        resources.insert("android.payload".into(), "b".repeat(64));
        assert!(!capability_from_resources("macos", "aarch64", &resources).implemented);
        resources.insert("images.payload".into(), "c".repeat(64));
        assert!(capability_from_resources("macos", "aarch64", &resources).implemented);
        for (os, arch) in [
            ("macos", "x86_64"),
            ("windows", "x86_64"),
            ("linux", "aarch64"),
            ("linux", "x86_64"),
        ] {
            assert!(!capability_from_resources(os, arch, &resources).implemented);
        }
    }
    #[test]
    fn only_the_complete_native_windows_pair_can_enter_device_configuration() {
        use std::collections::BTreeMap;
        let mut resources = BTreeMap::from([("source.payload".into(), "c".repeat(64))]);
        assert!(!capability_from_resources("windows", "x86_64", &resources).implemented);
        resources.insert("wsl/manifest.json".into(), "a".repeat(64));
        assert!(!capability_from_resources("windows", "x86_64", &resources).implemented);
        resources.insert("android.payload".into(), "b".repeat(64));
        assert!(capability_from_resources("windows", "x86_64", &resources).implemented);
        for (os, arch) in [("windows", "aarch64"), ("linux", "aarch64")] {
            assert!(!capability_from_resources(os, arch, &resources).implemented);
        }
        resources.remove("wsl/manifest.json");
        assert!(!capability_from_resources("windows", "x86_64", &resources).implemented);
    }
    #[test]
    fn native_completion_is_durable_and_cancellation_cannot_publish_ready() {
        let folder = std::env::temp_dir().join(format!(
            "aiphone-native-completion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&folder).unwrap();
        let manager = ManagerState::default();
        manager.set(&folder, "devices", "running", "").unwrap();
        assert!(!checkpoint::completed(&folder).unwrap());
        manager.cancelled.store(true, Ordering::SeqCst);
        assert_eq!(
            manager.complete(&folder),
            Err("installation_cancelled".into())
        );
        assert!(!checkpoint::completed(&folder).unwrap());
        manager.cancelled.store(false, Ordering::SeqCst);
        manager.complete(&folder).unwrap();
        assert!(checkpoint::completed(&folder).unwrap());
        let restarted = ManagerState::default();
        restarted.recover(&folder).unwrap();
        assert_eq!(restarted.progress.lock().unwrap().state, "installed");
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn deployment_contract_rejects_unknown_keys_and_unbounded_hardware() {
        let value = r#"{"locale":"en","phone":{"cpus":2,"memory_mb":2048,"storage_gb":16},"desktop":{"cpus":2,"memory_mb":2560}}"#;
        assert!(
            serde_json::from_str::<Request>(value)
                .unwrap()
                .validate()
                .is_ok()
        );
        for invalid in [
            value.replace("2048", "9999999"),
            value.replace("\"en\"", "\"../../en\""),
            value.replace("\"cpus\":2", "\"cpus\":0"),
        ] {
            assert!(
                serde_json::from_str::<Request>(&invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        assert!(
            serde_json::from_str::<Request>(&value.replace("\"locale\"", "\"command\"")).is_err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_stops_the_installer_process_group_before_returning() {
        let folder = std::env::temp_dir().join(format!(
            "ai-phone-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&folder).unwrap();
        let manager = Arc::new(ManagerState::default());
        let state = manager.clone();
        let mut command = Command::new("/bin/sh");
        command.current_dir(&folder).args([
            "-c",
            "(sleep 3; touch unsafe-replayed) & echo started; wait",
        ]);
        let reader_log = folder.join("install.log");
        let worker_folder = folder.clone();
        let worker = std::thread::spawn(move || execute(&state, command, &worker_folder));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !fs::read_to_string(&reader_log)
            .unwrap_or_default()
            .contains("started")
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.cancelled.store(true, Ordering::SeqCst);
        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            "installation_cancelled"
        );
        assert!(!folder.join("unsafe-replayed").exists());
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn retry_budget_is_not_the_initial_download_reserve() {
        let folder = std::env::temp_dir().join(format!("ai-phone-space-{}", std::process::id()));
        fs::create_dir(&folder).unwrap();
        let initial = checks(&folder)
            .into_iter()
            .find(|c| c.name == "storage")
            .unwrap()
            .required;
        fs::create_dir_all(folder.join("user/data/deployment")).unwrap();
        fs::write(folder.join("user/data/deployment/profile.json"), b"{}").unwrap();
        let resumed = checks(&folder)
            .into_iter()
            .find(|c| c.name == "storage")
            .unwrap()
            .required;
        assert!(resumed < initial);
        fs::remove_file(folder.join("user/data/deployment/profile.json")).unwrap();
        checkpoint::save(
            &folder,
            &Progress {
                phase: "runtime".into(),
                state: "running".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            checks(&folder)
                .into_iter()
                .find(|check| check.name == "storage")
                .unwrap()
                .required,
            resumed,
            "an admitted install interrupted before profile creation must retain its retry reserve"
        );
        fs::remove_dir_all(folder).unwrap();
    }
}
