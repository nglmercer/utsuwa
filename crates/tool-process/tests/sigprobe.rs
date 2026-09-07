#[tokio::test]
async fn probe() {
    let mut child = std::process::Command::new("sleep").arg("30")
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    child.kill().unwrap();
    for i in 0..200 {
        match child.try_wait() {
            Ok(Some(st)) => { println!("reaped iter {i}: {:?}", st.code()); return; }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => { println!("try_wait ERROR iter {i}: {e}"); return; }
        }
    }
    println!("STILL RUNNING after 5s");
}
