use crate::{
    Error, Result, StagedSource, archive, capsule,
    control::Control,
    filesystem, links,
    manifest::{self, Inventory},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::Path,
};

const MAX_COMPRESSED: u64 = 2 * 1024 * 1024 * 1024;

fn layout(home: &Path) -> Result<()> {
    for name in ["releases", "user", "user/data"] {
        filesystem::directory(&home.join(name), false)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn owns_release(root: &Path, candidate: &Path) -> Result<()> {
    let releases = root
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "releases"))
        .ok_or(Error::Invalid("foreign_installation"))?;
    let home = releases
        .parent()
        .ok_or(Error::Invalid("foreign_installation"))?;
    if candidate.parent() != Some(releases)
        || !candidate
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(manifest::identity)
        || !fs::symlink_metadata(candidate.join(manifest::HOST_MANIFEST))?.is_file()
    {
        return Err(Error::Invalid("foreign_installation"));
    }
    links::persistent(root, home, false)?;
    links::persistent(candidate, home, false)
}

fn verify_existing(
    root: &Path,
    index: &Inventory,
    home: &Path,
    control: &Control<'_>,
) -> Result<()> {
    filesystem::checked_parents(root, root)?;
    verify_manifest(root, index)?;
    for row in &index.manifest.files {
        control.check()?;
        filesystem::verify_record(root, row, control)?;
    }
    links::runtime(root, &index.manifest.components, false)?;
    links::persistent(root, home, false)
}

fn verify_manifest(root: &Path, index: &Inventory) -> Result<()> {
    if !manifest::same_manifest(
        &filesystem::bounded_file(&root.join(manifest::HOST_MANIFEST))?,
        &index.raw,
    ) {
        return Err(Error::Invalid("source_resource_changed"));
    }
    Ok(())
}

