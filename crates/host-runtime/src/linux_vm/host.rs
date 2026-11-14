//! Windows owner for one persistent Linux guest and its loopback endpoints.
use crate::{Error, Result, filesystem, linux_vm, native_ipc, vm_control};
use ai_phone_api::wire::{LinuxVmBootstrapReply, LinuxVmBootstrapRequest};
use ring::hmac;
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};

const START_LIMIT: Duration = Duration::from_secs(1950);
const STOP_LIMIT: Duration = Duration::from_secs(30);
const PROFILE_FRAME: usize = 8192 + 8;
const HIDDEN: u32 = 0x08000000;
const GUEST_WEB_PORT: u16 = 17860;
const ALUMINUM_BASE: &str = "348baa7207524397b74de603f08357b6d3b73947514eb9f662c6eb4e52a365a5";
const ALUMINUM_BASE_SIZE: u64 = 1_101_660_160;

struct QemuVmConfig<'a> {
    runtime: &'a Path,
    guest: &'a Path,
    home: &'a Path,
    cpu: u16,
    memory_mib: u32,
    web_port: u16,
    control_port: u16,
    base_sha256: &'a str,
    release_sha256: Option<&'a str>,
}

#[derive(serde::Deserialize)]
struct VmManifest {
    format: u8,
    runtime: String,
    files: Vec<VmFile>,
    release_sha256: Option<String>,
}

#[derive(serde::Deserialize)]
struct VmFile {
    path: String,
    size: u64,
    sha256: String,
}

pub fn key_path(home: &Path) -> PathBuf {
    home.join("control.key")
}

impl<'a> QemuVmConfig<'a> {
    fn parse(arguments: &'a [OsString]) -> Result<Self> {
        let [
            runtime,
            guest,
            home,
            cpu,
            memory,
            web,
            control,
            base,
            release,
        ] = arguments
        else {
            return Err(Error::Invalid("vm_owner_arguments"));
        };
        let invalid = || Error::Invalid("vm_owner_arguments");
        let number = |argument: &OsString| -> Result<u32> {
            argument
                .to_str()
                .and_then(|value| value.parse().ok())
                .ok_or_else(invalid)
        };
        Ok(Self {
            runtime: Path::new(runtime),
            guest: Path::new(guest),
            home: Path::new(home),
            cpu: number(cpu)?.try_into().map_err(|_| invalid())?,
            memory_mib: number(memory)?,
            web_port: number(web)?.try_into().map_err(|_| invalid())?,
            control_port: number(control)?.try_into().map_err(|_| invalid())?,
            base_sha256: base.to_str().ok_or_else(invalid)?,
            release_sha256: Some(release.to_str().ok_or_else(invalid)?).filter(|id| !id.is_empty()),
        })
    }

    fn validate(&self, profile: &[u8]) -> Result<()> {
        if ![self.runtime, self.guest, self.home]
            .iter()
            .all(|path| path.is_absolute())
            || self.web_port == 0
            || self.control_port == 0
            || self.web_port == self.control_port
            || self.cpu == 0
            || self.memory_mib == 0
            || !digest(self.base_sha256)
            || self.release_sha256.is_some_and(|id| !digest(id))
        {
            return Err(Error::Invalid("vm_owner_arguments"));
        }
        crate::profile::parse_request(profile)?;
        filesystem::directory(self.home, true)
    }

    fn worker_command(&self) -> Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--qemu-backend-owner")
            .args([self.runtime, self.guest, self.home])
            .args([
                self.cpu.to_string(),
                self.memory_mib.to_string(),
                self.web_port.to_string(),
                self.control_port.to_string(),
            ])
            .arg(self.base_sha256)
            .arg(self.release_sha256.unwrap_or_default())
            .creation_flags(HIDDEN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log(self.home, "owner.log")?);
        Ok(command)
    }

    fn lease(&self) -> Result<Option<filesystem::Lease>> {
        filesystem::lease(&self.home.join("owner.lock"))
    }

    fn existing_disk(&self) -> Result<PathBuf> {
        let disk = self.home.join("disk");
        filesystem::existing_directory(&disk)?;
        let identity = filesystem::bounded_file_with_limit(&disk.join("base.sha256"), 64)?;
        if identity != self.base_sha256.as_bytes() {
            return Err(Error::Invalid("vm_disk_base_changed"));
        }
        let path = disk.join("user.qcow2");
        filesystem::regular(&path)?;
        Ok(path)
    }

    async fn disk(&self, base: &Path) -> Result<PathBuf> {
        let disk = self.home.join("disk");
        if disk.try_exists()? {
            return self.existing_disk();
        }
        let control = crate::control::Control {
            available: &|_| 0,
            cancelled: &|| false,
        };
        if crate::capsule::hash(&mut filesystem::regular(base)?, &control)? != self.base_sha256 {
            return Err(Error::Invalid("vm_disk_base_changed"));
        }
        let staging = tempfile::tempdir_in(self.home)?;
        let image = self.runtime.join("qemu-img.exe");
        filesystem::regular(&image)?;
        let status = Command::new(image)
            .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(base)
            .arg(staging.path().join("user.qcow2"))
            .creation_flags(HIDDEN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log(self.home, "owner.log")?)
            .kill_on_drop(true)
            .status()
            .await?;
        if !status.success() {
            return Err(Error::Invalid("vm_overlay_create"));
        }
        OpenOptions::new()
            .write(true)
            .open(staging.path().join("user.qcow2"))?
            .sync_all()?;
        let mut identity = File::create_new(staging.path().join("base.sha256"))?;
        identity.write_all(self.base_sha256.as_bytes())?;
        identity.sync_all()?;
        drop(identity);
        fs::rename(staging.path(), &disk)?;
        self.existing_disk()
    }
}

