//! Fixed native SDK lifecycle, independent of device focus and model execution.
mod adb;
#[cfg(target_os = "macos")]
mod avo;
mod cli;
mod link;
mod plan;
mod process;
mod readiness;
#[cfg(windows)]
mod windows_cuttlefish;
#[cfg(target_os = "macos")]
pub use avo::runtime as avo_runtime;
pub use cli::run;
use std::time::Duration;
pub const CONTRACT: &str = "ai-phone-native-android-session-v1";

#[cfg(any(target_os = "macos", windows))]
pub(crate) async fn flush_attached(home: &std::path::Path) -> crate::Result<()> {
    use crate::{Error, filesystem};
    let dir = home.join("user/data/android");
    let native = dir.join("native");
    let Some(owner) = process::session_record(&native.join("session.pb"))? else {
        return Ok(());
    };
    if filesystem::lease(&native.join("session.lock"))?.is_some() {
        return Ok(());
    }
    use std::path::Path;
    let sdk = Path::new(&owner.sdk_root);
    let engine = super::Engine::installed(sdk);
    if owner.version != 1
        || !sdk.starts_with(home.join("components/android"))
        || Path::new(&owner.avd_home) != dir.join(engine.avd_directory())
    {
        return Err(Error::Invalid("android_session_owner"));
    }
    let port =
        u16::try_from(owner.adb_port).map_err(|_| Error::Invalid("android_session_ports"))?;
    let console =
        u16::try_from(owner.console_port).map_err(|_| Error::Invalid("android_session_ports"))?;
    adb::Adb {
        port,
        serial: engine.serial(console),
    }
    .sync()
    .await
}
pub(crate) async fn wait_serial(
    serial: String,
    observe_only: bool,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> crate::Result<ai_phone_api::wire::NativeAndroidBooted> {
    let adb = adb::Adb {
        port: 15037,
        serial,
    };
    if observe_only {
        return readiness::existing(&adb, stop, Duration::from_secs(120)).await;
    }
    let until = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut root_requested = false;
    loop {
        readiness::cancelled(stop)?;
        if let Some(value) = process::boot_probe(&adb, &mut root_requested).await? {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= until {
            return Err(crate::Error::Invalid("android_session_unavailable"));
        }
        readiness::pause(stop).await?;
    }
}

pub(crate) async fn backend_observation(
    serial: &str,
) -> crate::Result<ai_phone_api::wire::NativeAndroidBooted> {
    let (_owner, mut stop) = tokio::sync::watch::channel(false);
    readiness::existing(
        &adb::Adb {
            port: 15037,
            serial: serial.into(),
        },
        &mut stop,
        Duration::from_secs(10),
    )
    .await
}

#[cfg(test)]
mod tests;
