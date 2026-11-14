//! Offline Android resources and device creation. Preparation is not boot readiness.
use crate::{Error, Installation, Result, control::Control};
use std::path::{Path, PathBuf};
pub mod cli;
mod device;
pub mod input;
pub mod maintenance;
mod manifest;
mod resources;
pub mod session;
#[cfg(target_os = "macos")]
pub use session::avo_runtime;

pub const AVD_NAME: &str = "ai_phone_p2";
const OPTIONAL_SYSTEM_APPS: &[&str] = &[
    "com.android.browser",
    "com.android.calendar",
    "com.android.camera2",
    "com.android.contacts",
    "com.android.deskclock",
    "com.android.devicediagnostics",
    "com.android.dialer",
    "com.android.dreams.basic",
    "com.android.dreams.phototable",
    "com.android.egg",
    "com.android.emulator.multidisplay",
    "com.android.gallery3d",
    "com.android.messaging",
    "com.android.music",
    "com.android.musicfx",
    "com.android.quicksearchbox",
    "com.android.stk",
    "com.android.traceur",
    "org.chromium.webview_shell",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    #[default]
    Goldfish,
    Avo,
    Whpx,
}

impl Engine {
    fn avd_directory(self) -> &'static str {
        match self {
            Self::Goldfish => "avd",
            Self::Avo => "avo-avd",
            Self::Whpx => "whpx-avd",
        }
    }

    pub fn installed(sdk: &Path) -> Self {
        if sdk.join("avo/avo-vmm").is_file() {
            Self::Avo
        } else if sdk.join("cuttlefish/bzImage").is_file() {
            Self::Whpx
        } else {
            Self::Goldfish
        }
    }

    fn sdk_images(self, abi: &str) -> PathBuf {
        match self {
            Self::Goldfish => format!("system-images/android-35/default/{abi}").into(),
            Self::Avo => "avo/images".into(),
            Self::Whpx => "cuttlefish".into(),
        }
    }

    fn api(self) -> u32 {
        match self {
            Self::Avo => 37,
            Self::Goldfish | Self::Whpx => 35,
        }
    }

    pub fn serial(self, console_port: u16) -> String {
        match self {
            Self::Goldfish => format!("emulator-{console_port}"),
            #[cfg(target_os = "macos")]
            Self::Avo => format!(
                "localfilesystem:{}",
                avo_runtime(console_port).join("adb.sock").display()
            ),
            #[cfg(not(target_os = "macos"))]
            Self::Avo => unreachable!("Avo is available only on macOS"),
            Self::Whpx => format!("127.0.0.1:{}", console_port + 1),
        }
    }
}

pub struct Resources {
    pub cpus: u32,
    pub memory_mb: u32,
    pub storage_gb: u32,
}
impl Resources {
    fn validate(&self) -> Result<()> {
        if ![1, 2, 4].contains(&self.cpus)
            || ![1536, 2048, 3072, 4096].contains(&self.memory_mb)
            || ![16, 24, 32].contains(&self.storage_gb)
        {
            return Err(Error::Invalid("invalid_device_resources"));
        }
        Ok(())
    }
}

pub struct Prepared {
    pub sdk_root: PathBuf,
    pub avd_home: PathBuf,
    pub reused_sdk: bool,
    pub reused_device: bool,
}
impl Prepared {
    pub fn wire(&self) -> Result<ai_phone_api::wire::NativeAndroidPrepared> {
        Ok(ai_phone_api::wire::NativeAndroidPrepared {
            version: 1,
            sdk_root: self
                .sdk_root
                .to_str()
                .ok_or(Error::Invalid("android_device_path"))?
                .into(),
            avd_home: self
                .avd_home
                .to_str()
                .ok_or(Error::Invalid("android_device_path"))?
                .into(),
            avd_name: AVD_NAME.into(),
            reused_sdk: self.reused_sdk,
            reused_device: self.reused_device,
        })
    }
}

impl Installation {
    pub fn prepare_android(
        &mut self,
        payload: &Path,
        identity: &str,
        resources: Resources,
        available: &dyn Fn(&Path) -> u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Prepared> {
        let platform = manifest::Platform::current()?;
        self.prepare_android_for(
            payload,
            identity,
            resources,
            platform,
            &Control {
                available,
                cancelled,
            },
        )
    }

    pub fn resume_android(&self, identity: &str) -> Result<Option<Prepared>> {
        use crate::filesystem;
        let platform = manifest::Platform::current()?;
        if !crate::manifest::identity(identity) {
            return Err(Error::Invalid("android_resource_manifest"));
        }
        let root = self.home.join("components/android").join(identity);
        if !root.try_exists()? {
            return Ok(None);
        }
        filesystem::checked_parents(&self.home, &root)?;
        let raw = filesystem::bounded_file_with_limit(
            &root.join(manifest::NAME),
            manifest::LIMITS.metadata,
        )?;
        let value = manifest::parse(&raw, platform)?;
        for record in &value.files {
            filesystem::open_record(&root, record)?;
        }
        let sdk_root = root.join("sdk");
        let avd_home = self
            .home
            .join("user/data/android")
            .join(Engine::installed(&sdk_root).avd_directory());
        device::verify_existing(&avd_home, &sdk_root, platform.spec().abi)?;
        Ok(Some(Prepared {
            sdk_root,
            avd_home,
            reused_sdk: true,
            reused_device: true,
        }))
    }

    fn prepare_android_for(
        &mut self,
        payload: &Path,
        identity: &str,
        resources: Resources,
        platform: manifest::Platform,
        control: &Control<'_>,
    ) -> Result<Prepared> {
        resources.validate()?;
        control.check()?;
        let (sdk_root, reused_sdk) =
            resources::stage(&self.home, payload, identity, platform, control)?;
        let (avd_home, reused_device) = device::create(
            &self.home,
            &sdk_root,
            platform.spec().abi,
            resources,
            control,
        )?;
        Ok(Prepared {
            sdk_root,
            avd_home,
            reused_sdk,
            reused_device,
        })
    }
}

#[cfg(test)]
mod tests;
