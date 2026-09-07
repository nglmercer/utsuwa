use std::collections::HashMap;
use tool_process::{ProcessLimits, ProcessManager, SpawnSpec};

#[test]
fn dbg_kill() {
    let manager = ProcessManager::new(ProcessLimits::default());
    let exe = tool_process::resolve_executable("sleep").unwrap();
    let (handle, _) = manager.spawn(&SpawnSpec {
        executable: exe,
        args: vec!["30".to_string()],
        cwd: std::env::temp_dir(),
        env: HashMap::new(),
        timeout: std::time::Duration::from_secs(60),
    }).unwrap();
    let s0 = manager.status(&handle).unwrap();
    let pid = s0.pid;
    eprintln!("pid={pid} state={}", s0.state);
    eprintln!("ps before: {:?}", std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "pid,stat,comm"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()));
    let k = manager.kill(&handle).unwrap();
    eprintln!("kill returned state={}", k.state);
    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("ps after: {:?}", std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "pid,stat,comm"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()));
    let s1 = manager.status(&handle).unwrap();
    eprintln!("status after: state={} code={:?}", s1.state, s1.exit_code);
}
