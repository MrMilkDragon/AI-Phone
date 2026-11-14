use super::{Arc, Body, Bytes, Duration, Media, Ordering, Result, now};
use futures_util::stream;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::mpsc,
};
fn capture_command(kind: &str) -> Result<Child> {
    Ok(
        Command::new(std::env::var_os("AI_PHONE_DOCKER").unwrap_or("docker".into()))
            .args([
                "exec",
                "-i",
                "-u",
                "ai",
                "ai-phone-desktop",
                "/usr/local/bin/desktop-bridge",
                "capture",
                kind,
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?,
    )
}
async fn screen(media: &Media) -> Result<()> {
    let mut child = capture_command("screen")?;
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    let next_frame = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(next_frame);
    let mut buffer = Vec::new();
    let mut chunk = [0; 65536];
    let ended = 'capture: loop {
        if media.desktop.viewers.load(Ordering::Relaxed) == 0 {
            break false;
        }
        tokio::select! {
            _=heartbeat.tick()=>input.write_all(b"1").await?,
            _=media.desktop.changed.notified()=>{},
            _=&mut next_frame=>return Err("desktop screen stopped producing frames".into()),
            bytes=output.read(&mut chunk)=>{
                let bytes=bytes?;if bytes==0{break 'capture true;}
                buffer.extend_from_slice(&chunk[..bytes]);
                if buffer.len()>4*1024*1024{return Err("desktop frame exceeds capture buffer".into());}
                while let Some(begin)=buffer.windows(2).position(|v|v==[255,216]){
                    if begin>0{buffer.drain(..begin);}
                    let Some(end)=buffer[2..].windows(2).position(|v|v==[255,217]).map(|v|v+4)else{break;};
                    let jpeg=buffer.drain(..end).collect();media.desktop.publish(jpeg,now());
                    next_frame.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3));
                }
            }
        }
    };
    drop(input);
    if tokio::time::timeout(Duration::from_secs(1), child.wait())
        .await
        .is_err()
    {
        child.kill().await?;
    }
    if ended {
        return Err("desktop screen ended".into());
    }
    Ok(())
}
pub async fn capture(media: Arc<Media>) {
    loop {
        media.desktop.demand().await;
        if let Err(error) = screen(&media).await {
            eprintln!("desktop screen: {error}");
            media.desktop.end();
            media.desktop.changed.notified().await;
        }
    }
}
pub async fn audio() -> Result<Body> {
    let mut child = capture_command("audio")?;
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel(12);
    tokio::spawn(async move {
        let result:Result<()>=async{
            sender.send(Ok(Bytes::from_static(&[0;4]))).await?;
            let mut heartbeat=tokio::time::interval(Duration::from_secs(1));let mut bytes=[0;16384];
            loop{tokio::select!{
                _=sender.closed()=>break,
                _=heartbeat.tick()=>input.write_all(b"1").await?,
                read = output.read(&mut bytes) => {
                    let size = read?;
                    if size == 0 { break; }
                    if sender.try_send(Ok(Bytes::copy_from_slice(&bytes[..size]))).is_err() { break; }
                }
            }}Ok(())
        }.await;
        drop(input);
        let _ = child.wait().await;
        if let Err(error) = result {
            let _ = sender.send(Err(std::io::Error::other(error))).await;
        }
    });
    Ok(Body::from_stream(stream::unfold(
        receiver,
        |mut receiver| async move { receiver.recv().await.map(|value| (value, receiver)) },
    )))
}
