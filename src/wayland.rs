//! Conexión Wayland y estado compartido: captura workspaces + ventanas y
//! conserva los proxies vivos que necesita la restauración.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wayland_client::{
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
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
    toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{
        self, ZcosmicToplevelManagerV1,
    },
    workspace::v2::client::{
        zcosmic_workspace_handle_v2::{self, ZcosmicWorkspaceHandleV2},
        zcosmic_workspace_manager_v2::{self, ZcosmicWorkspaceManagerV2},
    },
};

// valores de los enums de los protocolos (ver XML en experiments/cosmic-protocols)
pub const TL_STATE_MAXIMIZED: u32 = 0;
pub const TL_STATE_MINIMIZED: u32 = 1;
pub const TL_STATE_FULLSCREEN: u32 = 3;
pub const TL_STATE_STICKY: u32 = 4;
pub const WS_EXT_ACTIVE: u32 = 1;
pub const WS_COSMIC_PINNED: u32 = 1;

// ---------------------------------------------------------------- modelo vivo

#[derive(Debug, Default, Clone)]
pub struct ToplevelData {
    pub identifier: String,
    pub app_id: String,
    pub title: String,
    pub states: Vec<u32>,
    /// output -> (x, y, w, h)
    pub geometry: HashMap<String, (i32, i32, i32, i32)>,
    pub outputs: Vec<String>,
    /// claves de `State::workspaces`
    pub workspaces: Vec<String>,
    pub done: bool,
}

pub struct ToplevelEntry {
    #[allow(dead_code)]
    pub ext: ExtForeignToplevelHandleV1,
    pub cosmic: Option<ZcosmicToplevelHandleV1>,
    pub data: ToplevelData,
}

#[derive(Debug, Default, Clone)]
pub struct WorkspaceData {
    pub id: Option<String>,
    pub name: String,
    pub coordinates: Vec<u32>,
    pub ext_state: u32,
    pub cosmic_state: u32,
    pub tiling: Option<u32>,
    pub group: Option<String>,
}

pub struct WorkspaceEntry {
    pub ext: ExtWorkspaceHandleV1,
    pub cosmic: Option<ZcosmicWorkspaceHandleV2>,
    pub data: WorkspaceData,
}

#[derive(Default)]
pub struct State {
    pub toplevel_info: Option<ZcosmicToplevelInfoV1>,
    pub toplevel_mgr: Option<ZcosmicToplevelManagerV1>,
    pub workspace_cosmic: Option<ZcosmicWorkspaceManagerV2>,
    pub workspace_mgr: Option<ExtWorkspaceManagerV1>,

    // objetos vivos, indexados por su ObjectId serializado a String
    pub toplevels: HashMap<String, ToplevelEntry>,
    pub workspaces: HashMap<String, WorkspaceEntry>,
    pub outputs: HashMap<String, (WlOutput, String)>,

    // handles pendientes de pedirles la extensión cosmic
    pending_toplevels: Vec<ExtForeignToplevelHandleV1>,
    pending_workspaces: Vec<ExtWorkspaceHandleV1>,

    // mapeo extensión cosmic -> handle ext (para enrutar sus eventos)
    cosmic_toplevel_map: HashMap<String, String>,
    cosmic_workspace_map: HashMap<String, String>,

    pub groups: HashMap<String, (ExtWorkspaceGroupHandleV1, Vec<String>)>,
    pub toplevels_done: bool,
}

pub fn key<P: Proxy>(p: &P) -> String {
    format!("{:?}", p.id())
}

impl State {
    pub fn workspace_by_name(&self, name: &str) -> Option<&WorkspaceEntry> {
        self.workspaces.values().find(|w| w.data.name == name)
    }

    pub fn output_by_name(&self, name: &str) -> Option<&WlOutput> {
        self.outputs
            .values()
            .find(|(_, n)| n == name)
            .map(|(o, _)| o)
    }

    pub fn any_output(&self) -> Option<&WlOutput> {
        self.outputs.values().next().map(|(o, _)| o)
    }

    fn output_name(&self, output: &WlOutput) -> String {
        self.outputs
            .get(&key(output))
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| key(output))
    }
}

/// Crea los objetos de extensión cosmic para los handles ext recién llegados.
pub fn pump_cosmic(state: &mut State, qh: &QueueHandle<State>) {
    if let Some(info) = state.toplevel_info.clone() {
        for h in std::mem::take(&mut state.pending_toplevels) {
            let c = info.get_cosmic_toplevel(&h, qh, ());
            state.cosmic_toplevel_map.insert(key(&c), key(&h));
            if let Some(e) = state.toplevels.get_mut(&key(&h)) {
                e.cosmic = Some(c);
            }
        }
    }
    if let Some(wmgr) = state.workspace_cosmic.clone() {
        for h in std::mem::take(&mut state.pending_workspaces) {
            let c = wmgr.get_cosmic_workspace(&h, qh, ());
            state.cosmic_workspace_map.insert(key(&c), key(&h));
            if let Some(e) = state.workspaces.get_mut(&key(&h)) {
                e.cosmic = Some(c);
            }
        }
    }
}

