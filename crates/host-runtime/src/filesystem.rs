use crate::{
    Error, Result,
    control::{CHUNK, Control},
    manifest::{MAX_MANIFEST, Record},
};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) fn alias(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

pub(crate) fn directory(path: &Path, recursive: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !alias(&metadata) => return Ok(()),
        Ok(_) => return Err(Error::Invalid("source_resource_directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

pub(crate) fn existing_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || alias(&metadata) {
        return Err(Error::Invalid("source_resource_directory"));
    }
    Ok(())
}

pub(crate) fn lock_file(path: &Path) -> Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (alias(&metadata) || !metadata.is_file())
    {
        return Err(Error::Invalid("source_resource_lock"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Invalid("source_resource_lock"));
    }
    Ok(file)
}

fn try_lock(file: &File) -> Result<bool> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) struct Lease(File);
impl Drop for Lease {
    fn drop(&mut self) {
        // Closing one descriptor alone can leave a duplicated open description
        // holding the lock. End this owner's lease before closing its handle.
        let _ = FileExt::unlock(&self.0);
    }
}
pub(crate) fn lease(path: &Path) -> Result<Option<Lease>> {
    let file = lock_file(path)?;
    if try_lock(&file)? {
        Ok(Some(Lease(file)))
    } else {
        Ok(None)
    }
}

pub(crate) fn regular(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if alias(&metadata) || !metadata.is_file() {
        return Err(Error::Invalid("source_resource_file"));
    }
    Ok(File::open(path)?)
}

pub(crate) fn checked_parents(root: &Path, path: &Path) -> Result<()> {
    let mut current = Some(path);
    while let Some(path) = current.filter(|path| path.starts_with(root)) {
        if alias(&fs::symlink_metadata(path)?) {
            return Err(Error::Invalid("source_resource_alias"));
        }
        if path == root {
            break;
        }
        current = path.parent();
    }
    Ok(())
}

pub(crate) fn mode(path: &Path, value: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(value))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, value);
    }
    Ok(())
}

fn correct_mode(metadata: &Metadata, expected: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777 == expected
    }
    #[cfg(not(unix))]
    {
        let _ = expected;
        !metadata.permissions().readonly()
    }
}

pub(crate) fn open_record(root: &Path, row: &Record) -> Result<File> {
    let path = root.join(&row.path);
    checked_parents(root, &path)?;
    let input = regular(&path)?;
    let metadata = input.metadata()?;
    if metadata.len() != row.size || !correct_mode(&metadata, row.mode) {
        return Err(Error::Invalid("source_resource_changed"));
    }
    Ok(input)
}

pub(crate) fn verify_record(root: &Path, row: &Record, control: &Control<'_>) -> Result<()> {
    let mut input = open_record(root, row)?;
    if crate::capsule::hash(&mut input, control)? != row.sha256 {
        return Err(Error::Invalid("source_resource_changed"));
    }
    Ok(())
}

pub(crate) fn bounded_file(path: &Path) -> Result<Vec<u8>> {
    bounded_file_with_limit(path, MAX_MANIFEST)
}

