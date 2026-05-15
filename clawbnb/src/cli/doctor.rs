//! `weclawbot doctor` — environment self-check.

use crate::sandbox;
use crate::skills::detect;

pub async fn run() -> Result<(), String> {
    println!("=== weclawbot doctor ===");

    let report = sandbox::preflight();

    print!("podman:               ");
    match &report.podman {
        Some(p) => println!("[ OK ] {} ({})", p.path.display(), p.version),
        None => println!("[FAIL] not in $PATH — `apt install podman`"),
    }

    print!("runsc (gVisor):       ");
    match &report.runsc {
        Some(r) if r.registered => println!("[ OK ] {} ({})", r.path.display(), r.version),
        Some(r) => println!(
            "[warn] {} ({}) — installed but NOT registered as podman runtime",
            r.path.display(),
            r.version
        ),
        None => println!("[FAIL] not in $PATH"),
    }

    print!("gVisor smoke test:    ");
    match report.runsc_smoke_ok {
        Some(true) => println!("[ OK ]"),
        Some(false) => println!("[FAIL] — see errors below"),
        None => println!("[skip]"),
    }

    print!("sandbox image:        ");
    if report.image_cached {
        println!("[ OK ] cached ({})", report.image_ref);
    } else {
        println!("[warn] not pulled yet ({})", report.image_ref);
    }

    print!("operator credentials: ");
    println!(
        "{}",
        if report.operator_credentials_present {
            "[ OK ] ~/.claude/.credentials.json present"
        } else {
            "[FAIL] missing — run `claude login`"
        }
    );

    print!("operator plugins dir: ");
    println!(
        "{}",
        if report.operator_plugins_present {
            "[ OK ] ~/.claude/plugins present (informational)"
        } else {
            "[warn] not found"
        }
    );

    // Defaults + discovered skills (operator-installed).
    println!();
    let plugins = detect::enabled_plugins();
    let skills = detect::discover_skills();
    println!("Operator-enabled plugins (informational, {}):", plugins.len());
    if plugins.is_empty() {
        println!("  (none — install in your interactive `claude` if desired)");
    } else {
        for p in &plugins {
            println!("  - {}@{}", p.plugin, p.marketplace);
        }
    }
    println!();
    println!("Discovered skills (informational, {}):", skills.len());
    for s in &skills {
        let desc: String = s.description.chars().take(80).collect();
        println!("  [{}::{}] {desc}", s.plugin, s.skill);
    }

    println!();
    let defaults = crate::defaults::load_defaults();
    let default_plugins = defaults
        .get("enabledPlugins")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    println!(
        "Defaults (new users get this on first sight): {} plugin(s), see {}",
        default_plugins,
        crate::sandbox::layout::defaults_settings_path().display()
    );

    let users = sandbox::list_user_hashes();
    println!("Existing users: {}", users.len());

    println!();
    if report.errors.is_empty() {
        println!("Status: READY");
        Ok(())
    } else {
        println!("Status: NOT READY ({} issue(s)):", report.errors.len());
        for e in &report.errors {
            println!("  ! {e}");
        }
        Err("doctor reported failures".into())
    }
}
