//! cosmic-workspace-saver: guarda y restaura la sesión de COSMIC
//! (workspaces, ventanas y su distribución).
//!
//!   cosmic-workspace-saver save    [--file PATH]
//!   cosmic-workspace-saver restore [--file PATH] [--timeout SEGS] [--dry-run]
//!   cosmic-workspace-saver dump

mod launch;
mod model;
mod wayland;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cosmic_protocols::workspace::v2::client::zcosmic_workspace_handle_v2::TilingState;
use wayland_client::Proxy;

use model::{SessionSnapshot, WindowSnap, WorkspaceSnap};
use wayland::{
    State, TL_STATE_FULLSCREEN, TL_STATE_MAXIMIZED, TL_STATE_MINIMIZED, TL_STATE_STICKY,
    WS_COSMIC_PINNED, WS_EXT_ACTIVE,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("save") => cmd_save(&args[1..]),
        Some("restore") => cmd_restore(&args[1..]),
        Some("dump") | None => cmd_dump(),
        Some(other) => {
            eprintln!("comando desconocido: {other}");
            eprintln!("uso: cosmic-workspace-saver [save|restore|dump] [--file PATH] [--timeout SEGS] [--dry-run]");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------- cli helpers

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn default_session_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(base).join("cosmic-workspace-saver/session.json")
}

fn session_path(args: &[String]) -> PathBuf {
    flag_value(args, "--file")
        .map(PathBuf::from)
        .unwrap_or_else(default_session_path)
}

// ---------------------------------------------------------------- save

fn build_snapshot(state: &State) -> SessionSnapshot {
    let mut workspaces: Vec<WorkspaceSnap> = state
        .workspaces
        .values()
        .map(|e| {
            let d = &e.data;
            WorkspaceSnap {
                name: d.name.clone(),
                coordinates: d.coordinates.clone(),
                tiling: d.tiling,
                active: d.ext_state & WS_EXT_ACTIVE != 0,
                pinned: d.cosmic_state & WS_COSMIC_PINNED != 0,
                output: d
                    .group
                    .as_ref()
                    .and_then(|g| state.groups.get(g))
                    .and_then(|(_, outs)| outs.first().cloned()),
            }
        })
        .collect();
    workspaces.sort_by(|a, b| a.coordinates.cmp(&b.coordinates));

    let mut windows: Vec<WindowSnap> = state
        .toplevels
        .values()
        // sin app_id no se puede relanzar; sin workspace ni output es una
        // ventana fantasma/oculta (p. ej. el proceso de fondo de Chrome)
        .filter(|e| !e.data.app_id.is_empty())
        .filter(|e| !e.data.workspaces.is_empty() || !e.data.outputs.is_empty())
        .map(|e| {
            let d = &e.data;
            let workspace = d
                .workspaces
                .first()
                .and_then(|k| state.workspaces.get(k))
                .map(|w| w.data.name.clone());
            let output = d.outputs.first().cloned();
            WindowSnap {
                app_id: d.app_id.clone(),
                title: d.title.clone(),
                workspace,
                geometry: output.as_ref().and_then(|o| d.geometry.get(o).copied()),
                output,
                maximized: d.states.contains(&TL_STATE_MAXIMIZED),
                minimized: d.states.contains(&TL_STATE_MINIMIZED),
                fullscreen: d.states.contains(&TL_STATE_FULLSCREEN),
                sticky: d.states.contains(&TL_STATE_STICKY),
            }
        })
        .collect();
    windows.sort_by(|a, b| (&a.app_id, &a.title).cmp(&(&b.app_id, &b.title)));

    // perfiles activos de los navegadores presentes en la sesión
    let mut browser_profiles = BTreeMap::new();
    for w in &windows {
        if let Some((binary, cfg)) = launch::browser_for(&w.app_id) {
            browser_profiles
                .entry(binary.to_string())
                .or_insert_with(|| launch::active_profiles(cfg));
        }
    }

    SessionSnapshot {
        saved_at_epoch: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        workspaces,
        windows,
        browser_profiles,
    }
}

fn cmd_save(args: &[String]) {
    let (_conn, _queue, _qh, state) = wayland::connect();
    let snap = build_snapshot(&state);

    let path = session_path(args);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("no se pudo crear el directorio de datos");
    }
    let json = serde_json::to_string_pretty(&snap).unwrap();
    std::fs::write(&path, json).expect("no se pudo escribir session.json");

    println!(
        "guardado: {} workspaces, {} ventanas -> {}",
        snap.workspaces.len(),
        snap.windows.len(),
        path.display()
    );
    for (browser, profiles) in &snap.browser_profiles {
        println!("  perfiles de {browser}: {profiles:?}");
    }
}