fn digest(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|value| value.is_ascii_hexdigit())
}

fn log(home: &Path, name: &str) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join(name))
}

struct QemuVm<'a> {
    child: Child,
    config: QemuVmConfig<'a>,
    _lease: filesystem::Lease,
}

impl<'a> QemuVm<'a> {
    async fn start(
        config: QemuVmConfig<'a>,
        profile: Vec<u8>,
        lease: filesystem::Lease,
    ) -> Result<Self> {
        let web = TcpListener::bind((Ipv4Addr::LOCALHOST, config.web_port)).await?;
        let control = TcpListener::bind((Ipv4Addr::LOCALHOST, config.control_port)).await?;
        let bootstrap = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let bootstrap_port = bootstrap.local_addr()?.port();
        let program = config.runtime.join("qemu-system-x86_64.exe");
        let firmware = config.runtime.join("share");
        let base = config.guest.join("guest.qcow2");
        let kernel = config.guest.join("bzImage");
        for file in [&program, &base, &kernel] {
            filesystem::regular(file)?;
        }
        filesystem::existing_directory(&firmware)?;
        let overlay = config.disk(&base).await?;
        let key = key_path(config.home);
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).map_err(|_| Error::Invalid("vm_bootstrap_key"))?;
        let mut staged_key = tempfile::NamedTempFile::new_in(config.home)?;
        staged_key.write_all(&secret)?;
        staged_key.as_file().sync_all()?;
        staged_key.persist(&key).map_err(|error| error.error)?;
        let secret = hmac::Key::new(hmac::HMAC_SHA256, &secret);
        let block = serde_json::json!({
            "driver": "qcow2", "node-name": "aiphone-disk",
            "file": {"driver": "file", "filename": overlay},
            "backing": {
                "driver": "qcow2", "read-only": true,
                "file": {"driver": "file", "filename": base}
            }
        });
        let network = format!(
            "user,id=network,hostfwd=tcp:127.0.0.1:{}-10.0.2.15:{},hostfwd=tcp:127.0.0.1:{}-10.0.2.15:{},hostfwd=tcp:127.0.0.1:17906-10.0.2.15:17906",
            config.web_port,
            GUEST_WEB_PORT,
            config.control_port,
            vm_control::GUEST_PORT,
        );
        // The key filename is relative to QEMU's working directory, so paths
        // with commas cannot become extra -fw_cfg options.
        let mut command = Command::new(program);
        command
            .args([
                "-machine",
                "q35,accel=whpx",
                "-cpu",
                "Nehalem",
                "-nodefaults",
                "-display",
                "none",
                "-no-reboot",
            ])
            .args([
                "-smp",
                &config.cpu.to_string(),
                "-m",
                &config.memory_mib.to_string(),
            ])
            .arg("-L")
            .arg(firmware)
            .arg("-kernel")
            .arg(kernel)
            .args(["-append", "root=/dev/vda rw net.ifnames=0 console=ttyS0"])
            .args([
                "-blockdev",
                &block.to_string(),
                "-device",
                "virtio-blk-pci,drive=aiphone-disk",
            ])
            .args([
                "-netdev",
                &network,
                "-device",
                "virtio-net-pci,netdev=network,romfile=",
            ])
            .args([
                "-chardev",
                &format!("socket,id=bootstrap,host=127.0.0.1,port={bootstrap_port}"),
            ])
            .args([
                "-device",
                "virtio-serial-pci",
                "-device",
                "virtserialport,chardev=bootstrap,name=org.aiphone.bootstrap",
            ])
            .args([
                "-fw_cfg",
                "name=opt/aiphone/control-key,file=control.key",
                "-serial",
                "file:boot.log",
            ])
            .current_dir(config.home)
            .creation_flags(HIDDEN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log(config.home, "qemu.log")?)
            .kill_on_drop(true);
        if config.release_sha256.is_some() {
            let initramfs = config.guest.join("release-init.cpio.gz");
            let release = config.guest.join("release.ext4");
            for path in [&initramfs, &release] {
                filesystem::regular(path)?;
            }
            let layer = serde_json::json!({
                "driver": "raw", "node-name": "aiphone-release", "read-only": true,
                "file": {"driver": "file", "filename": release}
            });
            command.arg("-initrd").arg(initramfs).args([
                "-blockdev",
                &layer.to_string(),
                "-device",
                "virtio-blk-pci,drive=aiphone-release",
            ]);
        }
        drop((web, control));
        let mut child = command.spawn()?;
        tokio::select! {
            result = linux_vm::authenticate(bootstrap, &secret, profile) => result?,
            status = child.wait() => { status?; return Err(Error::Invalid("vm_process_exited")); }
        }
        let vm = Self {
            child,
            config,
            _lease: lease,
        };
        let active = vm_control::version(vm.config.control_port, &key).await?;
        if vm
            .config
            .release_sha256
            .is_some_and(|id| active.as_slice() != id.as_bytes())
        {
            vm.stop().await?;
            return Err(Error::Invalid("vm_release_identity"));
        }
        Ok(vm)
    }

    async fn wait(&mut self) -> Result<()> {
        if !self.child.wait().await?.success() {
            return Err(Error::Invalid("vm_process_exited"));
        }
        Ok(())
    }

    async fn stop(mut self) -> Result<()> {
        vm_control::shutdown(self.config.control_port, &key_path(self.config.home)).await?;
        let status = timeout(STOP_LIMIT, self.child.wait())
            .await
            .map_err(|_| Error::Invalid("vm_stop_unknown"))??;
        if !status.success() {
            return Err(Error::Invalid("vm_stop_unknown"));
        }
        Ok(())
    }
}

