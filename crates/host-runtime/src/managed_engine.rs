//! Installation-owned Lima state; selecting it never changes a global Docker context.
use crate::{Error, Result, filesystem};
use rustix::{
    fs::{Access, access},
    process::getuid,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) const CONTRACT: &str = "ai-phone-managed-engine-v1";
const RECORD: &str = "data/deployment/container-host.json";
const MARKER: &str = ".aiphone-owner.json";

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Owner {
    format: u32,
    data_root: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    format: u32,
    data_root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    command: PathBuf,
    command_sha256: String,
    wrapper_sha256: String,
}

fn data_root(root: &Path) -> Result<PathBuf> {
    Ok(root.join("data").canonicalize()?)
}

fn installation_home(data: &Path) -> Result<PathBuf> {
    if data.file_name().is_none_or(|name| name != "data")
        || data
            .parent()
            .and_then(Path::file_name)
            .is_none_or(|name| name != "user")
    {
        return Err(Error::Invalid("managed_engine_installation"));
    }
    Ok(data
        .parent()
        .and_then(Path::parent)
        .ok_or(Error::Invalid("managed_engine_installation"))?
        .to_owned())
}

pub(crate) fn state_directory(root: &Path, home: &Path, user: &Path) -> Result<PathBuf> {
    let data = data_root(root)?;
    let home = home.canonicalize()?;
    if home != installation_home(&data)? {
        return Err(Error::Invalid("managed_engine_owner"));
    }
    state_path(&data, &home, user)
}

fn state_path(data: &Path, home: &Path, user: &Path) -> Result<PathBuf> {
    let legacy = home.join("lima");
    let state = if fs::symlink_metadata(&legacy).is_ok() {
        legacy
    } else {
        let digest = format!("{:x}", Sha256::digest(data.as_os_str().as_bytes()));
        user.canonicalize()?
            .join(".aiphone-vms")
            .join(&digest[..16])
    };
    socket_budget(&state)?;
    Ok(state)
}

fn socket_budget(state: &Path) -> Result<()> {
    if state
        .join("ai-phone/ssh.sock.1234567890123456")
        .as_os_str()
        .as_bytes()
        .len()
        >= 104
    {
        return Err(Error::Invalid("managed_engine_socket_path_too_long"));
    }
    Ok(())
}

fn directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != getuid().as_raw() || metadata.mode() & 0o777 != 0o700
    {
        return Err(Error::Invalid("managed_engine_directory_owner"));
    }
    Ok(())
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 16384 {
        return Err(Error::Invalid("managed_engine_record"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|_| Error::Invalid("managed_engine_record"))
}

fn verify_state(data: &Path, path: &Path) -> Result<()> {
    directory(
        path.parent()
            .ok_or(Error::Invalid("managed_engine_state"))?,
    )?;
    directory(path)?;
    let value: Owner = read(&path.join(MARKER))?;
    if value
        != (Owner {
            format: 1,
            data_root: data.to_owned(),
        })
    {
        return Err(Error::Invalid("managed_engine_state_owner"));
    }
    Ok(())
}

fn publish(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(Error::Invalid("managed_engine_record"))?;
    filesystem::directory(parent, true)?;
    filesystem::publish_private(
        parent,
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Invalid("managed_engine_record"))?,
        &serde_json::to_vec(value).map_err(|_| Error::Invalid("managed_engine_record"))?,
        false,
    )
}

pub(crate) fn claim_state(data: &Path, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(Error::Invalid("managed_engine_state"))?;
    for entry in [parent, path] {
        filesystem::directory(entry, false)?;
        directory(entry)?;
    }
    let marker = path.join(MARKER);
    if fs::symlink_metadata(&marker).is_ok() {
        return verify_state(data, path);
    }
    if fs::read_dir(path)?.next().is_some() {
        return Err(Error::Invalid("managed_engine_unowned_state"));
    }
    publish(
        &marker,
        &Owner {
            format: 1,
            data_root: data.to_owned(),
        },
    )
}