// ---------------------------------------------------------------- restore

struct Slot {
    win: WindowSnap,
    matched: bool,
}

fn cmd_restore(args: &[String]) {
    let path = session_path(args);
    let timeout = flag_value(args, "--timeout")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    let dry_run = args.iter().any(|a| a == "--dry-run");

    let txt = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no se pudo leer {}: {e}", path.display()));
    let snap: SessionSnapshot = serde_json::from_str(&txt).expect("session.json inválido");

    let (_conn, mut queue, qh, mut state) = wayland::connect();

    let mgr = state
        .toplevel_mgr
        .clone()
        .expect("falta zcosmic_toplevel_manager_v1 (¿compositor COSMIC?)");
    if mgr.version() < 4 {
        eprintln!(
            "[aviso] zcosmic_toplevel_manager_v1 v{} < 4: no se pueden mover ventanas de workspace",
            mgr.version()
        );
    }
    let ws_mgr = state
        .workspace_mgr
        .clone()
        .expect("falta ext_workspace_manager_v1");

    // ---- 1. workspaces: solo se pueden crear por adelantado los fijados.
    // COSMIC maneja los demás dinámicamente (no permite dos vacíos seguidos):
    // se crean solos cuando el anterior se puebla, así que los destinos
    // dinámicos se resuelven en cascada durante el loop de emparejamiento.
    let missing_pinned: Vec<WorkspaceSnap> = snap
        .workspaces
        .iter()
        .filter(|w| w.pinned && state.workspace_by_name(&w.name).is_none())
        .cloned()
        .collect();
    if !missing_pinned.is_empty() && !dry_run {
        for w in &missing_pinned {
            let group = w
                .output
                .as_ref()
                .and_then(|o| state.groups.values().find(|(_, outs)| outs.contains(o)))
                .or_else(|| state.groups.values().next());
            if let Some((g, _)) = group {
                println!("creando workspace fijado {:?}", w.name);
                g.create_workspace(w.name.clone());
            }
        }
        ws_mgr.commit();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && missing_pinned
                .iter()
                .any(|w| state.workspace_by_name(&w.name).is_none())
        {
            queue.roundtrip(&mut state).unwrap();
            wayland::pump_cosmic(&mut state, &qh);
            queue.roundtrip(&mut state).unwrap();
        }
    }

    // tiling/pin de los workspaces ya presentes; los dinámicos que aparezcan
    // durante el loop se configuran ahí mismo
    let mut ws_configured: HashSet<String> = HashSet::new();
    if !dry_run {
        apply_workspace_settings(&mut state, &snap, &mut ws_configured, &ws_mgr, &mut queue);
    }

    // ---- 2. lanzar aplicaciones que falten
    let mut live_counts: HashMap<String, usize> = HashMap::new();
    for e in state.toplevels.values() {
        if !e.data.app_id.is_empty() {
            *live_counts.entry(e.data.app_id.clone()).or_default() += 1;
        }
    }
    let mut saved_counts: HashMap<String, usize> = HashMap::new();
    for w in &snap.windows {
        *saved_counts.entry(w.app_id.clone()).or_default() += 1;
    }

    let mut launches: Vec<launch::Launch> = Vec::new();
    for (app_id, saved_n) in &saved_counts {
        let live_n = live_counts.get(app_id).copied().unwrap_or(0);
        if live_n >= *saved_n {
            continue;
        }
        if let Some((binary, _)) = launch::browser_for(app_id) {
            if live_n > 0 {
                // no sabemos qué perfil tienen las ventanas vivas; relanzar
                // perfiles duplicaría ventanas, mejor avisar y seguir
                eprintln!(
                    "[aviso] {app_id} ya tiene {live_n} ventana(s); no relanzo perfiles para no duplicar"
                );
                continue;
            }
            let profiles = snap
                .browser_profiles
                .get(binary)
                .cloned()
                .unwrap_or_else(|| vec!["Default".to_string()]);
            for p in profiles {
                launches.push(launch::Launch::BrowserProfile {
                    binary: binary.to_string(),
                    profile: p,
                });
            }
        } else if let Some(desktop) = launch::desktop_file_for(app_id) {
            // un lanzamiento por ventana faltante (p. ej. 2 terminales)
            for _ in 0..(saved_n - live_n) {
                launches.push(launch::Launch::Desktop(desktop.clone()));
            }
        } else {
            eprintln!("[aviso] sin .desktop para {app_id:?}; intento ejecutarlo directo");
            launches.push(launch::Launch::RawCommand(app_id.clone()));
        }
    }
    launches.sort();

    if dry_run {
        println!("== PLAN (dry-run) ==");
        for w in &missing_pinned {
            println!("  crear workspace fijado {:?}", w.name);
        }
        for w in snap
            .workspaces
            .iter()
            .filter(|w| !w.pinned && state.workspace_by_name(&w.name).is_none())
        {
            println!("  workspace {:?}: se creará en cascada al poblar los anteriores", w.name);
        }
        for l in &launches {
            println!("  lanzar: {}", l.describe());
        }
        println!(
            "  luego: mover {} ventanas a sus workspaces (timeout {timeout}s)",
            snap.windows.len()
        );
        return;
    }

    for l in &launches {
        println!("lanzando: {}", l.describe());
        if let Err(e) = l.spawn() {
            eprintln!("[aviso] falló el lanzamiento ({}): {e}", l.describe());
        }
    }

    // ---- 3. emparejar ventanas conforme aparecen y moverlas a su workspace
    let mut slots: Vec<Slot> = snap
        .windows
        .iter()
        .map(|w| Slot {
            win: w.clone(),
            matched: false,
        })
        .collect();
    let mut matched_live: HashSet<String> = HashSet::new();

    let deadline = Instant::now() + Duration::from_secs(timeout);
    // pasado este punto se acepta emparejar sin coincidencia de título
    let force_at = Instant::now() + Duration::from_secs((timeout * 2 / 3).max(10));

    loop {
        wayland::pump_cosmic(&mut state, &qh);
        queue.roundtrip(&mut state).unwrap();

        // configurar workspaces dinámicos que hayan aparecido en cascada
        apply_workspace_settings(&mut state, &snap, &mut ws_configured, &ws_mgr, &mut queue);

        let force = Instant::now() >= force_at;
        let pairs = match_pass(&state, &slots, &matched_live, force);
        for (ext_key, slot_idx) in pairs {
            if apply_placement(&state, &ext_key, &slots[slot_idx].win, force) {
                slots[slot_idx].matched = true;
                matched_live.insert(ext_key);
            }
        }

        if slots.iter().all(|s| s.matched) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    queue.roundtrip(&mut state).unwrap();

    // ---- 4. activar el workspace que estaba en foco
    if let Some(active) = snap.workspaces.iter().find(|w| w.active) {
        if let Some(entry) = state.workspace_by_name(&active.name) {
            entry.ext.activate();
            ws_mgr.commit();
            queue.roundtrip(&mut state).unwrap();
        }
    }

    let done = slots.iter().filter(|s| s.matched).count();
    println!("\nrestauradas {done}/{} ventanas", slots.len());
    for s in slots.iter().filter(|s| !s.matched) {
        println!("  pendiente: {} — {:?}", s.win.app_id, s.win.title);
    }
}

/// Aplica tiling y pin a los workspaces del snapshot que ya existen en vivo
/// y aún no fueron configurados. Se llama también dentro del loop porque los
/// workspaces dinámicos van apareciendo en cascada.
fn apply_workspace_settings(
    state: &mut State,
    snap: &SessionSnapshot,
    configured: &mut HashSet<String>,
    ws_mgr: &wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1,
    queue: &mut wayland_client::EventQueue<State>,
) {
    let mut dirty = false;
    for w in &snap.workspaces {
        if configured.contains(&w.name) {
            continue;
        }
        let Some(entry) = state.workspace_by_name(&w.name) else { continue };
        if let (Some(cosmic), Some(t)) = (&entry.cosmic, w.tiling) {
            if entry.data.tiling != Some(t) {
                let ts = if t == 1 {
                    TilingState::TilingEnabled
                } else {
                    TilingState::FloatingOnly
                };
                cosmic.set_tiling_state(ts);
                dirty = true;
            }
        }
        if w.pinned && entry.data.cosmic_state & WS_COSMIC_PINNED == 0 {
            if let Some(cosmic) = &entry.cosmic {
                cosmic.pin();
                dirty = true;
            }
        }
        configured.insert(w.name.clone());
    }
    if dirty {
        ws_mgr.commit();
        queue.roundtrip(state).unwrap();
    }
}

/// Empareja ventanas vivas sin asignar con slots guardados del mismo app_id:
/// título exacto > substring > candidato único > (en modo force) por orden.
fn match_pass(
    state: &State,
    slots: &[Slot],
    matched_live: &HashSet<String>,
    force: bool,
) -> Vec<(String, usize)> {
    // app_id -> claves de ventanas vivas candidatas
    let mut live: HashMap<&str, Vec<&String>> = HashMap::new();
    let mut live_keys: HashMap<String, &wayland::ToplevelEntry> = HashMap::new();
    for (k, e) in &state.toplevels {
        // sin handle cosmic todavía no se puede mover; esperar a la siguiente vuelta
        if e.data.done && e.cosmic.is_some() && !e.data.app_id.is_empty() && !matched_live.contains(k)
        {
            live.entry(e.data.app_id.as_str()).or_default().push(k);
            live_keys.insert(k.clone(), e);
        }
    }

    let mut pairs: Vec<(String, usize)> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();

    // el workspace destino todavía no existe (COSMIC lo creará en cascada):
    // no emparejar aún, salvo en modo force donde hay fallback de destino
    let blocked = |win: &WindowSnap| -> bool {
        !force
            && win
                .workspace
                .as_deref()
                .is_some_and(|n| state.workspace_by_name(n).is_none())
    };

    fn find_candidate(
        win: &WindowSnap,
        live: &HashMap<&str, Vec<&String>>,
        live_keys: &HashMap<String, &wayland::ToplevelEntry>,
        used: &HashSet<String>,
        pred: &dyn Fn(&str, &str) -> bool,
    ) -> Option<String> {
        live.get(win.app_id.as_str())?
            .iter()
            .find(|k| !used.contains(**k) && pred(&live_keys[**k].data.title, &win.title))
            .map(|k| (*k).clone())
    }

    // 1) título exacto
    for idx in 0..slots.len() {
        if slots[idx].matched {
            continue;
        }
        let win = &slots[idx].win;
        if blocked(win) {
            continue;
        }
        if let Some(k) = find_candidate(win, &live, &live_keys, &used, &|a, b| a == b) {
            used.insert(k.clone());
            pairs.push((k, idx));
        }
    }
    // 2) substring significativo (evita emparejar por títulos triviales)
    for idx in 0..slots.len() {
        if slots[idx].matched || pairs.iter().any(|(_, i)| *i == idx) {
            continue;
        }
        let win = &slots[idx].win;
        if blocked(win) {
            continue;
        }
        if let Some(k) = find_candidate(win, &live, &live_keys, &used, &|a, b| {
            a.len() >= 8 && b.len() >= 8 && (a.contains(b) || b.contains(a))
        }) {
            used.insert(k.clone());
            pairs.push((k, idx));
        }
    }
    // 3) candidato único / 4) forzado por orden
    for idx in 0..slots.len() {
        if slots[idx].matched || pairs.iter().any(|(_, i)| *i == idx) {
            continue;
        }
        let win = &slots[idx].win;
        if blocked(win) {
            continue;
        }
        let free: Vec<&String> = live
            .get(win.app_id.as_str())
            .map(|v| v.iter().filter(|k| !used.contains(**k)).copied().collect())
            .unwrap_or_default();
        let remaining_slots = slots
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                !s.matched && s.win.app_id == win.app_id && !pairs.iter().any(|(_, pi)| pi == i)
            })
            .count();
        let unambiguous = free.len() == 1 && remaining_slots == 1;
        if (unambiguous || force) && !free.is_empty() {
            let k = free[0].clone();
            used.insert(k.clone());
            pairs.push((k, idx));
        }
    }

    pairs
}

