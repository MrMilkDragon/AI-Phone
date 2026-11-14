//! The existing native phone owner outlives the installer, just as it outlives a viewport.
use crate::{Error, Result, native_ipc};
use ai_phone_api::wire::{
    NativeAndroidBooted, NativeAndroidSessionControl, NativeAndroidSessionReply,
    NativeAndroidStart, native_android_session_reply::Result as Reply,
};
use std::{process::Stdio, time::Duration};
use tokio::{process::Command, sync::watch};

#[cfg(target_os = "macos")]
pub(super) async fn shutdown(home: &std::path::Path) -> Result<()> {
    use crate::filesystem;
    use rustix::process::{Pid, Signal, kill_process};
    let lock = home.join("user/data/android/native/session.lock");
    if !lock.try_exists()? || filesystem::lease(&lock)?.is_some() {
        return Ok(());
    }
    let flush = crate::android::session::flush_attached(home).await;
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-Fpc", "--"])
        .arg(&lock)
        .output()
        .await?;
    if filesystem::lease(&lock)?.is_some() {
        return flush;
    }
    let body = String::from_utf8_lossy(&output.stdout);
    let mut lines = body.lines();
    let Some(value) = lines.next().and_then(|pid| pid.strip_prefix('p')) else {
        return Err(Error::Invalid("android_session_stop_owner"));
    };
    let pid = value
        .parse()
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(Error::Invalid("android_session_stop_owner"))?;
    if lines.next() != Some("cai-phone-client") || lines.any(|line| line.starts_with('p')) {
        return Err(Error::Invalid("android_session_stop_owner"));
    }
    let process = Command::new("/bin/ps")
        .args(["-ww", "-p", value, "-o", "command="])
        .output()
        .await?;
    if !String::from_utf8_lossy(&process.stdout)
        .trim_end()
        .ends_with("ai-phone-client --android-session")
    {
        return Err(Error::Invalid("android_session_stop_owner"));
    }
    match kill_process(pid, Signal::TERM) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => return Err(std::io::Error::from(error).into()),
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while filesystem::lease(&lock)?.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok::<(), Error>(())
    })
    .await
    .map_err(|_| Error::Invalid("android_emulator_stop_unknown"))??;
    flush
}

pub(super) async fn start(
    request: NativeAndroidStart,
    mut stop: watch::Receiver<bool>,
) -> Result<NativeAndroidBooted> {
    if *stop.borrow() {
        return Err(Error::Cancelled);
    }
    let phone = request
        .phone
        .as_ref()
        .ok_or(Error::Invalid("android_session_phone"))?;
    let serial =
        crate::android::Engine::installed(std::path::Path::new(&phone.sdk_root)).serial(5564);
    super::activity("ai-phone-client --android-session", "running");
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--android-session")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or(Error::Invalid("android_session_outcome_unknown"))?;
    let mut output = child
        .stdout
        .take()
        .ok_or(Error::Invalid("android_session_outcome_unknown"))?;
    native_ipc::write_bounded(&mut input, &request, 16384).await?;
    let receipt = tokio::select! {
        value=tokio::time::timeout(Duration::from_secs(150),native_ipc::read_bounded::<NativeAndroidSessionReply>(&mut output,512))=>value.map_err(|_|Error::Invalid("android_session_outcome_unknown"))?,
        _=stop.changed()=>{
            native_ipc::write_bounded(&mut input,&NativeAndroidSessionControl{cancel_startup:true},512).await?;
            tokio::time::timeout(Duration::from_secs(20),native_ipc::read_bounded::<NativeAndroidSessionReply>(&mut output,512)).await.map_err(|_|Error::Invalid("android_session_outcome_unknown"))?
        }
    }?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let boot = match receipt.result {
        Some(Reply::Booted(value))
            if value.version == 1
                && value.root_verified
                && value.serial == serial
                && value.width > 0
                && value.height > 0 =>
        {
            if *stop.borrow() {
                Err(Error::Cancelled)
            } else {
                Ok(value)
            }
        }
        Some(Reply::Failure(_)) => Err(Error::Invalid("android_session_start_failed")),
        _ => Err(Error::Invalid("android_session_outcome_unknown")),
    };
    super::activity(
        "ai-phone-client --android-session",
        if boot.is_ok() { "completed" } else { "failed" },
    );
    boot
}
