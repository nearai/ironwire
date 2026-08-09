//! `ironwire service` — run the daemon in the background.
//!
//! Three platforms, three mechanisms, one rule: **always a user agent, never a
//! system service.** The daemon reads the OAuth tokens Claude Code and Codex
//! stored in this user's home directory and binds loopback only. A root-run
//! system service reaching into a user's credential files is a category error,
//! and there is deliberately no flag to ask for one.
//!
//! Nothing here is required. `ironwire serve` in the foreground stays
//! first-class — it is what the installer tells you to run and what `doctor`
//! assumes.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// What the current platform uses to keep a user agent running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Manager {
    /// macOS launchd, per-user domain.
    Launchd,
    /// systemd, `--user` scope.
    SystemdUser,
    /// Windows Task Scheduler, at logon.
    SchTasks,
}

impl Manager {
    /// Detect the mechanism for this machine.
    fn detect() -> Option<Self> {
        if cfg!(target_os = "macos") {
            return Some(Self::Launchd);
        }
        if cfg!(target_os = "windows") {
            return Some(Self::SchTasks);
        }
        if cfg!(target_os = "linux") {
            // `systemctl --user` needs a user bus. In a container or over a
            // bare SSH session there often is not one, and pretending
            // otherwise produces a baffling failure at enable time.
            return std::path::Path::new("/run/systemd/system")
                .exists()
                .then_some(Self::SystemdUser);
        }
        None
    }

    fn label(self) -> &'static str {
        match self {
            Self::Launchd => "launchd (user agent)",
            Self::SystemdUser => "systemd (user unit)",
            Self::SchTasks => "Task Scheduler (at logon)",
        }
    }
}

/// Run `ironwire service <action>`.
pub(crate) fn run(action: &str, port: Option<u16>) -> Result<()> {
    let Some(manager) = Manager::detect() else {
        return Err(unsupported());
    };
    match action {
        "install" => install(manager, port),
        "uninstall" => uninstall(manager),
        "status" => status(manager),
        other => bail!("unknown action `{other}` (try: install, uninstall, status)"),
    }
}

fn unsupported() -> anyhow::Error {
    anyhow::anyhow!(
        "no supported service manager here.\n\
         \n\
         On Linux this usually means there is no systemd user bus — common in\n\
         containers and over a bare SSH session.\n\
         \n\
         Run it in the foreground instead, or under whatever supervisor you\n\
         already use:\n\
         \n\
             ironwire serve"
    )
}

fn binary() -> Result<PathBuf> {
    // The absolute path of *this* binary. A service unit that says `ironwire`
    // depends on a PATH the supervisor may not have, which is the classic way
    // a unit that works interactively fails at boot.
    std::env::current_exe().context("locating the ironwire binary")
}

fn install(manager: Manager, port: Option<u16>) -> Result<()> {
    let exe = binary()?;
    println!("Installing IronWire as a {}.", manager.label());
    println!();

    match manager {
        Manager::Launchd => install_launchd(&exe, port),
        Manager::SystemdUser => install_systemd(&exe, port),
        Manager::SchTasks => install_schtasks(&exe, port),
    }
}

// ---------------------------------------------------------------- launchd

fn launchd_plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("locating your home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join("dev.ironwire.daemon.plist"))
}

fn install_launchd(exe: &std::path::Path, port: Option<u16>) -> Result<()> {
    let path = launchd_plist_path()?;
    let home = dirs::home_dir().context("locating your home directory")?;
    let logs = home.join("Library").join("Logs");
    let mut args = format!(
        "    <string>{}</string>\n    <string>serve</string>\n",
        xml_escape(&exe.display().to_string())
    );
    if let Some(port) = port {
        args.push_str(&format!(
            "    <string>--port</string>\n    <string>{port}</string>\n"
        ));
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.ironwire.daemon</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(&logs.join("ironwire.log").display().to_string()),
    );

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::create_dir_all(&logs).ok();
    announce_write(&path);
    std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

    println!();
    println!("Start it now:");
    println!("    launchctl load -w {}", path.display());
    println!();
    println!("Logs: {}", logs.join("ironwire.log").display());
    Ok(())
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------- systemd

fn systemd_unit_path() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .or_else(dirs::home_dir)
        .context("locating your config directory")?;
    Ok(base.join("systemd").join("user").join("ironwire.service"))
}

