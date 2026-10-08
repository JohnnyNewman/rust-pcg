//! # pcg — the IDE shell
//!
//! Bevy owns scheduling, input and the window; egui draws panels and the graph
//! canvas. All analysis lives in the headless crates.
//!
//! Data (resources in [`model`]) is separated from control flow (systems):
//!
//! | system            | schedule               | reads → writes                          |
//! |-------------------|------------------------|-----------------------------------------|
//! | `watch::poll`     | Update                 | `Watch` → `LoadRequest` (debounced)     |
//! | `edit::poll`      | Update                 | `Editing` → `LoadRequest` (debounced live rebuild) |
//! | `lsp::drive`      | Update                 | `Project`, worker thread ⇄ `Lsp` → `LoadRequest` (answers arrived) |
//! | `viewstate::poll` | Update                 | `ViewState`, `View` → `LoadRequest` (relayout), the sidecar file |
//! | `load::start`     | Update                 | `LoadRequest`, `Cache`, `Editing` (overlay), `Lsp` (answers), `ViewState` → `LoadTask` |
//! | `load::poll`      | Update                 | `LoadTask` → `Project`, `Transition`, `View`, `Selection`, `Watch`, `Editing` |
//! | `anim::end`       | Update                 | `Time` → `Transition` (drops old snapshot) |
//! | `view::animate`   | Update                 | `Time` → `View`                         |
//! | `ui::ui`          | EguiPrimaryContextPass | everything → `View`, `Selection`, `UiState`, `Editing`, `ViewState`, `LoadRequest` |

mod anim;
mod canvas;
mod edit;
mod load;
mod lsp;
mod model;
mod theme;
mod ui;
mod view;
mod viewstate;
mod watch;

use bevy::prelude::*;
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
use model::*;

fn main() {
    // `pcg [--no-lsp] [path]`
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_lsp = args.iter().any(|a| a == "--no-lsp");
    let path: std::path::PathBuf = args.iter().find(|a| !a.starts_with("--")).map_or_else(|| ".".into(), Into::into);

    App::new()
        .insert_resource(ClearColor(theme::bevy_clear()))
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "pcg — graphical code editor".into(),
                resolution: (1600, 1000).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        .insert_resource(LoadRequest { path: path.clone(), pending: true, keep_view: false, relayout: false })
        .insert_resource(UiState { path_input: path.display().to_string(), ..default() })
        .init_resource::<Project>()
        .init_resource::<LoadTask>()
        .init_resource::<View>()
        .init_resource::<Selection>()
        .init_resource::<CanvasScratch>()
        .init_resource::<Transition>()
        .init_resource::<Cache>()
        .init_resource::<Watch>()
        .init_resource::<Editing>()
        .init_resource::<ViewState>()
        .insert_resource(Lsp { enabled: !no_lsp, ..default() })
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (watch::poll, edit::poll, lsp::drive, viewstate::poll, load::start, load::poll, anim::end, view::animate)
                .chain(),
        )
        .add_systems(EguiPrimaryContextPass, ui::ui)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
}
