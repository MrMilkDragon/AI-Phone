use super::{Engine, Resources};
use crate::{Error, Result, control::Control, filesystem};
use ai_phone_api::wire::NativeAndroidCreation;
use prost::Message;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const DIRECTORY: &str = "phone-local-01.avd";
const RECORD: &str = "ai_phone_p2.ini";
const JOURNAL: &str = ".create.pb";

pub(super) fn text(path: &Path) -> Result<String> {
    let value = path.to_str().ok_or(Error::Invalid("android_device_path"))?;
    if value.chars().any(char::is_control) {
        return Err(Error::Invalid("android_device_path"));
    }
    let normalized = value.replace('\\', "/");
    #[cfg(windows)]
    let normalized = if let Some(unc) = normalized.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        normalized
            .strip_prefix("//?/")
            .unwrap_or(&normalized)
            .to_owned()
    };
    Ok(normalized)
}
fn config(sdk: &Path, abi: &str, resources: &Resources) -> Result<Vec<u8>> {
    let image = text(&sdk.join(Engine::installed(sdk).sdk_images(abi)))?;
    let arch = if abi == "arm64-v8a" {
        "arm64"
    } else {
        "x86_64"
    };
    Ok(format!("avd.ini.encoding=UTF-8\nabi.type={abi}\nhw.cpu.arch={arch}\nimage.sysdir.1={image}/\ntag.id=default\ntag.display=Default\nhw.lcd.width=720\nhw.lcd.height=1440\nhw.lcd.density=320\nhw.keyboard=yes\nhw.gpu.enabled=yes\nhw.gpu.mode=host\nhw.audioInput=yes\nhw.audioOutput=yes\nhw.gps=yes\nhw.accelerometer=yes\nhw.gyroscope=yes\nhw.battery=yes\nhw.camera.back=none\nhw.camera.front=none\nshowDeviceFrame=no\nPlayStore.enabled=no\nfastboot.forceColdBoot=yes\nfastboot.forceFastBoot=no\nhw.cpu.ncore={}\nhw.ramSize={}\ndisk.dataPartition.size={}G\n",resources.cpus,resources.memory_mb,resources.storage_gb).into_bytes())
}
fn ini(path: &Path) -> Result<BTreeMap<String, String>> {
    let bytes = filesystem::bounded_file_with_limit(path, 65536)?;
    let value =
        String::from_utf8(bytes).map_err(|_| Error::Invalid("android_device_configuration"))?;
    let mut values = BTreeMap::new();
    for line in value
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
    {
        let (key, value) = line
            .split_once('=')
            .ok_or(Error::Invalid("android_device_configuration"))?;
        if values
            .insert(key.trim().into(), value.trim().into())
            .is_some()
        {
            return Err(Error::Invalid("android_device_configuration"));
        }
    }
    Ok(values)
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn verify_existing(home: &Path, sdk: &Path, abi: &str) -> Result<()> {
    let folder = home.join(DIRECTORY);
    filesystem::existing_directory(&folder)?;
    verify_record(home, &folder, Engine::installed(sdk))?;
    verify_image(&folder, sdk, abi)
}

pub(super) fn applied(home: &Path) -> Result<Resources> {
    let values = ini(&home.join(DIRECTORY).join("config.ini"))?;
    let number = |key: &str| {
        values
            .get(key)
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or(Error::Invalid("android_device_configuration"))
    };
    let storage = values
        .get("disk.dataPartition.size")
        .and_then(|v| v.strip_suffix('G'))
        .and_then(|v| v.parse().ok())
        .ok_or(Error::Invalid("android_device_configuration"))?;
    let resources = Resources {
        cpus: number("hw.cpu.ncore")?,
        memory_mb: number("hw.ramSize")?,
        storage_gb: storage,
    };
    resources.validate()?;
    Ok(resources)
}
fn verify_record(home: &Path, folder: &Path, engine: Engine) -> Result<()> {
    let record = ini(&home.join(RECORD))?;
    let path = record
        .get("path")
        .ok_or(Error::Invalid("android_device_record"))?;
    if Path::new(path).canonicalize()? != folder
        || record.get("target") != Some(&format!("android-{}", engine.api()))
    {
        return Err(Error::Invalid("android_device_record"));
    }
    Ok(())
}
fn verify_image(folder: &Path, sdk: &Path, abi: &str) -> Result<()> {
    let values = ini(&folder.join("config.ini"))?;
    let image = values
        .get("image.sysdir.1")
        .ok_or(Error::Invalid("android_device_configuration"))?;
    let selected = sdk.join(Engine::installed(sdk).sdk_images(abi));
    if Path::new(image).canonicalize()? != selected
        || values.get("abi.type").map(String::as_str) != Some(abi)
    {
        return Err(Error::Invalid("android_device_image_change"));
    }
    Ok(())
}
fn atomic(home: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut output = tempfile::NamedTempFile::new_in(home)?;
    output.write_all(bytes)?;
    output.as_file().sync_all()?;
    output
        .persist_noclobber(home.join(name))
        .map_err(|e| Error::Io(e.error))?;
    filesystem::sync_directory(home)
}
fn read_intent(path: &Path, sdk: &Path) -> Result<Resources> {
    let value =
        NativeAndroidCreation::decode(filesystem::bounded_file_with_limit(path, 8192)?.as_slice())
            .map_err(|_| Error::Invalid("android_device_creation"))?;
    if value.version != 1 || value.sdk_root != text(sdk)? {
        return Err(Error::Invalid("android_device_creation"));
    }
    let resources = Resources {
        cpus: value.cpus,
        memory_mb: value.memory_mb,
        storage_gb: value.storage_gb,
    };
    resources.validate()?;
    Ok(resources)
}
fn intent(home: &Path, sdk: &Path, resources: Resources) -> Result<Resources> {
    let path = home.join(JOURNAL);
    if exists(&path)? {
        return read_intent(&path, sdk);
    }
    let value = NativeAndroidCreation {
        version: 1,
        sdk_root: text(sdk)?,
        cpus: resources.cpus,
        memory_mb: resources.memory_mb,
        storage_gb: resources.storage_gb,
    };
    atomic(home, JOURNAL, &value.encode_to_vec())?;
    Ok(resources)
}
fn verify_pending(folder: &Path, expected: &[u8]) -> Result<()> {
    filesystem::existing_directory(folder)?;
    let items: Vec<_> = fs::read_dir(folder)?
        .take(2)
        .collect::<std::io::Result<_>>()?;
    if items.len() != 1
        || items[0].file_name() != "config.ini"
        || filesystem::bounded_file(&folder.join("config.ini"))? != expected
    {
        return Err(Error::Invalid("android_device_creation_unknown"));
    }
    Ok(())
}
fn recover_directory(home: &Path, expected: &[u8], control: &Control<'_>) -> Result<()> {
    let folder = home.join(DIRECTORY);
    if exists(&folder)? {
        return verify_pending(&folder, expected);
    }
    publish_directory(home, &folder, expected, control)
}
fn publish_directory(
    home: &Path,
    folder: &Path,
    expected: &[u8],
    control: &Control<'_>,
) -> Result<()> {
    let temporary = tempfile::Builder::new().prefix(".avd-").tempdir_in(home)?;
    let mut file = fs::File::create_new(temporary.path().join("config.ini"))?;
    file.write_all(expected)?;
    file.sync_all()?;
    // Close the completed file before publishing its parent directory.
    drop(file);
    filesystem::sync_directory(temporary.path())?;
    control.check()?;
    fs::rename(temporary.path(), folder)?;
    filesystem::sync_directory(home)
}
fn complete(home: &Path, sdk: &Path, abi: &str, control: &Control<'_>) -> Result<()> {
    control.check()?;
    let record = format!(
        "avd.ini.encoding=UTF-8\npath={}\ntarget=android-{}\n",
        text(&home.join(DIRECTORY))?,
        Engine::installed(sdk).api()
    );
    if !exists(&home.join(RECORD))? {
        atomic(home, RECORD, record.as_bytes())?;
    }
    verify_existing(home, sdk, abi)?;
    control.check()?;
    fs::remove_file(home.join(JOURNAL))?;
    filesystem::sync_directory(home)
}
pub(super) fn create(
    installation: &Path,
    sdk: &Path,
    abi: &str,
    resources: Resources,
    control: &Control<'_>,
) -> Result<(PathBuf, bool)> {
    let home = layout(installation, Engine::installed(sdk), control)?;
    create_at(&home, sdk, abi, resources, control)
}
pub(super) fn create_at(
    home: &Path,
    sdk: &Path,
    abi: &str,
    resources: Resources,
    control: &Control<'_>,
) -> Result<(PathBuf, bool)> {
    filesystem::directory(home, true)?;
    let state = creation_state(home)?;
    if state == Creation::Ready {
        verify_existing(home, sdk, abi)?;
        return Ok((home.to_path_buf(), true));
    }
    let resources = intent(home, sdk, resources)?;
    finish_creation(home, sdk, abi, &resources, &state, control)?;
    Ok((home.to_path_buf(), state != Creation::New))
}
fn finish_creation(
    home: &Path,
    sdk: &Path,
    abi: &str,
    resources: &Resources,
    state: &Creation,
    control: &Control<'_>,
) -> Result<()> {
    control.check()?;
    if *state != Creation::Published {
        recover_directory(home, &config(sdk, abi, resources)?, control)?;
    }
    complete(home, sdk, abi, control)
}

#[derive(PartialEq)]
enum Creation {
    New,
    Prepared,
    Published,
    Ready,
}
fn creation_state(home: &Path) -> Result<Creation> {
    let pending = exists(&home.join(JOURNAL))?;
    let folder = exists(&home.join(DIRECTORY))?;
    let marker = exists(&home.join(RECORD))?;
    match (pending, folder, marker) {
        (false, false, false) => Ok(Creation::New),
        (false, true, true) => Ok(Creation::Ready),
        (true, _, false) => Ok(Creation::Prepared),
        (true, true, true) => Ok(Creation::Published),
        _ => Err(Error::Invalid("android_device_creation_unknown")),
    }
}
fn layout(installation: &Path, engine: Engine, control: &Control<'_>) -> Result<PathBuf> {
    let base = installation.join("user/data/android");
    let home = base.join(engine.avd_directory());
    for path in [
        installation.join("user"),
        installation.join("user/data"),
        base,
        home.clone(),
    ] {
        control.check()?;
        filesystem::directory(&path, false)?;
    }
    Ok(home)
}