/// Mueve la ventana a su workspace y restaura sus estados.
/// Devuelve false si aún no se puede aplicar (reintentar en la siguiente vuelta).
fn apply_placement(state: &State, ext_key: &str, win: &WindowSnap, force: bool) -> bool {
    let Some(entry) = state.toplevels.get(ext_key) else { return false };
    let Some(cosmic) = &entry.cosmic else { return false };
    let Some(mgr) = &state.toplevel_mgr else { return false };

    let output = win
        .output
        .as_ref()
        .and_then(|o| state.output_by_name(o))
        .or_else(|| state.any_output());

    let mut placed_in = win.workspace.clone();
    if let Some(ws_name) = &win.workspace {
        let target = state.workspace_by_name(ws_name).or_else(|| {
            if force {
                // el destino nunca llegó a existir: mejor el último workspace
                // (el vacío final que COSMIC mantiene) que dejarla donde cayó
                let last = state
                    .workspaces
                    .values()
                    .max_by(|a, b| a.data.coordinates.cmp(&b.data.coordinates));
                placed_in = last.map(|w| w.data.name.clone());
                last
            } else {
                None
            }
        });
        let Some(ws) = target else { return false };

        // si ya está donde debe, no lo movemos
        let already = entry
            .data
            .workspaces
            .iter()
            .filter_map(|k| state.workspaces.get(k))
            .any(|w| w.data.name == ws.data.name);
        if !already {
            if let Some(out) = output {
                if mgr.version() >= 4 {
                    mgr.move_to_ext_workspace(cosmic, &ws.ext, out);
                }
            }
        }
    }

    // Estados (fullscreen/maximizado/etc.) NO se restauran a propósito:
    // set_fullscreen sobre un output fullscreenea en el workspace activo y
    // arrastra la ventana fuera de donde la acabamos de colocar. El autotiling
    // del workspace se encarga del acomodo.

    let note = if placed_in != win.workspace {
        format!(" (destino {:?} no existe)", win.workspace.as_deref().unwrap_or("?"))
    } else {
        String::new()
    };
    println!(
        "  ✓ {} — {:?} -> workspace {:?}{note}",
        win.app_id,
        win.title,
        placed_in.as_deref().unwrap_or("?")
    );
    true
}