/// Conecta al compositor y sincroniza el estado inicial completo.
pub fn connect() -> (Connection, EventQueue<State>, QueueHandle<State>, State) {
    let conn = Connection::connect_to_env().expect("no hay sesión Wayland");
    let display = conn.display();
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut state = State::default();

    // 1) globals  2) objetos base (outputs, toplevels ext, workspaces ext)
    queue.roundtrip(&mut state).unwrap();
    queue.roundtrip(&mut state).unwrap();

    // 3) extensiones cosmic para cada objeto base
    pump_cosmic(&mut state, &qh);

    // 4) respuestas de las extensiones, hasta el `done` del info
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline && !state.toplevels_done {
        queue.roundtrip(&mut state).unwrap();
    }

    (conn, queue, qh, state)
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
                "zcosmic_toplevel_manager_v1" => {
                    state.toplevel_mgr = Some(registry.bind::<ZcosmicToplevelManagerV1, _, _>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    ));
                }
                "ext_workspace_manager_v1" => {
                    state.workspace_mgr = Some(registry.bind::<ExtWorkspaceManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }
                "zcosmic_workspace_manager_v2" => {
                    state.workspace_cosmic =
                        Some(registry.bind::<ZcosmicWorkspaceManagerV2, _, _>(
                            name,
                            version.min(2),
                            qh,
                            (),
                        ));
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
            state.outputs.insert(key(output), (output.clone(), name));
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
            state.toplevels.insert(
                key(&toplevel),
                ToplevelEntry {
                    ext: toplevel.clone(),
                    cosmic: None,
                    data: ToplevelData::default(),
                },
            );
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
        let Some(e) = state.toplevels.get_mut(&k) else { return };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => e.data.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => e.data.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                e.data.identifier = identifier
            }
            ext_foreign_toplevel_handle_v1::Event::Done => e.data.done = true,
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

impl Dispatch<ZcosmicToplevelManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelManagerV1,
        _: zcosmic_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
        let Some(ext_key) = state.cosmic_toplevel_map.get(&key(handle)).cloned() else { return };

        match event {
            zcosmic_toplevel_handle_v1::Event::State { state: s } => {
                let v = s
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                if let Some(e) = state.toplevels.get_mut(&ext_key) {
                    e.data.states = v;
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputEnter { output } => {
                let name = state.output_name(&output);
                if let Some(e) = state.toplevels.get_mut(&ext_key) {
                    if !e.data.outputs.contains(&name) {
                        e.data.outputs.push(name);
                    }
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputLeave { output } => {
                let name = state.output_name(&output);
                if let Some(e) = state.toplevels.get_mut(&ext_key) {
                    e.data.outputs.retain(|o| *o != name);
                }
            }
            zcosmic_toplevel_handle_v1::Event::Geometry {
                output,
                x,
                y,
                width,
                height,
            } => {
                let name = state.output_name(&output);
                if let Some(e) = state.toplevels.get_mut(&ext_key) {
                    e.data.geometry.insert(name, (x, y, width, height));
                }
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceEnter { workspace } => {
                let wk = key(&workspace);
                if let Some(e) = state.toplevels.get_mut(&ext_key) {
                    if !e.data.workspaces.contains(&wk) {
                        e.data.workspaces.push(wk);
                    }
                }
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceLeave { workspace } => {
                let wk = key(&workspace);
                if let Some(e) = state.toplevels.get_mut(&ext_key) {
                    e.data.workspaces.retain(|w| *w != wk);
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
                state
                    .groups
                    .insert(key(&workspace_group), (workspace_group, Vec::new()));
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.workspaces.insert(
                    key(&workspace),
                    WorkspaceEntry {
                        ext: workspace.clone(),
                        cosmic: None,
                        data: WorkspaceData::default(),
                    },
                );
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
                let name = state.output_name(&output);
                if let Some((_, outs)) = state.groups.get_mut(&g) {
                    outs.push(name);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if let Some(w) = state.workspaces.get_mut(&key(&workspace)) {
                    w.data.group = Some(g);
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
            ext_workspace_handle_v1::Event::Id { id } => w.data.id = Some(id),
            ext_workspace_handle_v1::Event::Name { name } => w.data.name = name,
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                w.data.coordinates = coordinates
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: s } => {
                w.data.ext_state = s.into();
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
                w.data.tiling = s.into_result().ok().map(|v| v as u32);
            }
            zcosmic_workspace_handle_v2::Event::State { state: s } => {
                w.data.cosmic_state = s.into();
            }
            _ => {}
        }
    }
}