fn install_systemd(exe: &std::path::Path, port: Option<u16>) -> Result<()> {
    let path = systemd_unit_path()?;
    let port_arg = port.map_or_else(String::new, |p| format!(" --port {p}"));

    // The hardening directives are cheap and they bound the blast radius of a
    // bug in a process that holds credentials. `ProtectHome=read-write` rather
    // than `true`, because reading `~/.claude` and `~/.codex` is the job.
    let unit = format!(
        "# Written by `ironwire service install`.\n\
         #\n\
         # A **user** unit. The daemon reads credentials from this user's home\n\
         # directory and binds 127.0.0.1 only; running it as root would give a\n\
         # system service access to a user's OAuth tokens.\n\
         \n\
         [Unit]\n\
         Description=IronWire — localhost router for coding agents\n\
         Documentation=https://github.com/nearai/ironwire\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe}{port_arg} serve\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         ProtectSystem=strict\n\
         ProtectHome=read-write\n\
         ProtectKernelTunables=true\n\
         ProtectKernelModules=true\n\
         RestrictNamespaces=true\n\
         RestrictSUIDSGID=true\n\
         LockPersonality=true\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    );

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    announce_write(&path);
    std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;

    println!();
    println!("Start it now:");
    println!("    systemctl --user daemon-reload");
    println!("    systemctl --user enable --now ironwire");
    println!();
    println!("To keep it running while you are logged out:");
    println!("    loginctl enable-linger $USER");
    println!();
    println!("Logs: journalctl --user -u ironwire -f");
    Ok(())
}

// ---------------------------------------------------------------- schtasks

fn install_schtasks(exe: &std::path::Path, port: Option<u16>) -> Result<()> {
    let port_arg = port.map_or_else(String::new, |p| format!(" --port {p}"));
    println!("Run this to register a logon task:");
    println!();
    println!(
        "    schtasks /Create /TN IronWire /SC ONLOGON /TR \"\\\"{}\\\"{port_arg} serve\" /F",
        exe.display()
    );
    println!();
    println!("It runs as you, not as SYSTEM — IronWire holds your credentials");
    println!("and must not run with more privilege than you have.");
    Ok(())
}

// ---------------------------------------------------------------- teardown

fn uninstall(manager: Manager) -> Result<()> {
    match manager {
        Manager::Launchd => {
            let path = launchd_plist_path()?;
            println!("    launchctl unload -w {}", path.display());
            remove_if_present(&path)
        }
        Manager::SystemdUser => {
            let path = systemd_unit_path()?;
            println!("    systemctl --user disable --now ironwire");
            remove_if_present(&path)?;
            println!("    systemctl --user daemon-reload");
            Ok(())
        }
        Manager::SchTasks => {
            println!("    schtasks /Delete /TN IronWire /F");
            Ok(())
        }
    }
}

fn remove_if_present(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        println!("Removed {}", path.display());
    } else {
        println!("{} does not exist — nothing to remove.", path.display());
    }
    Ok(())
}

fn status(manager: Manager) -> Result<()> {
    println!("Service manager: {}", manager.label());
    let installed = match manager {
        Manager::Launchd => Some(launchd_plist_path()?),
        Manager::SystemdUser => Some(systemd_unit_path()?),
        Manager::SchTasks => None,
    };
    match installed {
        Some(path) if path.exists() => println!("Installed:       {}", path.display()),
        Some(path) => {
            println!("Installed:       no ({} does not exist)", path.display());
            println!();
            println!("    ironwire service install");
        }
        None => println!("Check with:      schtasks /Query /TN IronWire"),
    }
    Ok(())
}

/// TRUST.md: name the file before touching it, every time.
fn announce_write(path: &std::path::Path) {
    println!("Writing {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plist_escapes_a_path_that_would_break_the_xml() {
        // Homes with an `&` in them are rare and the failure is total: launchd
        // silently refuses to load a malformed plist.
        assert_eq!(
            xml_escape("/Users/a&b/<bin>/ironwire"),
            "/Users/a&amp;b/&lt;bin&gt;/ironwire"
        );
    }

    #[test]
    fn every_action_is_recognised_or_named() {
        // A typo should say what the options are, not fail obscurely.
        let err = run("strat", None).unwrap_err().to_string();
        assert!(
            err.contains("install") || err.contains("service manager"),
            "got: {err}"
        );
    }
}
