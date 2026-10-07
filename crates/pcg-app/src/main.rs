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
//! | `load::start`     | Update                 | `LoadRequest`, `Cache` → `LoadTask`     |
//! | `load::poll`      | Update                 | `LoadTask` → `Project`, `Transition`, `View`, `Selection`, `Watch` |
//! | `anim::end`       | Update                 | `Time` → `Transition` (drops old snapshot) |
//! | `view::animate`   | Update                 | `Time` → `View`                         |
//! | `ui::ui`          | EguiPrimaryContextPass | everything → `View`, `Selection`, `UiState`, `LoadRequest` |

mod anim;
mod canvas;
mod load;
mod model;
mod theme;
mod ui;
mod view;
mod watch;

use bevy::prelude::*;
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
use model::*;

fn main() {
    let path = std::env::args().nth(1).map(std::path::PathBuf::from).unwrap_or_else(|| ".".into());

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
        .insert_resource(LoadRequest { path: path.clone(), pending: true, keep_view: false })
        .insert_resource(UiState { path_input: path.display().to_string(), ..default() })
        .init_resource::<Project>()
        .init_resource::<LoadTask>()
        .init_resource::<View>()
        .init_resource::<Selection>()
        .init_resource::<CanvasScratch>()
        .init_resource::<Transition>()
        .init_resource::<Cache>()
        .init_resource::<Watch>()
        .add_systems(Startup, setup)
        .add_systems(Update, (watch::poll, load::start, load::poll, anim::end, view::animate).chain())
        .add_systems(EguiPrimaryContextPass, ui::ui)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
}