// ---------------------------------------------------------------- dump

fn cmd_dump() {
    let (_conn, _queue, _qh, state) = wayland::connect();

    println!("== OUTPUTS ==");
    for (_, name) in state.outputs.values() {
        println!("  {name}");
    }

    println!("\n== WORKSPACES ==");
    let mut ws: Vec<_> = state.workspaces.iter().collect();
    ws.sort_by(|(_, a), (_, b)| a.data.coordinates.cmp(&b.data.coordinates));
    for (k, w) in &ws {
        let d = &w.data;
        let tiling = match d.tiling {
            Some(1) => "tiling",
            Some(0) => "floating",
            _ => "?",
        };
        println!(
            "  [{}] name={:?} id={:?} coords={:?} {} ext_state=0x{:x} cosmic_state=0x{:x}",
            k, d.name, d.id, d.coordinates, tiling, d.ext_state, d.cosmic_state
        );
    }

    println!("\n== VENTANAS ==");
    for e in state.toplevels.values() {
        let d = &e.data;
        println!("  {} — {:?}", d.app_id, d.title);
        println!("     identifier: {}", d.identifier);
        println!("     states:     {:?}", d.states);
        println!("     outputs:    {:?}", d.outputs);
        let names: Vec<_> = d
            .workspaces
            .iter()
            .map(|k| {
                state
                    .workspaces
                    .get(k)
                    .map(|w| w.data.name.clone())
                    .unwrap_or_else(|| k.clone())
            })
            .collect();
        println!("     workspaces: {:?}", names);
        println!("     geometry:   {:?}", d.geometry);
    }

    if !state.toplevels_done {
        eprintln!("\n[aviso] no llegó zcosmic_toplevel_info_v1.done");
    }
}