fn repairable(error: &Error) -> bool {
    matches!(error, Error::Invalid("source_resource_changed"))
        || matches!(error, Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(unix)]
pub(crate) fn resume(
    home: &Path,
    expected: &str,
    control: &Control<'_>,
) -> Result<Option<StagedSource>> {
    let root = home.join("releases").join(expected);
    let verify = || {
        filesystem::existing_directory(&root)?;
        links::persistent(&root, home, false)?;
        let raw = filesystem::bounded_file(&root.join(manifest::HOST_MANIFEST))?;
        let value: manifest::Manifest =
            serde_json::from_slice(&raw).map_err(|_| Error::Invalid("source_resource_changed"))?;
        let headers = value
            .files
            .iter()
            .map(|row| (row.path.clone(), row.size))
            .chain([(manifest::HOST_MANIFEST.to_owned(), raw.len() as u64)])
            .collect();
        let inventory = Inventory {
            manifest: value,
            raw,
            headers,
        };
        manifest::validate(&inventory).map_err(|_| Error::Invalid("source_resource_changed"))?;
        for row in &inventory.manifest.files {
            control.check()?;
            filesystem::open_record(&root, row)?;
        }
        links::runtime(&root, &inventory.manifest.components, false)?;
        Ok(inventory.manifest.components)
    };
    match verify() {
        Ok(components) => Ok(Some(StagedSource {
            directory: root,
            data: home.join("user/data"),
            identity: expected.to_owned(),
            components,
            reused: true,
        })),
        Err(error) if repairable(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn recover_existing(
    file: &mut File,
    root: &Path,
    index: &Inventory,
    home: &Path,
    control: &Control<'_>,
) -> Result<()> {
    filesystem::existing_directory(root)?;
    links::persistent(root, home, false)?;
    let manifest_missing = match verify_manifest(root, index) {
        Ok(()) => false,
        Err(error) if repairable(&error) => true,
        Err(error) => return Err(error),
    };
    let mut damaged = BTreeMap::new();
    for row in &index.manifest.files {
        control.check()?;
        match filesystem::verify_record(root, row, control) {
            Ok(()) => {}
            Err(error) if repairable(&error) => {
                filesystem::checked_parents(
                    root,
                    root.join(&row.path)
                        .parent()
                        .ok_or(Error::Invalid("source_resource_path"))?,
                )?;
                damaged.insert(row.path.as_str(), row);
            }
            Err(error) => return Err(error),
        }
    }
    if !manifest_missing && damaged.is_empty() {
        match links::runtime(root, &index.manifest.components, false) {
            Err(error) if repairable(&error) => {}
            result => return result,
        }
    }
    control.space(root, damaged.values().map(|row| row.size).sum())?;
    let temporary = tempfile::Builder::new()
        .prefix(".source-repair-")
        .tempdir_in(home.join("releases"))?;
    let result = (|| {
        let mut written = 0;
        if !damaged.is_empty() {
            let repeated = archive::visit(file, control, None, |name, _, source| {
                match damaged.get(name) {
                    Some(row) => filesystem::copy_record(
                        temporary.path(),
                        row,
                        source,
                        control,
                        &mut written,
                    ),
                    None => archive::drain(source),
                }
            })?;
            if repeated.headers != index.headers
                || !manifest::same_manifest(&repeated.raw, &index.raw)
            {
                return Err(Error::Invalid("source_resource_changed"));
            }
            for name in damaged.keys() {
                control.check()?;
                fs::rename(temporary.path().join(name), root.join(name))?;
            }
        }
        if manifest_missing {
            control.check()?;
            filesystem::write_manifest(temporary.path(), &index.raw)?;
            fs::rename(
                temporary.path().join(manifest::HOST_MANIFEST),
                root.join(manifest::HOST_MANIFEST),
            )?;
        }
        links::runtime(root, &index.manifest.components, true)?;
        filesystem::sync_directory(root)
    })();
    filesystem::finish_temporary(result, temporary)
}

fn write_source(
    file: &mut File,
    work: &Path,
    expected: &str,
    control: &Control<'_>,
) -> Result<Inventory> {
    let mut records = BTreeMap::new();
    let mut written = 0;
    let index = verified_inventory(file, expected, control, |name, size, source| {
        if records.len().is_multiple_of(32) {
            control.space(work, 0)?;
        }
        manifest::source_name(name, true)?;
        let digest = filesystem::stage_record(work, name, size, source, control, &mut written)?;
        records.insert(name.to_owned(), digest);
        Ok(())
    })?;
    control.space(work, 0)?;
    for row in &index.manifest.files {
        control.check()?;
        if records.get(&row.path) != Some(&row.sha256) {
            return Err(Error::Invalid("source_resource_changed"));
        }
        filesystem::mode(&work.join(&row.path), row.mode)?;
    }
    filesystem::write_manifest(work, &index.raw)?;
    Ok(index)
}

fn publish(
    work: &Path,
    destination: &Path,
    index: &Inventory,
    home: &Path,
    control: &Control<'_>,
) -> Result<()> {
    control.check()?;
    if fs::symlink_metadata(destination).is_ok() {
        return verify_existing(destination, index, home, control);
    }
    fs::rename(work, destination)?;
    filesystem::sync_directory(
        destination
            .parent()
            .ok_or(Error::Invalid("source_resource_directory"))?,
    )
}

fn new_source(
    file: &mut File,
    home: &Path,
    destination: &Path,
    expected: &str,
    control: &Control<'_>,
) -> Result<Inventory> {
    control.space(&home.join("releases"), 0)?;
    let temporary = tempfile::Builder::new()
        .prefix(".source-stage-")
        .tempdir_in(home.join("releases"))?;
    let work = temporary.path().join("source");
    let result = (|| {
        filesystem::directory(&work, false)?;
        let index = write_source(file, &work, expected, control)?;
        links::runtime(&work, &index.manifest.components, true)?;
        links::persistent(&work, home, true)?;
        filesystem::sync_tree(&work, control)?;
        publish(&work, destination, &index, home, control)?;
        Ok(index)
    })();
    filesystem::finish_temporary(result, temporary)
}

fn verified_inventory(
    file: &mut File,
    expected: &str,
    control: &Control<'_>,
    body: impl FnMut(&str, u64, &mut dyn Read) -> Result<()>,
) -> Result<Inventory> {
    let mut digest = Sha256::new();
    let index = archive::visit(file, control, Some(&mut digest), body)?;
    if format!("{:x}", digest.finalize()) != expected {
        return Err(Error::Invalid("source_resource_changed"));
    }
    Ok(index)
}

pub(crate) fn stage(
    payload: &Path,
    home: &Path,
    expected: &str,
    control: &Control<'_>,
    repair: bool,
) -> Result<StagedSource> {
    control.check()?;
    let mut file = filesystem::regular(payload)?;
    capsule::check_identity(&file, expected, MAX_COMPRESSED)?;
    layout(home)?;
    let destination = home.join("releases").join(expected);
    let reused = fs::symlink_metadata(&destination).is_ok();
    let index = if reused {
        let index = verified_inventory(&mut file, expected, control, |_, _, source| {
            archive::drain(source)
        })?;
        if repair {
            recover_existing(&mut file, &destination, &index, home, control)?;
        } else {
            verify_existing(&destination, &index, home, control)?;
        }
        index
    } else {
        new_source(&mut file, home, &destination, expected, control)?
    };
    Ok(StagedSource {
        directory: destination,
        data: home.join("user/data"),
        identity: expected.to_owned(),
        components: index.manifest.components,
        reused,
    })
}
