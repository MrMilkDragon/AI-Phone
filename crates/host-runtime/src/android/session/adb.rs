use crate::{Error, Result};
use std::time::Duration;

pub(super) struct Adb {
    pub port: u16,
    pub serial: String,
}
fn error(value: ai_phone_adb::Error) -> Error {
    match value {
        ai_phone_adb::Error::Io(value) => Error::Io(value),
        ai_phone_adb::Error::Rejected(_) => Error::Invalid("android_adb_rejected"),
        ai_phone_adb::Error::Protocol => Error::Invalid("android_adb_frame"),
        ai_phone_adb::Error::Limit => Error::Invalid("android_adb_output"),
        ai_phone_adb::Error::Timeout => Error::Invalid("android_adb_unknown"),
    }
}
impl Adb {
    fn client(&self) -> Result<ai_phone_adb::Client> {
        ai_phone_adb::Client::new(self.port, self.serial.clone()).map_err(error)
    }
    async fn request(&self, service: &str, limit: usize) -> Result<Vec<u8>> {
        self.client()?
            .raw(service, limit, Duration::from_secs(3))
            .await
            .map_err(error)
    }
    #[cfg(any(target_os = "macos", windows))]
    pub async fn sync(&self) -> Result<()> {
        let receipt = self
            .client()?
            .shell(
                "sync",
                ai_phone_adb::Limits {
                    stdout: 1024,
                    stderr: 1024,
                },
                Duration::from_secs(4),
            )
            .await
            .map_err(error)?;
        if receipt.code != 0 {
            return Err(Error::Invalid("android_sync_failed"));
        }
        Ok(())
    }
    pub async fn uid(&self) -> Result<u32> {
        let bytes = self.request("shell:id -u", 128).await?;
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .ok_or(Error::Invalid("android_root_identity"))
    }
    pub async fn boot_completed(&self) -> Result<bool> {
        Ok(self
            .request("shell:getprop sys.boot_completed", 128)
            .await?
            .trim_ascii()
            == b"1")
    }
    pub async fn root(&self) -> Result<()> {
        let bytes = self.request("root:", 4096).await?;
        if bytes
            .windows(b"cannot run as root".len())
            .any(|v| v == b"cannot run as root")
        {
            return Err(Error::Invalid("android_root_unavailable"));
        }
        Ok(())
    }
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.request("exec:screencap -p", 8 * 1024 * 1024).await
    }
    pub async fn server(&self) -> Result<()> {
        let version = self
            .client()?
            .version(Duration::from_secs(3))
            .await
            .map_err(error)?;
        if version.is_empty() || !version.iter().all(u8::is_ascii_hexdigit) {
            return Err(Error::Invalid("android_adb_frame"));
        }
        Ok(())
    }
    pub async fn attach(&self) -> Result<bool> {
        let reply = self
            .client()?
            .connect_device(Duration::from_secs(2))
            .await
            .map_err(error)?;
        Ok(reply.starts_with(b"connected to ") || reply.starts_with(b"already connected to "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn selects_the_exact_serial_before_a_bounded_raw_uid_request() {
        let server = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = server.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = server.accept().await.unwrap();
            for expected in ["host:transport:emulator-5582", "shell:id -u"] {
                let mut length = [0; 4];
                socket.read_exact(&mut length).await.unwrap();
                let size =
                    usize::from_str_radix(std::str::from_utf8(&length).unwrap(), 16).unwrap();
                let mut bytes = vec![0; size];
                socket.read_exact(&mut bytes).await.unwrap();
                assert_eq!(bytes, expected.as_bytes());
                socket.write_all(b"OKAY").await.unwrap();
            }
            socket.write_all(b"0\r\n").await.unwrap();
        });
        let result = Adb {
            port,
            serial: "emulator-5582".into(),
        }
        .uid()
        .await;
        if result.is_err() {
            peer.abort();
            let _ = peer.await;
        } else {
            peer.await.unwrap();
        }
        assert_eq!(result.unwrap(), 0);
    }
}
