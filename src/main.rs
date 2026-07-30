// Volcado del estado de la sesión COSMIC: workspaces + ventanas + relación entre ambos.
//
//
// Ajusta el `path` a donde tengas clonado cosmic-protocols.

use std::collections::HashMap;

use wayland_client::{
    protocol::{wl_output, wl_registry},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
        ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
    },
    workspace::v1::client::{
        ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
        ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
        ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
    },
};

use cosmic_protocols::{
    toplevel_info::v1::client::{
        zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
        zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
    },
    workspace::v2::client::{
        zcosmic_workspace_handle_v2::{self, ZcosmicWorkspaceHandleV2},
        zcosmic_workspace_manager_v2::{self, ZcosmicWorkspaceManagerV2},
    },
};

// ---------------------------------------------------------------- modelo

#[derive(Debug, Default, Clone)]
struct Toplevel {
    identifier: String,
    app_id: String,
    title: String,
    states: Vec<u32>,
    /// output -> (x, y, w, h)
    geometry: HashMap<String, (i32, i32, i32, i32)>,
    outputs: Vec<String>,
    /// claves de `State::workspaces`
    workspaces: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct Workspace {
    id: Option<String>,
    name: String,
    coordinates: Vec<u32>,
    ext_state: u32,
    cosmic_state: u32,
    tiling: Option<u32>,
    capabilities: u32,
    cosmic_capabilities: u32,
    group: Option<String>,
}

#[derive(Default)]
struct State {
    // globals
    toplevel_info: Option<ZcosmicToplevelInfoV1>,
    workspace_cosmic: Option<ZcosmicWorkspaceManagerV2>,

    // objetos vivos, indexados por su ObjectId serializado a String
    toplevels: HashMap<String, Toplevel>,
    workspaces: HashMap<String, Workspace>,
    outputs: HashMap<String, String>, // object id -> nombre del output

    // handles pendientes de pedirles la extensión cosmic
    pending_toplevels: Vec<ExtForeignToplevelHandleV1>,
    pending_workspaces: Vec<ExtWorkspaceHandleV1>,

    // mapeo extensión cosmic -> handle ext (para enrutar sus eventos)
    cosmic_toplevel_map: HashMap<String, String>,
    cosmic_workspace_map: HashMap<String, String>,

    groups: HashMap<String, Vec<String>>, // grupo -> outputs
    toplevels_done: bool,
}

fn key<P: Proxy>(p: &P) -> String {
    format!("{:?}", p.id())
}

// ---------------------------------------------------------------- registry

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "ext_foreign_toplevel_list_v1" => {
                    registry.bind::<ExtForeignToplevelListV1, _, _>(name, version.min(1), qh, ());
                }
                "zcosmic_toplevel_info_v1" => {
                    state.toplevel_info = Some(registry.bind::<ZcosmicToplevelInfoV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    ));
                }
                "ext_workspace_manager_v1" => {
                    registry.bind::<ExtWorkspaceManagerV1, _, _>(name, version.min(1), qh, ());
                }
                "zcosmic_workspace_manager_v2" => {
                    state.workspace_cosmic = Some(
                        registry.bind::<ZcosmicWorkspaceManagerV2, _, _>(name, version.min(2), qh, ()),
                    );
                }
                "wl_output" => {
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs.insert(key(output), name);
        }
    }
}

// ---------------------------------------------------------------- toplevels (ext)

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.insert(key(&toplevel), Toplevel::default());
            state.pending_toplevels.push(toplevel);
        }
    }

    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let k = key(handle);
        let Some(t) = state.toplevels.get_mut(&k) else { return };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => t.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                t.identifier = identifier
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&k);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- toplevels (cosmic)

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_info_v1::Event::Done = event {
            state.toplevels_done = true;
        }
    }
}