async fn cancelled(test: &dyn Fn() -> bool) {
    while !test() {
        sleep(Duration::from_millis(100)).await;
    }
}

pub async fn stop_installed(home: &Path) -> Result<()> {
    if !home.try_exists()? {
        return Ok(());
    }
    let home = home.canonicalize()?;
    let flush = crate::android::session::flush_attached(&home).await;
    let qemu = home.join("user/qemu");
    if qemu.try_exists()? {
        stop_running(&qemu, vm_control::GUEST_PORT).await?;
    }
    let android = home.join("user/data/android/native/session.lock");
    if android.try_exists()? {
        wait_for_stop(&android).await?;
    }
    flush
}

async fn stop_running(home: &Path, port: u16) -> Result<()> {
    let lock = home.join("owner.lock");
    if filesystem::lease(&lock)?.is_some() {
        return Ok(());
    }
    vm_control::shutdown(port, &key_path(home)).await?;
    wait_for_stop(&lock).await
}

async fn wait_for_stop(lock: &Path) -> Result<()> {
    timeout(STOP_LIMIT, async {
        while filesystem::lease(lock)?.is_none() {
            sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| Error::Invalid("vm_stop_unknown"))?
}

pub async fn ensure(
    bundle: &Path,
    home: &Path,
    cpu: u16,
    memory_mib: u32,
    profile: &[u8],
    cancel: &dyn Fn() -> bool,
    activity: impl Fn(bool),
) -> Result<()> {
    let manifest: VmManifest = serde_json::from_reader(File::open(bundle.join("manifest.json"))?)
        .map_err(|_| Error::Invalid("vm_bundle_invalid"))?;
    if manifest.format != 1 || manifest.runtime != "qemu-whpx-backend" {
        return Err(Error::Invalid("vm_bundle_invalid"));
    }
    let file = |name: &str| {
        let record = manifest
            .files
            .iter()
            .find(|file| file.path == name)
            .ok_or(Error::Invalid("vm_bundle_invalid"))?;
        let path = bundle.join(name);
        if fs::metadata(&path)?.len() != record.size {
            return Err(Error::Invalid("vm_bundle_changed"));
        }
        Ok((path, &record.sha256))
    };
    let base_record = manifest
        .files
        .iter()
        .find(|value| value.path == "guest/guest.qcow2")
        .ok_or(Error::Invalid("vm_bundle_invalid"))?;
    let base_size = filesystem::regular(&bundle.join("guest/guest.qcow2"))?
        .metadata()?
        .len();
    let base_sha256 = match base_size {
        size if size == base_record.size => base_record.sha256.as_str(),
        ALUMINUM_BASE_SIZE => ALUMINUM_BASE,
        _ => return Err(Error::Invalid("vm_bundle_changed")),
    };
    let config = QemuVmConfig {
        runtime: &bundle.join("runtime"),
        guest: &bundle.join("guest"),
        home,
        cpu,
        memory_mib,
        web_port: GUEST_WEB_PORT,
        control_port: vm_control::GUEST_PORT,
        base_sha256,
        release_sha256: manifest.release_sha256.as_deref(),
    };
    config.validate(profile)?;
    let key = key_path(config.home);
    let deadline = Instant::now() + START_LIMIT;
    let replace = loop {
        if cancel() {
            return Err(Error::Cancelled);
        }
        if config.lease()?.is_some() {
            break false;
        }
        if let Ok(active) = vm_control::version(config.control_port, &key).await {
            config.existing_disk()?;
            if config
                .release_sha256
                .is_none_or(|id| active.as_slice() == id.as_bytes())
            {
                return Ok(());
            }
            break true;
        }
        if Instant::now() >= deadline {
            return Err(Error::Invalid("vm_owner_start_timeout"));
        }
        sleep(Duration::from_millis(500)).await;
    };
    if config.release_sha256.is_some() {
        let control = crate::control::Control {
            available: &|_| 0,
            cancelled: cancel,
        };
        for name in ["guest/release-init.cpio.gz", "guest/release.ext4"] {
            let (path, expected) = file(name)?;
            if crate::capsule::hash(&mut File::open(path)?, &control)? != *expected {
                return Err(Error::Invalid("vm_bundle_changed"));
            }
        }
    }
    if replace {
        stop_running(config.home, config.control_port).await?;
    }
    let mut child = config.worker_command()?.spawn()?;
    activity(true);
    let mut input = child.stdin.take().ok_or(Error::Invalid("vm_owner_pipe"))?;
    let mut output = child.stdout.take().ok_or(Error::Invalid("vm_owner_pipe"))?;
    native_ipc::write_bounded(
        &mut input,
        &LinuxVmBootstrapRequest {
            profile: profile.to_vec(),
            ..Default::default()
        },
        PROFILE_FRAME,
    )
    .await?;
    let response = tokio::select! { biased;
        _ = cancelled(cancel) => Err(Error::Cancelled),
        result = timeout(START_LIMIT, native_ipc::read::<LinuxVmBootstrapReply>(&mut output)) =>
            result.map_err(|_| Error::Invalid("vm_owner_start_timeout")),
    };
    let reply = match response {
        Ok(reply) => reply?,
        Err(error) => {
            input.write_u8(1).await?;
            input.flush().await?;
            drop(input);
            timeout(STOP_LIMIT + Duration::from_secs(15), child.wait())
                .await
                .map_err(|_| Error::Invalid("vm_owner_stop_unknown"))??;
            return Err(error);
        }
    };
    if !reply.error.is_empty() {
        eprintln!("QEMU backend startup: {}", reply.error);
        return Err(Error::Invalid("vm_owner_start_failed"));
    }
    activity(false);
    Ok(())
}

pub async fn worker(arguments: &[OsString]) -> Result<()> {
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let admission = async {
        let config = QemuVmConfig::parse(arguments)?;
        let request: LinuxVmBootstrapRequest =
            native_ipc::read_bounded(&mut input, PROFILE_FRAME).await?;
        config.validate(&request.profile)?;
        let lease = config.lease()?.ok_or(Error::Busy)?;
        Ok::<_, Error>((config, request.profile, lease))
    }
    .await;
    let cancel = async {
        if input.read_u8().await.is_ok_and(|byte| byte == 1) {
            return;
        }
        std::future::pending::<()>().await;
    };
    tokio::pin!(cancel);
    let start = match admission {
        Ok((config, profile, lease)) => tokio::select! { biased;
            _ = &mut cancel => Err(Error::Cancelled),
            result = QemuVm::start(config, profile, lease) => result,
        },
        Err(error) => Err(error),
    };
    let mut vm = match start {
        Ok(vm) => vm,
        Err(error) => {
            let reply = LinuxVmBootstrapReply {
                error: error.code().into(),
                ..Default::default()
            };
            let _ = native_ipc::write(&mut output, &reply).await;
            return Err(error);
        }
    };
    let result = native_ipc::write(&mut output, &LinuxVmBootstrapReply::default()).await;
    if let Err(error) = result {
        vm.stop().await?;
        return Err(error.into());
    }
    tokio::select! {
        _ = &mut cancel => vm.stop().await,
        result = vm.wait() => result,
    }
}