fn executable(path: &Path, home: &Path) -> Result<String> {
    if !fs::symlink_metadata(path)?.is_file()
        || !path.canonicalize()?.starts_with(home)
        || access(path, Access::EXEC_OK).is_err()
    {
        return Err(Error::Invalid("managed_engine_executable"));
    }
    let mut input = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let count = input.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn selection(root: &Path, user: &Path) -> Result<Option<Selection>> {
    let path = root.join(RECORD);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let value: Selection = read(&path)?;
    let data = data_root(root)?;
    let home = installation_home(&data)?;
    if value.format != 1
        || value.home != home
        || value.data_root != data
        || value.state != state_path(&data, &home, user)?
    {
        return Err(Error::Invalid("managed_engine_selection_owner"));
    }
    verify_state(&data, &value.state)?;
    if executable(&value.command, &home)? != value.command_sha256
        || executable(&home.join("bin/docker"), &home)? != value.wrapper_sha256
    {
        return Err(Error::Invalid("managed_engine_executable_changed"));
    }
    Ok(Some(value))
}

pub(crate) fn environment(root: &Path, user: &Path) -> Result<BTreeMap<String, String>> {
    selection(root, user)?.map_or_else(|| Ok(BTreeMap::new()), |value| value.environment())
}

#[cfg(target_os = "macos")]
pub(crate) async fn shutdown(home: &Path) -> Result<()> {
    let user = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(Error::Invalid("unix_host_home"))?;
    let Some(engine) = selection(&home.join("user"), &user)? else {
        return Ok(());
    };
    let command = |arguments: &[&str]| {
        let mut command = tokio::process::Command::new(&engine.command);
        command
            .args(arguments)
            .env("LIMA_HOME", &engine.state)
            .kill_on_drop(true);
        command
    };
    let stop = async {
        let observed = command(&[
            "list",
            "--format",
            "{{if eq .Name \"ai-phone\"}}{{.Status}}{{end}}",
        ])
        .output()
        .await?;
        if !observed.status.success() {
            return Err(Error::Invalid("container_host_status"));
        }
        if matches!(observed.stdout.trim_ascii(), b"" | b"Stopped") {
            return Ok(());
        }
        if !command(&["stop", "ai-phone"]).status().await?.success() {
            return Err(Error::Invalid("container_host_stop_failed"));
        }
        Ok(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), stop)
        .await
        .map_err(|_| Error::Invalid("container_host_stop_timeout"))?
}

impl Selection {
    fn environment(&self) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::from([
            (
                "AI_PHONE_MANAGED_BIN".into(),
                self.home
                    .join("bin")
                    .to_str()
                    .ok_or(Error::Invalid("managed_engine_path"))?
                    .into(),
            ),
            (
                "LIMA_HOME".into(),
                self.state
                    .to_str()
                    .ok_or(Error::Invalid("managed_engine_path"))?
                    .into(),
            ),
        ]))
    }
}
pub(crate) fn save(
    root: &Path,
    user: &Path,
    home: &Path,
    state: &Path,
    command: &Path,
    wrapper: &Path,
) -> Result<BTreeMap<String, String>> {
    let home = home.canonicalize()?;
    let data = data_root(root)?;
    if home != installation_home(&data)?
        || state != state_path(&data, &home, user)?
        || wrapper.canonicalize()? != home.join("bin")
    {
        return Err(Error::Invalid("managed_engine_selection_owner"));
    }
    claim_state(&data, state)?;
    let value = Selection {
        format: 1,
        data_root: data,
        state: state.to_owned(),
        command: command.to_owned(),
        command_sha256: executable(command, &home)?,
        wrapper_sha256: executable(&wrapper.join("docker"), &home)?,
        home,
    };
    publish(&root.join(RECORD), &value)?;
    value.environment()
}

pub(crate) fn run(arguments: &[OsString]) -> Result<()> {
    let [root, user, operation, rest @ ..] = arguments else {
        return Err(Error::Invalid("managed_engine_arguments"));
    };
    let root = Path::new(root);
    let user = Path::new(user);
    let value = match (operation.to_str(), rest) {
        (Some("state"), [home]) => json!(state_directory(root, Path::new(home), user)?),
        (Some("claim"), [state]) => {
            claim_state(&data_root(root)?, Path::new(state))?;
            Value::Null
        }
        (Some("save"), [home, state, command, wrapper]) => {
            save(
                root,
                user,
                Path::new(home),
                Path::new(state),
                Path::new(command),
                Path::new(wrapper),
            )?;
            Value::Null
        }
        (Some("selection"), []) => json!(selection(root, user)?),
        (Some("environment"), []) => json!(environment(root, user)?),
        (Some("start"), []) => {
            if let Some(value) = selection(root, user)? {
                // Engine logs belong on stderr; stdout remains a single native receipt.
                let status = Command::new(value.command)
                    .args(["start", "--tty=false", "ai-phone"])
                    .env("LIMA_HOME", value.state)
                    .current_dir(root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(std::io::stderr()))
                    .status()?;
                if !status.success() {
                    return Err(Error::Invalid("managed_engine_start"));
                }
                json!(true)
            } else {
                json!(false)
            }
        }
        _ => return Err(Error::Invalid("managed_engine_arguments")),
    };
    println!("{value}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_limit_counts_actual_utf8_bytes() {
        let suffix = "/ai-phone/ssh.sock.1234567890123456".len();
        assert!(socket_budget(Path::new(&"a".repeat(103 - suffix))).is_ok());
        assert!(socket_budget(Path::new(&"a".repeat(104 - suffix))).is_err());
        assert!(socket_budget(Path::new(&"界".repeat((104 - suffix).div_ceil(3)))).is_err());
    }
}