impl Dispatch<ZcosmicToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let k = key(handle);
        eprintln!("[cosmic toplevel] {k} -> {:?} | {event:?}", state.cosmic_toplevel_map.get(&k));
        let Some(ext_key) = state.cosmic_toplevel_map.get(&key(handle)).cloned() else { return };

        match event {
            zcosmic_toplevel_handle_v1::Event::State { state: s } => {
                let v = s
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                if let Some(t) = state.toplevels.get_mut(&ext_key) {
                    t.states = v;
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputEnter { output } => {
                let name = state
                    .outputs
                    .get(&key(&output))
                    .cloned()
                    .unwrap_or_else(|| key(&output));
                if let Some(t) = state.toplevels.get_mut(&ext_key) {
                    t.outputs.push(name);
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputLeave { output } => {
                let name = state
                    .outputs
                    .get(&key(&output))
                    .cloned()
                    .unwrap_or_else(|| key(&output));
                if let Some(t) = state.toplevels.get_mut(&ext_key) {
                    t.outputs.retain(|o| *o != name);
                }
            }
            zcosmic_toplevel_handle_v1::Event::Geometry {
                output,
                x,
                y,
                width,
                height,
            } => {
                let name = state
                    .outputs
                    .get(&key(&output))
                    .cloned()
                    .unwrap_or_else(|| key(&output));
                if let Some(t) = state.toplevels.get_mut(&ext_key) {
                    t.geometry.insert(name, (x, y, width, height));
                }
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceEnter { workspace } => {
                let wk = key(&workspace);
                if let Some(t) = state.toplevels.get_mut(&ext_key) {
                    t.workspaces.push(wk);
                }
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceLeave { workspace } => {
                let wk = key(&workspace);
                if let Some(t) = state.toplevels.get_mut(&ext_key) {
                    t.workspaces.retain(|w| *w != wk);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- workspaces (ext)

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.groups.insert(key(&workspace_group), Vec::new());
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.workspaces.insert(key(&workspace), Workspace::default());
                state.pending_workspaces.push(workspace);
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let g = key(group);
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                let name = state
                    .outputs
                    .get(&key(&output))
                    .cloned()
                    .unwrap_or_else(|| key(&output));
                state.groups.entry(g).or_default().push(name);
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if let Some(w) = state.workspaces.get_mut(&key(&workspace)) {
                    w.group = Some(g);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let k = key(handle);
        let Some(w) = state.workspaces.get_mut(&k) else { return };
        match event {
            ext_workspace_handle_v1::Event::Id { id } => w.id = Some(id),
            ext_workspace_handle_v1::Event::Name { name } => w.name = name,
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                w.coordinates = coordinates
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: s } => {
                w.ext_state = s.into();
            }
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => {
                w.capabilities = capabilities.into();
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(&k);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- workspaces (cosmic)

impl Dispatch<ZcosmicWorkspaceManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicWorkspaceManagerV2,
        _: zcosmic_workspace_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZcosmicWorkspaceHandleV2, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZcosmicWorkspaceHandleV2,
        event: zcosmic_workspace_handle_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(ext_key) = state.cosmic_workspace_map.get(&key(handle)).cloned() else { return };
        let Some(w) = state.workspaces.get_mut(&ext_key) else { return };
        match event {
            zcosmic_workspace_handle_v2::Event::TilingState { state: s } => {
                w.tiling = s.into_result().ok().map(|v| v as u32);
            }
            zcosmic_workspace_handle_v2::Event::State { state: s } => {
                w.cosmic_state = s.into();
            }
            zcosmic_workspace_handle_v2::Event::Capabilities { capabilities } => {
                w.cosmic_capabilities = capabilities.into();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let conn = Connection::connect_to_env().expect("no hay sesión Wayland");
    let display = conn.display();
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut state = State::default();

    // 1) globals
    queue.roundtrip(&mut state).unwrap();
    // 2) objetos base: outputs, toplevels ext, workspaces ext
    queue.roundtrip(&mut state).unwrap();

    // 3) pedir las extensiones cosmic para cada objeto base
    let info = state.toplevel_info.clone().expect("falta zcosmic_toplevel_info_v1");
    for h in std::mem::take(&mut state.pending_toplevels) {
        let c = info.get_cosmic_toplevel(&h, &qh, ());
        state.cosmic_toplevel_map.insert(key(&c), key(&h));
    }
    let wmgr = state
        .workspace_cosmic
        .clone()
        .expect("falta zcosmic_workspace_manager_v2");
    for h in std::mem::take(&mut state.pending_workspaces) {
        let c = wmgr.get_cosmic_workspace(&h, &qh, ());
        state.cosmic_workspace_map.insert(key(&c), key(&h));
    }

    // 4) recoger lo que responden las extensiones, hasta el `done` del info
        queue.flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline && !state.toplevels_done {
            queue.blocking_dispatch(&mut state).unwrap();
        }
    // ------------------------------------------------------------ salida

    println!("== OUTPUTS ==");
    for name in state.outputs.values() {
        println!("  {name}");
    }

    println!("\n== WORKSPACES ==");
    let mut ws: Vec<_> = state.workspaces.iter().collect();
    ws.sort_by_key(|(_, w)| w.coordinates.clone());
    for (k, w) in &ws {
        let tiling = match w.tiling {
            Some(1) => "tiling",
            Some(0) => "floating",
            _ => "?",
        };
        println!(
            "  [{}] name={:?} id={:?} coords={:?} {} ext_state=0x{:x} cosmic_state=0x{:x}",
            k, w.name, w.id, w.coordinates, tiling, w.ext_state, w.cosmic_state
        );
    }

    println!("\n== VENTANAS ==");
    for (_, t) in &state.toplevels {
        println!("  {} — {:?}", t.app_id, t.title);
        println!("     identifier: {}", t.identifier);
        println!("     states:     {:?}", t.states);
        println!("     outputs:    {:?}", t.outputs);
        let names: Vec<_> = t
            .workspaces
            .iter()
            .map(|k| {
                state
                    .workspaces
                    .get(k)
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| k.clone())
            })
            .collect();
        println!("     workspaces: {:?}", names);
        println!("     geometry:   {:?}", t.geometry);
    }

    if !state.toplevels_done {
        eprintln!("\n[aviso] no llegó zcosmic_toplevel_info_v1.done");
    }
}