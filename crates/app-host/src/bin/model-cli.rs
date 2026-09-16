//! Headless model-test CLI: ask one question of a real model through the
//! full native agent stack (provider adapter, tool registry, policy
//! engine) and print the verified result.
//!
//! ```sh
//! cargo run -p app-host --bin model-cli -- --ask "take a screenshot and save it to my Desktop" --yes
//! ```
//!
//! Defaults target the Kilo gateway's free model (`kilo-auto/free`, no API
//! key). Anything else needs `--provider/--model/--base-url` plus
//! `--api-key` or `UTSUWA_MODEL_API_KEY`. State is an isolated temp
//! database: the CLI never touches the desktop app's real `state.db`.
//!
//! Permission requests suspend the turn exactly like in the app. Pass
//! `--yes` to auto-approve with session-scoped grants (test use only),
//! otherwise each request is confirmed on stdin.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KILO_BASE_URL: &str = "https://api.kilo.ai/api/gateway";
const KILO_FREE_MODEL: &str = "kilo-auto/free";

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    ask: String,
    provider: String,
    model: String,
    base_url: String,
    api_key: Option<String>,
    vision: Option<bool>,
    profile: Option<String>,
    auto_approve: bool,
    timeout: Duration,
    verbose: bool,
}

fn usage() -> &'static str {
    "usage: model-cli --ask \"question\" [--provider ID] [--model ID] [--base-url URL]\n\
     \t[--api-key KEY] [--vision auto|on|off] [--profile NAME] [--yes]\n\
     \t[--timeout-secs N] [--verbose]\n\
     \n\
     \tDefaults: --provider kilo --model kilo-auto/free --vision auto\n\
     \t--api-key falls back to $UTSUWA_MODEL_API_KEY (Kilo needs none).\n\
     \t--vision auto uses the API-discovered capability; on is a debug\n\
     \toverride that forces image sending; off force-disables it.\n\
     \t--profile minimal|simple|standard|developer|computeruse|full shrinks the\n\
     \tmodel-facing tool surface (default full, like the app; small free models\n\
     \tcope better with developer for shell/file tasks).\n\
     \t--yes auto-approves permission requests with session grants."
}

fn vision_mode(vision: Option<bool>) -> &'static str {
    match vision {
        None => "auto",
        Some(true) => "on",
        Some(false) => "off",
    }
}

fn print_resolved_capabilities(args: &CliArgs) {
    app_host::runtime::providers::print_resolved_capabilities(
        &args.provider,
        &args.model,
        &args.base_url,
        args.api_key.as_deref(),
        args.vision,
    )
}

const KNOWN_PROFILES: &[&str] = &[
    "minimal",
    "simple",
    "standard",
    "developer",
    "computeruse",
    "computer-use",
    "computer_use",
    "full",
];

