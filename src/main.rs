mod app;
mod audio;
mod bridge;
mod config;
mod http_client;
mod model;
mod resolver;
mod updater;

use std::borrow::Cow;

use gpui::{
    App, AppContext as _, Application, AssetSource, Bounds, KeyBinding, Menu, MenuItem,
    SharedString, TitlebarOptions, WindowBounds, WindowOptions, actions, px, size,
};
use gpui_component::Root;

use app::PocketYtmApp;
use audio::AudioEngine;
use bridge::YtMusicBridge;
use config::AppConfig;
use http_client::NativeHttpClient;

const LOADER_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M21 12a9 9 0 1 1-6.22-8.56"/></svg>"#;

struct PocketAssets;

impl AssetSource for PocketAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok((path == "icons/loader.svg").then_some(Cow::Borrowed(LOADER_SVG)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(if path == "icons" {
            vec!["icons/loader.svg".into()]
        } else {
            Vec::new()
        })
    }
}

actions!(
    pocket_ytm,
    [
        TogglePlayback,
        NextTrack,
        PreviousTrack,
        FocusSearch,
        CheckForUpdates,
        Quit
    ]
);

fn main() {
    if let Some(exit_code) = audio::maybe_run_media_stream() {
        std::process::exit(exit_code);
    }
    env_logger::init();
    let config = AppConfig::from_env();
    let bridge = YtMusicBridge::new(config.clone());
    let audio = AudioEngine::new(config);

    Application::new()
        .with_assets(PocketAssets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            match NativeHttpClient::new() {
                Ok(client) => cx.set_http_client(client),
                Err(error) => log::warn!("remote artwork client unavailable: {error:#}"),
            }

            let mut bindings = vec![KeyBinding::new("space", TogglePlayback, Some("PocketYtm"))];
            if cfg!(target_os = "macos") {
                bindings.extend([
                    KeyBinding::new("cmd-right", NextTrack, Some("PocketYtm")),
                    KeyBinding::new("cmd-left", PreviousTrack, Some("PocketYtm")),
                    KeyBinding::new("cmd-k", FocusSearch, Some("PocketYtm")),
                    KeyBinding::new("cmd-q", Quit, None),
                ]);
            } else {
                bindings.extend([
                    KeyBinding::new("ctrl-right", NextTrack, Some("PocketYtm")),
                    KeyBinding::new("ctrl-left", PreviousTrack, Some("PocketYtm")),
                    KeyBinding::new("ctrl-k", FocusSearch, Some("PocketYtm")),
                    KeyBinding::new("ctrl-q", Quit, None),
                ]);
            }
            cx.bind_keys(bindings);
            cx.set_menus(vec![Menu {
                name: "Pocket Music".into(),
                items: vec![
                    MenuItem::action("검색", FocusSearch),
                    MenuItem::action("재생/일시정지", TogglePlayback),
                    MenuItem::action("업데이트 확인", CheckForUpdates),
                    MenuItem::separator(),
                    MenuItem::action("종료", Quit),
                ],
            }]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let window_options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Pocket Music".into()),
                    appears_transparent: false,
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1320.), px(820.)),
                    cx,
                ))),
                window_min_size: Some(size(px(980.), px(660.))),
                ..Default::default()
            };

            let bridge = bridge.clone();
            let audio = audio.clone();
            cx.open_window(window_options, move |window, cx| {
                let view = cx.new(|cx| PocketYtmApp::new(bridge, audio, window, cx));
                view.update(cx, |view, cx| view.load_initial(cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open Pocket Music window");
            cx.activate(true);
        });
}
