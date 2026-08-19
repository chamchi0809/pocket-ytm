mod app;
mod audio;
mod bridge;
mod config;
mod http_client;
mod image_cache;
mod model;
mod updater;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions,
    WindowBounds, WindowOptions, actions, px, size,
};
use gpui_component::Root;

use app::PocketYtmApp;
use audio::AudioEngine;
use bridge::YtMusicBridge;
use config::AppConfig;
use http_client::NativeHttpClient;

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

    Application::new().run(move |cx: &mut App| {
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
                MenuItem::action("업데이트 확인…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("종료", Quit),
            ],
        }]);
        cx.on_action(|_: &Quit, cx| cx.quit());

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
