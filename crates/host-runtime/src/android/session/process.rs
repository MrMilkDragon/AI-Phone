use super::{adb::Adb, plan::Plan, readiness};
use crate::android::Engine;
use crate::{Error, Result, filesystem};
use ai_phone_api::wire::{NativeAndroidBooted, NativeAndroidSessionOwner};
use prost::Message;
use std::{fs::File, io::Write, path::Path, process::Stdio, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::watch,
    time::Instant,
};

pub(super) enum Started {
    Existing(NativeAndroidBooted),
    Owned(Box<Phone>),
}
pub(super) struct Phone {
    plan: Plan,
    _lock: filesystem::Lease,
    log: File,
    adb: Option<Child>,
    emulator: Option<Child>,
    pub receipt: Option<NativeAndroidBooted>,
    link: Option<crate::device_bridge::Connection>,
    #[cfg(target_os = "macos")]
    relay: Option<super::avo::Relay>,
}
fn owner_path(plan: &Plan) -> std::path::PathBuf {
    plan.home.join("session.pb")
}
pub(super) fn session_record(path: &Path) -> Result<Option<NativeAndroidSessionOwner>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let value = NativeAndroidSessionOwner::decode(
        filesystem::bounded_file_with_limit(path, 16384)?.as_slice(),
    )
    .map_err(|_| Error::Invalid("android_session_owner"))?;
    Ok(Some(value))
}
fn read_owner(plan: &Plan) -> Result<Option<NativeAndroidSessionOwner>> {
    let value = session_record(&owner_path(plan))?;
    if value.as_ref().is_some_and(|record| record != &plan.owner()) {
        return Err(Error::Invalid("android_session_conflict"));
    }
    Ok(value)
}
fn save_owner(plan: &Plan) -> Result<()> {
    let mut out = tempfile::NamedTempFile::new_in(&plan.home)?;
    out.write_all(&plan.owner().encode_to_vec())?;
    out.as_file().sync_all()?;
    out.persist(owner_path(plan))
        .map_err(|e| Error::Io(e.error))?;
    filesystem::sync_directory(&plan.home)
}
fn client(plan: &Plan) -> Adb {
    Adb {
        port: plan.ports.adb,
        serial: plan.serial(),
    }
}
fn spawn(mut command: Command, log: &File) -> Result<Child> {
    command
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    Ok(command.spawn()?)
}
fn alive(child: &mut Option<Child>, reason: &'static str) -> Result<()> {
    if child
        .as_mut()
        .ok_or(Error::Invalid(reason))?
        .try_wait()?
        .is_some()
    {
        return Err(Error::Invalid(reason));
    }
    Ok(())
}
fn space(plan: &Plan) -> Result<()> {
    let images = plan.sdk.join(plan.engine.sdk_images(plan.abi));
    let allocation = match plan.engine {
        Engine::Goldfish => {
            let disk = plan.avd.join("phone-local-01.avd/userdata-qemu.img");
            let data = if disk.try_exists()? {
                0
            } else {
                u64::from(plan.resources.storage_gb) << 30
            };
            data + std::fs::metadata(images.join("system.img"))?.len()
        }
        Engine::Avo if !plan.avd.join("cuttlefish/super.img").try_exists()? => {
            std::fs::metadata(images.join("super.img"))?.len()
        }
        Engine::Avo | Engine::Whpx => 0,
    };
    let needed = crate::control::RESERVE + allocation;
    if fs2::available_space(&plan.home)? < needed {
        return Err(Error::LowSpace);
    }
    Ok(())
}
pub(super) async fn start(plan: Plan, stop: &mut watch::Receiver<bool>) -> Result<Started> {
    readiness::cancelled(stop)?;
    if let Some(super::plan::Backend::Wsl(distribution)) = &plan.backend {
        super::link::ensure_wsl(distribution, stop).await?;
    }
    let lease = filesystem::lease(&plan.home.join("session.lock"))?;
    let owner = read_owner(&plan)?;
    let Some(lease) = lease else {
        owner.ok_or(Error::Invalid("android_session_starting"))?;
        let receipt = if plan.backend.is_none() {
            readiness::existing(&client(&plan), stop, Duration::from_secs(120)).await?
        } else {
            super::link::observe(&plan, stop).await?
        };
        return Ok(Started::Existing(receipt));
    };
    let mut phone = Phone::new(plan, lease)?;
    if let Err(error) = phone.launch(stop).await {
        if let Err(cleanup) = phone.shutdown().await {
            eprintln!("android_start_cleanup: {}: {:?}", error.code(), cleanup);
        }
        return Err(error);
    }
    Ok(Started::Owned(Box::new(phone)))
}
impl Phone {
    fn new(plan: Plan, lease: filesystem::Lease) -> Result<Self> {
        let reservations = plan.ports.reserve(plan.engine)?;
        space(&plan)?;
        let log = plan.log()?;
        save_owner(&plan)?;
        let value = Self {
            plan,
            _lock: lease,
            log,
            adb: None,
            emulator: None,
            receipt: None,
            link: None,
            #[cfg(target_os = "macos")]
            relay: None,
        };
        drop(reservations);
        Ok(value)
    }
    async fn launch(&mut self, stop: &mut watch::Receiver<bool>) -> Result<()> {
        #[cfg(windows)]
        if self.plan.engine == Engine::Whpx {
            self.plan.prepare_adb_keys(&self.log).await?;
        }
        self.adb = Some(spawn(self.plan.adb_server()?, &self.log)?);
        await_server(&mut self.adb, &client(&self.plan), stop).await?;
        #[cfg(windows)]
        let mut console = None;
        let command = match self.plan.engine {
            Engine::Goldfish => self.plan.emulator()?,
            #[cfg(target_os = "macos")]
            Engine::Avo => {
                self.relay = Some(super::avo::Relay::start(self.plan.ports.console)?);
                super::avo::command(&self.plan)?
            }
            #[cfg(windows)]
            Engine::Whpx => {
                let listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
                let port = listener.local_addr()?.port();
                console = Some(listener);
                super::windows_cuttlefish::command(&self.plan, port).await?
            }
            _ => return Err(Error::Invalid("android_host_unsupported")),
        };
        self.emulator = Some(spawn(command, &self.log)?);
        #[cfg(windows)]
        if let Some(console) = console {
            let emulator = self
                .emulator
                .as_mut()
                .expect("Android emulator was just spawned");
            tokio::select! {
                provision = tokio::time::timeout(
                    Duration::from_secs(120),
                    super::windows_cuttlefish::provision_adb(&self.plan, console),
                ) => provision.map_err(|_| Error::Invalid("android_console_timeout"))??,
                _ = emulator.wait() => return Err(Error::Invalid("android_emulator_exited")),
                _ = stop.changed() => return Err(Error::Cancelled),
            }
        }
        self.receipt = Some(self.wait_boot(stop).await?);
        #[cfg(windows)]
        if self.plan.engine == Engine::Whpx {
            super::windows_cuttlefish::stage_audio(&self.plan).await?;
        }
        if self.plan.backend.is_some() {
            let (link, receipt) = super::link::start(&self.plan, stop).await?;
            self.link = Some(link);
            self.receipt = Some(receipt);
        }
        Ok(())
    }
    async fn wait_boot(&mut self, stop: &mut watch::Receiver<bool>) -> Result<NativeAndroidBooted> {
        let until = Instant::now() + Duration::from_secs(120);
        let adb = client(&self.plan);
        let mut attached = self.plan.engine == Engine::Goldfish;
        let mut root_requested = false;
        loop {
            alive(&mut self.adb, "android_adb_server_exited")?;
            alive(&mut self.emulator, "android_emulator_exited")?;
            readiness::cancelled(stop)?;
            if !attached {
                #[cfg(target_os = "macos")]
                let relay_ready = self.plan.engine != Engine::Avo
                    || super::avo::ready(self.plan.ports.console).await;
                #[cfg(not(target_os = "macos"))]
                let relay_ready = true;
                attached = relay_ready && adb.attach().await.is_ok_and(|attached| attached);
            }
            if attached && let Some(value) = boot_probe(&adb, &mut root_requested).await? {
                return Ok(value);
            }
            if Instant::now() >= until {
                return Err(Error::Invalid("android_boot_timeout"));
            }
            readiness::pause(stop).await?;
        }
    }
    pub async fn wait(&mut self, stop: &mut watch::Receiver<bool>) -> Result<()> {
        let adb = self
            .adb
            .as_mut()
            .ok_or(Error::Invalid("android_adb_server_exited"))?;
        let emulator = self
            .emulator
            .as_mut()
            .ok_or(Error::Invalid("android_emulator_exited"))?;
        if *stop.borrow() {
            return Ok(());
        }
        let device = async {
            tokio::select! {
                _=stop.changed()=>Ok(()),
                status=adb.wait()=>{status?;Err(Error::Invalid("android_adb_server_exited"))},
                status=emulator.wait()=>if status?.success(){Ok(())}else{Err(Error::Invalid("android_emulator_exited"))},
            }
        };
        if let Some(link) = self.link.as_mut() {
            return tokio::select! {result=device=>result,result=link.wait()=>result};
        }
        #[cfg(target_os = "macos")]
        if let Some(relay) = self.relay.as_mut() {
            return tokio::select! {result=device=>result,result=relay.exited()=>result};
        }
        device.await
    }
    async fn stop_emulator(&mut self) -> Result<()> {
        let Some(emulator) = self.emulator.as_mut() else {
            return Ok(());
        };
        match self.plan.engine {
            #[cfg(target_os = "macos")]
            Engine::Avo => return super::avo::shutdown(emulator).await,
            #[cfg(windows)]
            Engine::Whpx => return super::windows_cuttlefish::shutdown(&self.plan, emulator).await,
            _ => {}
        }
        let until = Instant::now() + Duration::from_secs(15);
        if !console_ready(emulator, self.plan.ports.console, until).await? {
            return Ok(());
        }
        let command = self.plan.stop().map_err(|_| stop_unknown())?;
        let mut stop = spawn(command, &self.log).map_err(|_| stop_unknown())?;
        if tokio::time::timeout_at(
            (Instant::now() + Duration::from_secs(5)).min(until),
            stop.wait(),
        )
        .await
        .is_err()
        {
            let _ = stop.kill().await;
            let _ = stop.wait().await;
        }
        match tokio::time::timeout_at(until, emulator.wait()).await {
            Ok(result) => {
                result.map_err(|_| stop_unknown())?;
                Ok(())
            }
            Err(_) => Err(stop_unknown()),
        }
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(link) = self.link.as_mut() {
            link.shutdown().await?;
        }
        self.link = None;
        self.stop_emulator().await?;
        stop_adb(&mut self.adb).await?;
        #[cfg(target_os = "macos")]
        {
            self.relay = None;
        }
        match std::fs::remove_file(owner_path(&self.plan)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        filesystem::sync_directory(Path::new(&self.plan.home))
    }
}

async fn stop_adb(adb: &mut Option<Child>) -> Result<()> {
    if let Some(adb) = adb.as_mut() {
        if adb
            .try_wait()
            .map_err(|_| Error::Invalid("android_adb_stop_unknown"))?
            .is_none()
        {
            adb.kill()
                .await
                .map_err(|_| Error::Invalid("android_adb_stop_unknown"))?;
        }
        adb.wait()
            .await
            .map_err(|_| Error::Invalid("android_adb_stop_unknown"))?;
    }
    Ok(())
}

async fn await_server(
    child: &mut Option<Child>,
    adb: &Adb,
    stop: &mut watch::Receiver<bool>,
) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        alive(child, "android_adb_server_exited")?;
        readiness::cancelled(stop)?;
        if adb.server().await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(Error::Invalid("android_adb_server_timeout"));
        }
        readiness::pause(stop).await?;
    }
}

pub(super) async fn boot_probe(
    adb: &Adb,
    root_requested: &mut bool,
) -> Result<Option<NativeAndroidBooted>> {
    if adb.boot_completed().await.unwrap_or(false)
        && let Ok(uid) = adb.uid().await
    {
        if uid == 0 {
            return Ok(readiness::snapshot(adb, false).await.ok());
        }
        if !*root_requested {
            *root_requested = true;
            if let Err(error) = adb.root().await
                && error.code() == "android_root_unavailable"
            {
                return Err(error);
            }
        }
    }
    Ok(None)
}

fn stop_unknown() -> Error {
    Error::Invalid("android_emulator_stop_unknown")
}

// The process exists before its authenticated console. Cancellation in that
// interval must wait for the endpoint, then issue one stop within the same budget.
async fn console_ready(child: &mut Child, port: u16, until: Instant) -> Result<bool> {
    loop {
        if child.try_wait().map_err(|_| stop_unknown())?.is_some() {
            return Ok(false);
        }
        if matches!(
            tokio::time::timeout(
                Duration::from_millis(200),
                tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            )
            .await,
            Ok(Ok(_))
        ) {
            return Ok(true);
        }
        if Instant::now() >= until {
            return Err(stop_unknown());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