fn parse_args(argv: &[String]) -> Result<CliArgs, String> {
    let mut ask = None;
    let mut provider = None;
    let mut model = None;
    let mut base_url = None;
    let mut api_key = None;
    let mut vision = None;
    let mut profile = None;
    let mut auto_approve = false;
    let mut timeout_secs = 300u64;
    let mut verbose = false;
    let mut index = 0;
    let flag_value = |argv: &[String], index: &mut usize, flag: &str| -> Result<String, String> {
        *index += 1;
        argv.get(*index)
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while index < argv.len() {
        match argv[index].as_str() {
            "--ask" => ask = Some(flag_value(argv, &mut index, "--ask")?),
            "--provider" => provider = Some(flag_value(argv, &mut index, "--provider")?),
            "--model" => model = Some(flag_value(argv, &mut index, "--model")?),
            "--base-url" => base_url = Some(flag_value(argv, &mut index, "--base-url")?),
            "--api-key" => api_key = Some(flag_value(argv, &mut index, "--api-key")?),
            "--vision" => {
                vision = match flag_value(argv, &mut index, "--vision")?.as_str() {
                    "auto" => None,
                    "on" => Some(true),
                    "off" => Some(false),
                    other => {
                        return Err(format!("--vision must be auto, on, or off (got {other})"))
                    }
                };
            }
            "--profile" => {
                let name = flag_value(argv, &mut index, "--profile")?;
                if !KNOWN_PROFILES.contains(&name.to_ascii_lowercase().as_str()) {
                    return Err(format!(
                        "--profile must be one of minimal, simple, standard, developer, computeruse, full (got {name})"
                    ));
                }
                profile = Some(name.to_ascii_lowercase());
            }
            "--yes" => auto_approve = true,
            "--verbose" => verbose = true,
            "--timeout-secs" => {
                let raw = flag_value(argv, &mut index, "--timeout-secs")?;
                timeout_secs = raw
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout-secs must be a number (got {raw})"))?;
            }
            "--help" | "-h" => return Err(usage().to_string()),
            other => {
                return Err(format!(
                    "unknown argument: {other}\n{usage}",
                    usage = usage()
                ))
            }
        }
        index += 1;
    }
    let ask = ask.ok_or_else(|| format!("missing --ask\n{usage}", usage = usage()))?;
    if ask.trim().is_empty() {
        return Err("--ask must not be empty".to_string());
    }
    let provider = provider.unwrap_or_else(|| "kilo".to_string());
    let model = model.unwrap_or_else(|| {
        if provider == "kilo" {
            KILO_FREE_MODEL.to_string()
        } else {
            String::new()
        }
    });
    if model.is_empty() {
        return Err("--model is required for non-kilo providers".to_string());
    }
    let base_url = base_url.unwrap_or_else(|| {
        if provider == "kilo" {
            KILO_BASE_URL.to_string()
        } else {
            String::new()
        }
    });
    if base_url.is_empty() {
        return Err("--base-url is required for non-kilo providers".to_string());
    }
    let api_key = api_key.or_else(|| std::env::var("UTSUWA_MODEL_API_KEY").ok());
    Ok(CliArgs {
        ask,
        provider,
        model,
        base_url,
        api_key,
        vision,
        profile,
        auto_approve,
        timeout: Duration::from_secs(timeout_secs.max(5)),
        verbose,
    })
}

fn confirm(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

fn run(argv: Vec<String>) -> i32 {
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    // Isolated state: a temp database plus in-memory secrets. The desktop
    // app's real state.db, grants, and keychain entries are never touched.
    let state_dir = std::env::temp_dir().join(format!("utsuwa-model-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state_dir);
    if let Err(error) = std::fs::create_dir_all(&state_dir) {
        eprintln!(
            "cannot create temp state dir {}: {error}",
            state_dir.display()
        );
        return 2;
    }
    let storage = Arc::new(Mutex::new(
        match storage_core::Storage::open(&state_dir.join("state.db")) {
            Ok(store) => store,
            Err(error) => {
                eprintln!("cannot open temp state.db: {error}");
                return 2;
            }
        },
    ));
    {
        let store = storage.lock().expect("temp storage lock");
        let settings = [
            (app_host::runtime::SETTING_PROVIDER, args.provider.as_str()),
            (app_host::runtime::SETTING_BASE_URL, args.base_url.as_str()),
            (app_host::runtime::SETTING_MODEL_NAME, args.model.as_str()),
        ];
        for (key, value) in settings {
            if let Err(error) = store.set_setting(key, &serde_json::json!(value)) {
                eprintln!("cannot write temp setting {key}: {error}");
                return 2;
            }
        }
        if let Some(vision) = args.vision {
            if let Err(error) = store.set_setting(
                app_host::runtime::SETTING_MODEL_VISION,
                &serde_json::json!(vision),
            ) {
                eprintln!("cannot write temp vision setting: {error}");
                return 2;
            }
        }
        if let Some(profile) = &args.profile {
            if let Err(error) = store.set_setting(
                app_host::runtime::SETTING_TOOL_PROFILE,
                &serde_json::json!(profile),
            ) {
                eprintln!("cannot write temp tool-profile setting: {error}");
                return 2;
            }
        }
    }
    let secrets: Arc<dyn secret_core::SecretStore> = Arc::new(secret_core::MemoryStore::default());
    if let Some(key) = &args.api_key {
        if let Err(error) = secrets.set(secret_core::ACCOUNT_MODEL_API_KEY, key) {
            eprintln!("cannot stage API key: {error}");
            return 2;
        }
    }
    let approvals = Arc::new(Mutex::new(policy_core::ApprovalQueue::new()));
    let events: Arc<Mutex<Vec<ipc_core::HostEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let verbose = args.verbose;
    let emit: app_host::runtime::EmitFn = Arc::new(move |event| {
        if verbose {
            println!("event {} {}", event.event, event.data);
        }
        match event.event.as_str() {
            "agent.text_delta" => {
                if let Some(delta) = event.data.get("delta").and_then(|v| v.as_str()) {
                    print!("{delta}");
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                }
            }
            "agent.tool_started" => {
                println!(
                    "\n[tool] {} started",
                    event
                        .data
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                );
            }
            "agent.tool_finished" => {
                println!(
                    "[tool] {} finished ok={}",
                    event
                        .data
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?"),
                    event
                        .data
                        .get("ok")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                );
            }
            _ => {}
        }
        sink.lock().expect("event log lock").push(event);
    });
    let runtime = match app_host::runtime::AgentRuntime::start_with_secrets(
        Arc::clone(&approvals),
        Some(Arc::clone(&storage)),
        None,
        Arc::clone(&emit),
        Arc::clone(&secrets),
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start agent runtime: {error}");
            return 1;
        }
    };
    println!(
        "provider={} model={} base_url={} api_key={} vision={} profile={}",
        args.provider,
        args.model,
        args.base_url,
        if args.api_key.is_some() {
            "set"
        } else {
            "none"
        },
        vision_mode(args.vision),
        args.profile.as_deref().unwrap_or("full"),
    );
    print_resolved_capabilities(&args);
    println!("---");
    if let Err(error) = runtime.send_message(args.ask.clone()) {
        eprintln!("cannot start turn: {error}");
        return 1;
    }
    let started = Instant::now();
    let mut seen = 0usize;
    let mut handled_suspensions = 0u32;
    loop {
        if started.elapsed() > args.timeout {
            eprintln!(
                "\ntimed out after {}s waiting for the turn",
                args.timeout.as_secs()
            );
            return 1;
        }
        let fresh: Vec<ipc_core::HostEvent> = {
            let log = events.lock().expect("event log lock");
            log.iter().skip(seen).cloned().collect()
        };
        seen += fresh.len();
        for event in fresh {
            match event.event.as_str() {
                "agent.turn_done" => {
                    println!(
                        "\n---\nANSWER:\n{}",
                        event
                            .data
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                    );
                    if let Some(steps) = event.data.get("executed").and_then(|v| v.as_array()) {
                        println!("---\nEXECUTED ({}):", steps.len());
                        for step in steps {
                            println!(
                                "  {} -> {}",
                                step.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                                step.get("output")
                                    .map(|o| o.to_string())
                                    .unwrap_or_default()
                            );
                        }
                    }
                    let _ = std::fs::remove_dir_all(&state_dir);
                    return 0;
                }
                "agent.turn_failed" => {
                    eprintln!(
                        "\nturn failed: {}",
                        event
                            .data
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                    );
                    let _ = std::fs::remove_dir_all(&state_dir);
                    return 1;
                }
                "agent.turn_cancelled" => {
                    eprintln!("\nturn cancelled");
                    let _ = std::fs::remove_dir_all(&state_dir);
                    return 1;
                }
                "agent.turn_suspended" => {
                    handled_suspensions += 1;
                    let request_id = event
                        .data
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let pending = approvals.lock().expect("approvals lock").list();
                    if pending.is_empty() {
                        eprintln!("\nturn suspended with no pending request; cannot continue");
                        return 1;
                    }
                    let mut approved_all = true;
                    for request in &pending {
                        println!(
                            "\n[permission] {:?} on {:?}\n  reason: {}\n  id: {}",
                            request.capability, request.resource, request.reason, request.id
                        );
                        let approve = if request.requires_once {
                            println!("  (sensitive: one-shot approval only)");
                            args.auto_approve || confirm("approve once?")
                        } else if args.auto_approve {
                            true
                        } else {
                            confirm("approve for this session?")
                        };
                        if !approve {
                            approved_all = false;
                            break;
                        }
                        let lifetime = if request.requires_once {
                            policy_core::GrantLifetime::Once
                        } else {
                            policy_core::GrantLifetime::Session
                        };
                        if let Err(error) = approvals
                            .lock()
                            .expect("approvals lock")
                            .decide(&request.id, Some(lifetime))
                        {
                            eprintln!("cannot record approval: {error}");
                            return 1;
                        }
                    }
                    runtime.notify_decided(&request_id, approved_all);
                    if !approved_all {
                        println!("denied; waiting for the model to react...");
                    }
                    if handled_suspensions > 50 {
                        eprintln!("too many suspensions; aborting");
                        return 1;
                    }
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn main() {
    // Surface retry backoffs and provider diagnostics on stderr; silent
    // by default below warnings so normal output stays readable.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();
    std::process::exit(run(std::env::args().skip(1).collect()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn kilo_free_defaults_need_only_a_question() {
        let args = parse_args(&argv(&["--ask", "hi"])).unwrap();
        assert_eq!(args.provider, "kilo");
        assert_eq!(args.model, "kilo-auto/free");
        assert_eq!(args.base_url, KILO_BASE_URL);
        assert_eq!(args.vision, None);
        assert_eq!(args.profile, None);
        assert!(!args.auto_approve);
    }

    #[test]
    fn tool_profile_is_validated_up_front() {
        let args = parse_args(&argv(&["--ask", "hi", "--profile", "developer"])).unwrap();
        assert_eq!(args.profile, Some("developer".to_string()));
        assert!(parse_args(&argv(&["--ask", "hi", "--profile", "everything"])).is_err());
    }

    #[test]
    fn explicit_provider_needs_model_and_base_url() {
        assert!(parse_args(&argv(&["--ask", "hi", "--provider", "ollama"])).is_err());
        let args = parse_args(&argv(&[
            "--ask",
            "hi",
            "--provider",
            "ollama",
            "--model",
            "llava:13b",
            "--base-url",
            "http://localhost:11434/v1",
            "--vision",
            "on",
            "--yes",
            "--timeout-secs",
            "60",
        ]))
        .unwrap();
        assert_eq!(args.model, "llava:13b");
        assert_eq!(args.vision, Some(true));
        assert!(args.auto_approve);
        assert_eq!(args.timeout, Duration::from_secs(60));
    }

    #[test]
    fn bad_flags_fail_closed() {
        assert!(parse_args(&argv(&[])).is_err());
        assert!(parse_args(&argv(&["--ask", "  "])).is_err());
        assert!(parse_args(&argv(&["--ask", "hi", "--vision", "maybe"])).is_err());
        assert!(parse_args(&argv(&["--ask", "hi", "--nope"])).is_err());
    }

    #[test]
    fn vision_flag_defaults_to_auto_discovery() {
        // `auto` (and the default) means API-discovered; `on`/`off` are
        // debug overrides that force image sending on or off.
        assert_eq!(parse_args(&argv(&["--ask", "hi"])).unwrap().vision, None);
        assert_eq!(
            parse_args(&argv(&["--ask", "hi", "--vision", "auto"]))
                .unwrap()
                .vision,
            None
        );
        assert_eq!(vision_mode(None), "auto");
        assert_eq!(vision_mode(Some(true)), "on");
        assert_eq!(vision_mode(Some(false)), "off");
        assert!(usage().contains("--vision auto"));
    }
}
