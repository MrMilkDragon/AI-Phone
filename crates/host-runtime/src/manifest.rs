use crate::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const HOST_MANIFEST: &str = "HOST-MANIFEST.json";
pub(crate) const MAX_FILES: usize = 20_000;
pub(crate) const MAX_FILE: u64 = 384 * 1024 * 1024;
pub(crate) const MAX_TOTAL: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_MANIFEST: u64 = 4 * 1024 * 1024;
pub(crate) const COMPONENTS: [(&str, &str); 4] = [
    ("web", "runtime/manifest.json"),
    ("native-services", "runtime/services/manifest.json"),
    ("phone-apps", "runtime/phone-apps/manifest.json"),
    ("container-host", "runtime/container-host/manifest.json"),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "RawManifest")]
pub(crate) struct Manifest {
    pub format: u32,
    pub components: Vec<String>,
    pub files: Vec<Record>,
}

#[derive(Deserialize)]
struct RawManifest {
    format: u32,
    #[serde(default)]
    components: Value,
    files: Vec<Record>,
}

impl TryFrom<RawManifest> for Manifest {
    type Error = &'static str;

    fn try_from(value: RawManifest) -> std::result::Result<Self, Self::Error> {
        let components = if value.format == 1 {
            Vec::new()
        } else {
            serde_json::from_value(value.components).map_err(|_| "Invalid source components")?
        };
        Ok(Self {
            format: value.format,
            components,
            files: value.files,
        })
    }
}

pub(crate) struct Inventory {
    pub manifest: Manifest,
    pub raw: Vec<u8>,
    pub headers: BTreeMap<String, u64>,
}

pub(crate) fn identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn canonical(value: &str) -> Result<()> {
    if value.is_empty() || value.contains(['\\', ':']) || value.chars().any(char::is_control) {
        return Err(Error::Invalid("source_resource_path"));
    }
    if value.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(Error::Invalid("source_resource_path"));
    }
    Ok(())
}

fn runtime_name(value: &str, enabled: bool) -> bool {
    let mut parts = value.split('/');
    enabled
        && parts.next() == Some("runtime")
        && matches!(
            parts.next(),
            Some("web" | "bin" | "manifest.json" | "services" | "phone-apps" | "container-host")
        )
}

pub(crate) fn source_name(value: &str, enabled: bool) -> Result<()> {
    canonical(value)?;
    let runtime = runtime_name(value, enabled);
    let vendor = value.split('/').any(|part| part == "node_modules");
    for part in value.split('/') {
        forbidden_part(part, runtime, vendor)?;
        if value != ".env.example" && part.starts_with(".env") {
            return Err(Error::Invalid("source_resource_private_path"));
        }
    }
    Ok(())
}

fn forbidden_part(part: &str, runtime: bool, vendor: bool) -> Result<()> {
    let always = matches!(part, ".git" | ".tools");
    let source = !runtime
        && matches!(
            part,
            "data" | "user" | "target" | "node_modules" | "runtime"
        );
    let private = runtime && !vendor && matches!(part, "data" | "user");
    if always || source || private {
        Err(Error::Invalid("source_resource_private_path"))
    } else {
        Ok(())
    }
}

fn enabled_components(value: &Manifest) -> Result<bool> {
    if value.format == 1 {
        return Ok(false);
    }
    let declared: Vec<&str> = value.components.iter().map(String::as_str).collect();
    let ordered: Vec<&str> = COMPONENTS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| declared.contains(name))
        .collect();
    if value.format != 2 || declared.is_empty() || declared != ordered {
        return Err(Error::Invalid("source_resource_components"));
    }
    Ok(true)
}

fn validate_row(row: &Record, runtime: bool) -> Result<()> {
    source_name(&row.path, runtime)?;
    if !identity(&row.sha256) || ![0o644, 0o755].contains(&row.mode) || row.size > MAX_FILE {
        return Err(Error::Invalid("source_resource_record"));
    }
    if !row.path.starts_with("runtime/") && row.size > 16 * 1024 * 1024 {
        return Err(Error::Invalid("source_resource_size"));
    }
    Ok(())
}

fn validate_components(value: &Manifest, names: &BTreeSet<&str>) -> Result<()> {
    for (role, path) in COMPONENTS {
        if value.components.iter().any(|name| name == role) && !names.contains(path) {
            return Err(Error::Invalid("source_resource_component_manifest"));
        }
    }
    for name in names.iter().filter(|name| name.starts_with("runtime/")) {
        let owner = COMPONENTS
            .iter()
            .rev()
            .find(|(_, path)| {
                let prefix = path.rsplit_once('/').map_or("", |(prefix, _)| prefix);
                name.starts_with(&format!("{prefix}/"))
            })
            .map(|(role, _)| *role)
            .ok_or(Error::Invalid("source_resource_components"))?;
        if !value.components.iter().any(|name| name == owner) {
            return Err(Error::Invalid("source_resource_components"));
        }
    }
    Ok(())
}

fn inventory_row<'a>(
    row: &'a Record,
    inventory: &Inventory,
    names: &mut BTreeSet<&'a str>,
    runtime: bool,
) -> Result<()> {
    validate_row(row, runtime)?;
    if row.path == HOST_MANIFEST
        || !names.insert(row.path.as_str())
        || inventory.headers.get(&row.path) != Some(&row.size)
    {
        return Err(Error::Invalid("source_resource_inventory"));
    }
    Ok(())
}

fn inventory_rows(inventory: &Inventory, runtime: bool) -> Result<(BTreeSet<&str>, u64)> {
    let mut names = BTreeSet::new();
    let mut total = inventory.raw.len() as u64;
    for row in &inventory.manifest.files {
        inventory_row(row, inventory, &mut names, runtime)?;
        total = total
            .checked_add(row.size)
            .ok_or(Error::Invalid("source_resource_size"))?;
    }
    Ok((names, total))
}

fn validate_totals(
    header_count: usize,
    file_count: usize,
    total: u64,
    runtime: bool,
) -> Result<()> {
    let expected = header_count
        .checked_sub(1)
        .ok_or(Error::Invalid("source_resource_inventory"))?;
    if file_count != expected || total > MAX_TOTAL || (!runtime && total > MAX_FILE) {
        return Err(Error::Invalid("source_resource_inventory"));
    }
    Ok(())
}

pub(crate) fn validate(inventory: &Inventory) -> Result<()> {
    let value = &inventory.manifest;
    let runtime = enabled_components(value)?;
    let (names, total) = inventory_rows(inventory, runtime)?;
    validate_totals(inventory.headers.len(), names.len(), total, runtime)?;
    if runtime {
        validate_components(value, &names)?;
    }
    Ok(())
}

pub(crate) fn same_manifest(first: &[u8], second: &[u8]) -> bool {
    let left = serde_json::from_slice::<Value>(first);
    let right = serde_json::from_slice::<Value>(second);
    matches!((left, right), (Ok(left), Ok(right)) if left == right)
}