pub(crate) fn bounded_file_with_limit(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let mut input = regular(path)?;
    if input.metadata()?.len() > maximum {
        return Err(Error::Invalid("source_resource_metadata_size"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut input)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(Error::Invalid("source_resource_metadata_size"));
    }
    Ok(bytes)
}

pub(crate) fn copy_record(
    root: &Path,
    row: &Record,
    source: &mut dyn Read,
    control: &Control<'_>,
    written: &mut u64,
) -> Result<()> {
    if stage_record(root, &row.path, row.size, source, control, written)? != row.sha256 {
        return Err(Error::Invalid("source_resource_changed"));
    }
    mode(&root.join(&row.path), row.mode)
}

pub(crate) fn stage_record(
    root: &Path,
    name: &str,
    size: u64,
    source: &mut dyn Read,
    control: &Control<'_>,
    written: &mut u64,
) -> Result<String> {
    let path = root.join(name);
    let parent = path
        .parent()
        .ok_or(Error::Invalid("source_resource_path"))?;
    directory(parent, true)?;
    checked_parents(root, parent)?;
    let mut output = File::create_new(path)?;
    let mut digest = Sha256::new();
    let mut actual = 0;
    let mut buffer = [0; CHUNK];
    loop {
        control.check()?;
        let length = source.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        actual += length as u64;
        if actual > size {
            return Err(Error::Invalid("source_resource_changed"));
        }
        check_written(root, control, written, length as u64)?;
        output.write_all(&buffer[..length])?;
        digest.update(&buffer[..length]);
    }
    if actual != size {
        return Err(Error::Invalid("source_resource_changed"));
    }
    output.sync_all()?;
    Ok(format!("{:x}", digest.finalize()))
}

fn check_written(path: &Path, control: &Control<'_>, written: &mut u64, size: u64) -> Result<()> {
    *written += size;
    if *written >= 32 * 1024 * 1024 {
        control.space(path, 0)?;
        *written = 0;
    }
    Ok(())
}

pub(crate) fn write_manifest(root: &Path, bytes: &[u8]) -> Result<()> {
    let path = root.join(crate::manifest::HOST_MANIFEST);
    let mut output = File::create_new(&path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    mode(&path, 0o644)
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn publish_private(home: &Path, name: &str, bytes: &[u8], initial: bool) -> Result<()> {
    let mut output = tempfile::NamedTempFile::new_in(home)?;
    output.write_all(bytes)?;
    output.as_file().sync_all()?;
    let path = home.join(name);
    if initial {
        output.persist_noclobber(path)
    } else {
        output.persist(path)
    }
    .map_err(|error| Error::Io(error.error))?;
    sync_directory(home)
}

pub(crate) fn finish_temporary<T>(result: Result<T>, temporary: tempfile::TempDir) -> Result<T> {
    match (result, temporary.close()) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(primary), Err(cleanup)) => Err(Error::Cleanup {
            primary: Box::new(primary),
            cleanup,
        }),
    }
}

pub(crate) fn sync_tree(path: &Path, control: &Control<'_>) -> Result<()> {
    sync_children(path, control, 0, &mut 0)
}

fn sync_children(
    path: &Path,
    control: &Control<'_>,
    depth: usize,
    visited: &mut usize,
) -> Result<()> {
    control.check()?;
    *visited += 1;
    if depth > 64 || *visited > crate::manifest::MAX_FILES * 64 {
        return Err(Error::Invalid("source_resource_directory_budget"));
    }
    for item in fs::read_dir(path)? {
        sync_descendant(&item?, control, depth, visited)?;
    }
    sync_directory(path)
}

fn sync_descendant(
    item: &fs::DirEntry,
    control: &Control<'_>,
    depth: usize,
    visited: &mut usize,
) -> Result<()> {
    let metadata = fs::symlink_metadata(item.path())?;
    if metadata.is_dir() && !alias(&metadata) {
        sync_children(&item.path(), control, depth + 1, visited)?;
    }
    Ok(())
}

pub(crate) fn normalized_target(path: &Path, depth: usize) -> Result<PathBuf> {
    if depth > 64 {
        return Err(Error::Invalid("source_resource_link"));
    }
    if let Ok(value) = path.canonicalize() {
        return Ok(value);
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        let target = link_destination(path)?;
        return normalized_target(&target, depth + 1);
    }
    let parent = path
        .parent()
        .ok_or(Error::Invalid("source_resource_link"))?
        .canonicalize()?;
    Ok(parent.join(
        path.file_name()
            .ok_or(Error::Invalid("source_resource_link"))?,
    ))
}

fn link_destination(path: &Path) -> Result<PathBuf> {
    let target = fs::read_link(path)?;
    if target.is_absolute() {
        return Ok(target);
    }
    Ok(path
        .parent()
        .ok_or(Error::Invalid("source_resource_link"))?
        .join(target))
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[test]
    fn lease_release_does_not_wait_for_a_duplicate_description_to_close() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("owned.lock");
        let guard = lease(&path).unwrap().unwrap();
        let duplicate = guard.0.try_clone().unwrap();
        assert!(lease(&path).unwrap().is_none());
        drop(guard);
        assert!(
            lease(&path).unwrap().is_some(),
            "the owner ended, even if a transient process descriptor still exists"
        );
        drop(duplicate);
    }
}
