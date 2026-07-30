//! Relanzamiento de aplicaciones: resolución de archivos .desktop y manejo
//! especial de navegadores Chromium (perfiles vía --profile-directory).

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// (app_id de la ventana principal, binario, dir de config relativo a $HOME)
const BROWSERS: &[(&str, &str, &str)] = &[
    ("google-chrome", "google-chrome", ".config/google-chrome"),
    ("chromium", "chromium", ".config/chromium"),
    ("brave-browser", "brave-browser", ".config/BraveSoftware/Brave-Browser"),
];

pub fn browser_for(app_id: &str) -> Option<(&'static str, &'static str)> {
    BROWSERS
        .iter()
        .find(|(id, _, _)| *id == app_id)
        .map(|(_, bin, cfg)| (*bin, *cfg))
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Perfiles activos en la última sesión del navegador, leídos de `Local State`.
pub fn active_profiles(config_dir_rel: &str) -> Vec<String> {
    let path = PathBuf::from(home()).join(config_dir_rel).join("Local State");
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return Vec::new();
    };
    let profile = &json["profile"];
    if let Some(arr) = profile["last_active_profiles"].as_array() {
        let v: Vec<String> = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(last) = profile["last_used"].as_str() {
        return vec![last.to_string()];
    }
    vec!["Default".to_string()]
}

fn application_dirs() -> Vec<PathBuf> {
    let home = home();
    vec![
        PathBuf::from(format!("{home}/.local/share/applications")),
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from(format!(
            "{home}/.local/share/flatpak/exports/share/applications"
        )),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ]
}

/// Busca `{app_id}.desktop` (exacto y luego case-insensitive). Cubre también
/// las PWAs de Chrome/Brave, que instalan su propio .desktop con el app_id.
pub fn desktop_file_for(app_id: &str) -> Option<PathBuf> {
    let dirs = application_dirs();
    for d in &dirs {
        let p = d.join(format!("{app_id}.desktop"));
        if p.exists() {
            return Some(p);
        }
    }
    let wanted = format!("{}.desktop", app_id.to_lowercase());
    for d in &dirs {
        let Ok(entries) = std::fs::read_dir(d) else { continue };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().to_lowercase() == wanted {
                return Some(entry.path());
            }
        }
    }
    // último recurso: apps cuyo app_id no coincide con el nombre del archivo
    // (p. ej. AppImages); buscar por StartupWMClass y luego por Name
    for key in ["StartupWMClass", "Name"] {
        for d in &dirs {
            let Ok(entries) = std::fs::read_dir(d) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "desktop") {
                    continue;
                }
                let Ok(txt) = std::fs::read_to_string(&path) else { continue };
                let hit = txt.lines().any(|l| {
                    l.strip_prefix(key)
                        .and_then(|r| r.trim_start().strip_prefix('='))
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case(app_id))
                });
                if hit {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn have_systemd_run() -> bool {
    Command::new("systemd-run")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Línea Exec= del .desktop, sin los field codes (%u, %F, ...).
fn exec_from_desktop(path: &std::path::Path) -> Option<String> {
    let txt = std::fs::read_to_string(path).ok()?;
    let exec = txt
        .lines()
        .find_map(|l| l.strip_prefix("Exec="))?
        .split_whitespace()
        .filter(|tok| !tok.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ");
    (!exec.is_empty()).then_some(exec)
}

/// Ejecuta `sh -c "exec <cmdline>"` en su propia unidad systemd, para que NO
/// quede en el cgroup de la terminal: si no, al cerrar la terminal que corrió
/// `restore`, systemd limpia el scope entero y se cierra todo lo restaurado.
fn spawn_detached(cmdline: &str) -> std::io::Result<()> {
    let sh_arg = format!("exec {cmdline}");
    let mut cmd = if have_systemd_run() {
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--collect", "--quiet", "sh", "-c", &sh_arg]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &sh_arg]);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Un lanzamiento planificado, mostrable y ejecutable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Launch {
    Desktop(PathBuf),
    BrowserProfile { binary: String, profile: String },
    RawCommand(String),
}

impl Launch {
    fn cmdline(&self) -> String {
        match self {
            Launch::Desktop(p) => exec_from_desktop(p)
                .unwrap_or_else(|| format!("gio launch '{}'", p.display())),
            Launch::BrowserProfile { binary, profile } => {
                format!("{binary} \"--profile-directory={profile}\"")
            }
            Launch::RawCommand(c) => format!("'{c}'"),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Launch::Desktop(p) => format!("{}  [{}]", self.cmdline(), p.display()),
            _ => self.cmdline(),
        }
    }

    pub fn spawn(&self) -> std::io::Result<()> {
        spawn_detached(&self.cmdline())
    }
}
