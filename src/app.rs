use std::{collections::HashSet, sync::Arc, time::Duration};

use gpui::{
    AnyElement, Context, Entity, Focusable as _, IntoElement, ObjectFit, Render, SharedString,
    StyledImage as _, Task, Timer, Window, div, img, prelude::*, px, relative, rgb,
};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::{
    CheckForUpdates, FocusSearch, NextTrack, PreviousTrack, TogglePlayback,
    audio::{AudioEngine, AudioSnapshot, PlaybackPhase},
    bridge::YtMusicBridge,
    model::{AccountStatus, BrowsePage, Lyrics, MediaItem, MediaSection},
    updater::{AvailableUpdate, UpdateCheck, UpdateClient},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Explore,
    Library,
    Search,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Debug, Clone)]
enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(AvailableUpdate),
    Installing(String),
    Error(String),
}

pub struct PocketYtmApp {
    backend: Arc<YtMusicBridge>,
    audio: AudioEngine,
    search_input: Entity<InputState>,
    auth_input: Entity<InputState>,
    auth_status: AccountStatus,
    show_auth: bool,
    auth_busy: bool,
    auth_message: Option<String>,
    page: Page,
    sections: Vec<MediaSection>,
    search_results: Vec<MediaItem>,
    search_query: String,
    detail: Option<BrowsePage>,
    loading: bool,
    error: Option<String>,
    queue: Vec<MediaItem>,
    queue_index: usize,
    show_queue: bool,
    show_lyrics: bool,
    lyrics_browse_id: Option<String>,
    lyrics: Option<Lyrics>,
    shuffle: bool,
    repeat: RepeatMode,
    liked: HashSet<String>,
    audio_snapshot: AudioSnapshot,
    last_ended_generation: Option<u64>,
    updater: Option<UpdateClient>,
    update_state: UpdateState,
    show_update: bool,
    _poll_task: Task<()>,
}

impl PocketYtmApp {
    pub fn new(
        backend: Arc<YtMusicBridge>,
        audio: AudioEngine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("노래, 앨범, 아티스트 검색"));
        let auth_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste Copy as fetch (Node.js) here")
                .multi_line(true)
        });
        cx.subscribe(&search_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let query = input.read(cx).value().to_string();
                if !query.trim().is_empty() {
                    this.search(query, cx);
                }
            }
        })
        .detach();

        let poll_audio = audio.clone();
        let poll_task = cx.spawn(async move |weak, cx| {
            loop {
                Timer::after(Duration::from_millis(200)).await;
                let snapshot = poll_audio.snapshot();
                let Some(entity) = weak.upgrade() else {
                    break;
                };
                if entity
                    .update(cx, |this, cx| {
                        this.update_audio(snapshot, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            backend,
            audio_snapshot: audio.snapshot(),
            audio,
            search_input,
            auth_input,
            auth_status: AccountStatus::default(),
            show_auth: std::env::var_os("POCKET_YTM_SHOW_LOGIN").is_some(),
            auth_busy: false,
            auth_message: None,
            page: Page::Home,
            sections: vec![],
            search_results: vec![],
            search_query: String::new(),
            detail: None,
            loading: false,
            error: None,
            queue: vec![],
            queue_index: 0,
            show_queue: false,
            show_lyrics: false,
            lyrics_browse_id: None,
            lyrics: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            liked: HashSet::new(),
            last_ended_generation: None,
            updater: UpdateClient::new()
                .map_err(|error| {
                    log::warn!("update client unavailable: {error:#}");
                    error
                })
                .ok(),
            update_state: UpdateState::Idle,
            show_update: false,
            _poll_task: poll_task,
        }
    }

    pub fn load_initial(&mut self, cx: &mut Context<Self>) {
        self.load_auth_status(cx);
        self.load_home(cx);
        self.check_for_updates(false, cx);
    }

    fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if matches!(
            self.update_state,
            UpdateState::Checking | UpdateState::Installing(_)
        ) {
            if manual {
                self.show_update = true;
                cx.notify();
            }
            return;
        }

        let Some(updater) = self.updater.clone() else {
            if manual {
                self.update_state =
                    UpdateState::Error("업데이트 클라이언트를 초기화하지 못했습니다.".into());
                self.show_update = true;
                cx.notify();
            }
            return;
        };

        self.update_state = UpdateState::Checking;
        self.show_update = manual;
        let task = cx.background_spawn(async move { updater.check() });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        match result {
                            Ok(UpdateCheck::UpToDate) => {
                                this.update_state = UpdateState::UpToDate;
                            }
                            Ok(UpdateCheck::Available(update)) => {
                                this.update_state = UpdateState::Available(update);
                                this.show_update = true;
                            }
                            Err(error) if manual => {
                                this.update_state = UpdateState::Error(format!("{error:#}"));
                                this.show_update = true;
                            }
                            Err(error) => {
                                log::warn!("automatic update check failed: {error:#}");
                                this.update_state = UpdateState::Idle;
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(update) = self.update_state.clone() else {
            return;
        };
        let Some(updater) = self.updater.clone() else {
            self.update_state =
                UpdateState::Error("업데이트 클라이언트를 초기화하지 못했습니다.".into());
            cx.notify();
            return;
        };

        let version = update.version.clone();
        self.update_state = UpdateState::Installing(version);
        let task =
            cx.background_spawn(async move { updater.download_and_prepare_install(&update) });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| match result {
                        Ok(()) => cx.quit(),
                        Err(error) => {
                            this.update_state = UpdateState::Error(format!("{error:#}"));
                            this.show_update = true;
                            cx.notify();
                        }
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn open_update_release(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(update) = &self.update_state else {
            return;
        };
        let mut command = if cfg!(target_os = "macos") {
            let mut command = std::process::Command::new("open");
            command.arg(&update.release_url);
            command
        } else if cfg!(target_os = "windows") {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "start", "", &update.release_url]);
            command
        } else {
            let mut command = std::process::Command::new("xdg-open");
            command.arg(&update.release_url);
            command
        };
        if let Err(error) = command.spawn() {
            self.update_state =
                UpdateState::Error(format!("릴리스 페이지를 열지 못했습니다: {error}"));
            cx.notify();
        }
    }

    fn load_auth_status(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.auth_status() });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        if let Ok(status) = result {
                            this.auth_status = status;
                            cx.notify();
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn authenticate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auth_busy {
            return;
        }
        let headers = self.auth_input.read(cx).value().to_string();
        if headers.trim().is_empty() {
            self.auth_message = Some("복사한 요청 헤더를 붙여 넣어 주세요.".into());
            cx.notify();
            return;
        }

        // Request headers contain session credentials. Remove them from UI memory as
        // soon as the background authentication task owns its copy.
        self.auth_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.auth_busy = true;
        self.auth_message = Some("로그인 정보를 확인하고 있습니다…".into());
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.authenticate(&headers) });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.auth_busy = false;
                        match result {
                            Ok(status) => {
                                this.auth_status = status;
                                this.auth_message = Some("로그인되었습니다.".into());
                                this.load_home(cx);
                            }
                            Err(error) => {
                                this.auth_message = Some(format!("로그인 실패: {error:#}"));
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn logout(&mut self, cx: &mut Context<Self>) {
        if self.auth_busy {
            return;
        }
        self.auth_busy = true;
        self.auth_message = Some("로그아웃하는 중…".into());
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.logout() });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.auth_busy = false;
                        match result {
                            Ok(status) => {
                                this.auth_status = status;
                                this.auth_message = Some("로그아웃되었습니다.".into());
                                this.load_home(cx);
                            }
                            Err(error) => {
                                this.auth_message = Some(format!("로그아웃 실패: {error:#}"));
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn open_music_login(&mut self, cx: &mut Context<Self>) {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = std::process::Command::new("open");
            command.arg("https://music.youtube.com");
            command
        } else if cfg!(target_os = "windows") {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "start", "", "https://music.youtube.com"]);
            command
        } else {
            let mut command = std::process::Command::new("xdg-open");
            command.arg("https://music.youtube.com");
            command
        };
        if let Err(error) = command.spawn() {
            self.auth_message = Some(format!("시스템 브라우저를 열지 못했습니다: {error}"));
            cx.notify();
        }
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input.read(cx).focus_handle(cx).focus(window);
    }

    fn on_toggle_playback(&mut self, _: &TogglePlayback, _: &mut Window, _: &mut Context<Self>) {
        self.audio.toggle();
    }

    fn on_next_track(&mut self, _: &NextTrack, _: &mut Window, _: &mut Context<Self>) {
        self.next();
    }

    fn on_previous_track(&mut self, _: &PreviousTrack, _: &mut Window, _: &mut Context<Self>) {
        self.previous();
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_search(window, cx);
    }

    fn on_check_for_updates(
        &mut self,
        _: &CheckForUpdates,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_for_updates(true, cx);
    }

    fn next(&mut self) {
        self.play_next(false);
    }

    fn previous(&mut self) {
        if self.audio_snapshot.position > Duration::from_secs(4) {
            self.audio.seek(Duration::ZERO);
        } else if !self.queue.is_empty() {
            self.queue_index = self.queue_index.saturating_sub(1);
            self.audio.load(self.queue[self.queue_index].clone());
        }
    }

    fn update_audio(&mut self, snapshot: AudioSnapshot, cx: &mut Context<Self>) {
        let ended = snapshot.phase == PlaybackPhase::Ended
            && self.last_ended_generation != Some(snapshot.generation);
        if ended {
            self.last_ended_generation = Some(snapshot.generation);
        }
        let changed = snapshot.phase != self.audio_snapshot.phase
            || snapshot.position.as_secs() != self.audio_snapshot.position.as_secs()
            || snapshot.generation != self.audio_snapshot.generation
            || snapshot.error != self.audio_snapshot.error;
        self.audio_snapshot = snapshot;
        if ended {
            self.play_next(true);
        }
        if changed {
            cx.notify();
        }
    }

    fn load_home(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Home;
        self.loading = true;
        self.error = None;
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.home() });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.loading = false;
                        match result {
                            Ok(sections) => this.sections = sections,
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn load_explore(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Explore;
        self.loading = true;
        self.error = None;
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.explore() });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.loading = false;
                        match result {
                            Ok(sections) => this.sections = sections,
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn load_library(&mut self, cx: &mut Context<Self>) {
        if !self.auth_status.authenticated {
            self.show_auth = true;
            self.auth_message = Some("보관함을 사용하려면 먼저 로그인하세요.".into());
            cx.notify();
            return;
        }
        self.page = Page::Library;
        self.loading = true;
        self.error = None;
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.library("all") });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.loading = false;
                        match result {
                            Ok(sections) => this.sections = sections,
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn search(&mut self, query: String, cx: &mut Context<Self>) {
        self.page = Page::Search;
        self.search_query = query.trim().to_owned();
        self.loading = true;
        self.error = None;
        let backend = self.backend.clone();
        let request = self.search_query.clone();
        let task = cx.background_spawn(async move { backend.search(&request) });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.loading = false;
                        match result {
                            Ok(items) => this.search_results = items,
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn browse(&mut self, item: MediaItem, cx: &mut Context<Self>) {
        self.page = Page::Detail;
        self.detail = None;
        self.loading = true;
        self.error = None;
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.browse(&item) });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        this.loading = false;
                        match result {
                            Ok(detail) => this.detail = Some(detail),
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn play_item(&mut self, item: MediaItem, source_queue: Vec<MediaItem>, cx: &mut Context<Self>) {
        if !item.playable() {
            self.browse(item, cx);
            return;
        }
        let mut queue: Vec<_> = source_queue
            .into_iter()
            .filter(MediaItem::playable)
            .collect();
        if queue.is_empty() {
            queue.push(item.clone());
        }
        self.queue_index = queue
            .iter()
            .position(|candidate| candidate.id == item.id)
            .unwrap_or(0);
        self.queue = queue;
        self.audio.load(item.clone());
        self.lyrics = None;
        self.lyrics_browse_id = None;
        self.last_ended_generation = None;
        self.hydrate_watch_queue(item, cx);
        cx.notify();
    }

    fn hydrate_watch_queue(&mut self, current: MediaItem, cx: &mut Context<Self>) {
        let Some(video_id) = current.video_id.clone() else {
            return;
        };
        let current_id = current.id.clone();
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.watch_queue(&video_id) });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        if let Ok(watch) = result {
                            this.lyrics_browse_id = watch.lyrics_browse_id;
                            if !watch.items.is_empty() {
                                this.queue_index = watch
                                    .items
                                    .iter()
                                    .position(|item| item.id == current_id)
                                    .unwrap_or(0);
                                this.queue = watch.items;
                            }
                            cx.notify();
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn play_next(&mut self, automatic: bool) {
        if self.queue.is_empty() {
            return;
        }
        if automatic && self.repeat == RepeatMode::One {
            self.audio.load(self.queue[self.queue_index].clone());
            return;
        }
        if self.shuffle && self.queue.len() > 1 {
            self.queue_index = (self.queue_index * 7 + 3) % self.queue.len();
        } else if self.queue_index + 1 < self.queue.len() {
            self.queue_index += 1;
        } else if self.repeat == RepeatMode::All {
            self.queue_index = 0;
        } else {
            return;
        }
        self.audio.load(self.queue[self.queue_index].clone());
        self.lyrics = None;
        self.lyrics_browse_id = None;
    }

    fn toggle_lyrics(&mut self, cx: &mut Context<Self>) {
        self.show_lyrics = !self.show_lyrics;
        self.show_queue = false;
        if !self.show_lyrics || self.lyrics.is_some() {
            cx.notify();
            return;
        }
        let Some(browse_id) = self.lyrics_browse_id.clone() else {
            self.error = Some("이 트랙에는 제공되는 가사가 없습니다.".into());
            cx.notify();
            return;
        };
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.lyrics(&browse_id) });
        cx.spawn(async move |weak, cx| {
            let result = task.await;
            if let Some(entity) = weak.upgrade() {
                entity
                    .update(cx, |this, cx| {
                        match result {
                            Ok(lyrics) => this.lyrics = Some(lyrics),
                            Err(error) => this.error = Some(format!("{error:#}")),
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn toggle_like(&mut self, cx: &mut Context<Self>) {
        if !self.auth_status.authenticated {
            self.show_auth = true;
            self.auth_message = Some("좋아요를 저장하려면 먼저 로그인하세요.".into());
            cx.notify();
            return;
        }
        let Some(video_id) = self
            .audio_snapshot
            .item
            .as_ref()
            .and_then(|item| item.video_id.clone())
        else {
            return;
        };
        let rating = if self.liked.remove(&video_id) {
            "INDIFFERENT"
        } else {
            self.liked.insert(video_id.clone());
            "LIKE"
        };
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move { backend.rate_song(&video_id, rating) });
        cx.spawn(async move |weak, cx| {
            if let Err(error) = task.await
                && let Some(entity) = weak.upgrade()
            {
                entity
                    .update(cx, |this, cx| {
                        this.error = Some(format!("좋아요를 저장하지 못했습니다: {error:#}"));
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let nav = |label: &'static str, page: Page, glyph: &'static str, cx: &mut Context<Self>| {
            let active = self.page == page;
            div()
                .id(SharedString::from(format!("nav-{label}")))
                .flex()
                .items_center()
                .gap_3()
                .h(px(42.))
                .px_3()
                .rounded_lg()
                .text_size(px(14.))
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active { rgb(0xffffff) } else { rgb(0xa7a7ad) })
                .bg(if active { rgb(0x29292d) } else { rgb(0x171719) })
                .hover(|style| style.bg(rgb(0x252529)).text_color(rgb(0xffffff)))
                .cursor_pointer()
                .child(div().w(px(20.)).text_center().child(glyph))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| match page {
                    Page::Home => this.load_home(cx),
                    Page::Explore => this.load_explore(cx),
                    Page::Library => this.load_library(cx),
                    _ => {}
                }))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(218.))
            .h_full()
            .bg(rgb(0x171719))
            .border_r_1()
            .border_color(rgb(0x2a2a2e))
            .px_4()
            .pt_5()
            .pb(px(108.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .mb_7()
                    .px_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(31.))
                            .rounded_full()
                            .bg(rgb(0xff1744))
                            .text_color(rgb(0xffffff))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("▶"),
                    )
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(0xffffff))
                            .child("Pocket Music"),
                    ),
            )
            .child(nav("홈", Page::Home, "⌂", cx))
            .child(nav("둘러보기", Page::Explore, "◉", cx))
            .child(nav("보관함", Page::Library, "▤", cx))
            .child(div().flex_1())
            .child(
                div()
                    .px_2()
                    .text_size(px(11.))
                    .line_height(px(17.))
                    .text_color(rgb(0x66666c))
                    .child("GPUI · native audio")
                    .child("Zero DOM · Zero Chromium"),
            )
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let account_label = if self.auth_status.authenticated {
            if self.auth_status.name.is_empty() {
                "내 계정".to_owned()
            } else {
                self.auth_status.name.clone()
            }
        } else {
            "로그인".to_owned()
        };
        div()
            .flex()
            .items_center()
            .h(px(68.))
            .flex_shrink_0()
            .px_7()
            .gap_4()
            .border_b_1()
            .border_color(rgb(0x28282c))
            .bg(rgb(0x101012))
            .child(
                div()
                    .text_color(rgb(0x77777e))
                    .text_size(px(18.))
                    .child("‹   ›"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .w(px(480.))
                    .h(px(40.))
                    .px_3()
                    .rounded_full()
                    .bg(rgb(0x252529))
                    .border_1()
                    .border_color(rgb(0x34343a))
                    .child(
                        Input::new(&self.search_input)
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .w_full(),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(34.))
                    .rounded_full()
                    .bg(rgb(0x252529))
                    .text_size(px(12.))
                    .text_color(rgb(0xb8b8bd))
                    .child("●")
                    .child("NATIVE"),
            )
            .child(
                div()
                    .id("account")
                    .flex()
                    .items_center()
                    .gap_2()
                    .max_w(px(180.))
                    .px_4()
                    .h(px(34.))
                    .rounded_full()
                    .border_1()
                    .border_color(if self.auth_status.authenticated {
                        rgb(0x525258)
                    } else {
                        rgb(0xff3157)
                    })
                    .bg(rgb(0x252529))
                    .hover(|style| style.bg(rgb(0x323237)))
                    .text_size(px(12.))
                    .text_color(rgb(0xf1f1f2))
                    .cursor_pointer()
                    .child(
                        div()
                            .text_color(if self.auth_status.authenticated {
                                rgb(0x56d887)
                            } else {
                                rgb(0xff6c83)
                            })
                            .child("●"),
                    )
                    .child(div().truncate().child(account_label))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_auth = true;
                        this.auth_message = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn artwork(item: &MediaItem, size: f32, rounded: f32) -> AnyElement {
        if let Some(url) = &item.thumbnail {
            let image = img(url.clone())
                .size(px(size))
                .object_fit(ObjectFit::Cover)
                .with_loading(move || {
                    div()
                        .size(px(size))
                        .rounded(px(rounded))
                        .bg(rgb(0x2b2b30))
                        .into_any_element()
                })
                .with_fallback(move || {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(size))
                        .rounded(px(rounded))
                        .bg(rgb(0x2b2b30))
                        .text_color(rgb(0x77777e))
                        .child("♪")
                        .into_any_element()
                });
            div()
                .flex_shrink_0()
                .size(px(size))
                .rounded(px(rounded))
                .overflow_hidden()
                .child(image)
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(size))
                .rounded(px(rounded))
                .bg(rgb(0x2b2b30))
                .text_size(px(size * 0.28))
                .text_color(rgb(0x77777e))
                .child("♪")
                .into_any_element()
        }
    }

    fn render_card(
        item: MediaItem,
        source_queue: Vec<MediaItem>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let click_item = item.clone();
        div()
            .id(SharedString::from(format!(
                "card-{}-{}",
                item.kind, item.id
            )))
            .flex()
            .flex_col()
            .w(px(168.))
            .p_2()
            .rounded_xl()
            .hover(|style| style.bg(rgb(0x222226)))
            .cursor_pointer()
            .child(Self::artwork(
                &item,
                152.,
                if item.kind == "artist" { 76. } else { 8. },
            ))
            .child(
                div()
                    .mt_3()
                    .w_full()
                    .truncate()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0xf2f2f3))
                    .child(item.title.clone()),
            )
            .child(
                div()
                    .mt_1()
                    .w_full()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(0x898990))
                    .child(if item.subtitle.is_empty() {
                        item.kind.clone()
                    } else {
                        item.subtitle.clone()
                    }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.play_item(click_item.clone(), source_queue.clone(), cx)
            }))
            .into_any_element()
    }

    fn render_row(
        item: MediaItem,
        source_queue: Vec<MediaItem>,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let click_item = item.clone();
        let duration = item.duration_seconds.map(format_time).unwrap_or_default();
        div()
            .id(SharedString::from(format!("row-{index}-{}", item.id)))
            .flex()
            .items_center()
            .h(px(66.))
            .px_3()
            .gap_4()
            .rounded_lg()
            .hover(|style| style.bg(rgb(0x222226)))
            .cursor_pointer()
            .child(
                div()
                    .w(px(24.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(rgb(0x717178))
                    .child(if item.playable() {
                        (index + 1).to_string()
                    } else {
                        "•".into()
                    }),
            )
            .child(Self::artwork(
                &item,
                48.,
                if item.kind == "artist" { 24. } else { 5. },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(0xf0f0f2))
                            .child(item.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(0x85858c))
                            .child(item.subtitle.clone()),
                    ),
            )
            .child(
                div()
                    .w(px(90.))
                    .text_size(px(11.))
                    .text_color(rgb(0x6f6f76))
                    .child(item.kind.to_uppercase()),
            )
            .child(
                div()
                    .w(px(44.))
                    .text_right()
                    .text_size(px(12.))
                    .text_color(rgb(0x8a8a91))
                    .child(duration),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.play_item(click_item.clone(), source_queue.clone(), cx)
            }))
            .into_any_element()
    }

    fn render_sections(&self, sections: Vec<MediaSection>, cx: &mut Context<Self>) -> AnyElement {
        let mut rendered = Vec::new();
        for section in sections {
            let queue = section.items.clone();
            let mut cards = Vec::new();
            for item in section.items {
                cards.push(Self::render_card(item, queue.clone(), cx));
            }
            rendered.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .mb_8()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(21.))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(0xf5f5f6))
                                    .child(section.title),
                            )
                            .child(
                                div()
                                    .pb_1()
                                    .text_size(px(12.))
                                    .text_color(rgb(0x77777e))
                                    .child(section.subtitle),
                            ),
                    )
                    .child(div().flex().flex_wrap().gap_3().children(cards))
                    .into_any_element(),
            );
        }
        div().children(rendered).into_any_element()
    }

    fn render_search_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let queue = self.search_results.clone();
        let mut rows = Vec::new();
        for (index, item) in self.search_results.clone().into_iter().enumerate() {
            rows.push(Self::render_row(item, queue.clone(), index, cx));
        }
        div()
            .child(
                div()
                    .mb_5()
                    .text_size(px(24.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(0xffffff))
                    .child(format!("‘{}’ 검색 결과", self.search_query)),
            )
            .children(rows)
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return div().into_any_element();
        };
        let hero = MediaItem {
            title: detail.title.clone(),
            thumbnail: detail.thumbnail.clone(),
            ..Default::default()
        };
        div()
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_6()
                    .mb_9()
                    .child(Self::artwork(&hero, 190., 10.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .pb_2()
                            .child(
                                div()
                                    .mb_2()
                                    .text_size(px(12.))
                                    .text_color(rgb(0x8d8d94))
                                    .child("YOUTUBE MUSIC"),
                            )
                            .child(
                                div()
                                    .text_size(px(34.))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(0xffffff))
                                    .child(detail.title.clone()),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .text_size(px(14.))
                                    .text_color(rgb(0xa0a0a7))
                                    .child(detail.subtitle.clone()),
                            )
                            .child(
                                div()
                                    .mt_3()
                                    .max_w(px(660.))
                                    .text_size(px(12.))
                                    .line_height(px(18.))
                                    .text_color(rgb(0x77777e))
                                    .child(detail.description.clone()),
                            ),
                    ),
            )
            .child(self.render_sections(detail.sections.clone(), cx))
            .into_any_element()
    }

    fn render_center(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = match self.page {
            Page::Home => "다시 듣기",
            Page::Explore => "둘러보기",
            Page::Library => "보관함",
            Page::Search => "검색",
            Page::Detail => "",
        };
        let body = if self.loading {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(320.))
                .text_color(rgb(0x8a8a91))
                .child("YouTube Music에서 불러오는 중…")
                .into_any_element()
        } else if self.page == Page::Search {
            self.render_search_results(cx)
        } else if self.page == Page::Detail {
            self.render_detail(cx)
        } else {
            div()
                .child(
                    div()
                        .mb_8()
                        .text_size(px(30.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(0xffffff))
                        .child(title),
                )
                .child(self.render_sections(self.sections.clone(), cx))
                .into_any_element()
        };

        div()
            .id("content-scroll")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px_8()
            .pt_8()
            .pb(px(130.))
            .bg(rgb(0x101012))
            .child(
                div()
                    .max_w(px(1180.))
                    .w_full()
                    .mx_auto()
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .mb_6()
                                .p_4()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(0x74303b))
                                .bg(rgb(0x351b20))
                                .text_size(px(12.))
                                .line_height(px(18.))
                                .text_color(rgb(0xffb4be))
                                .child(error),
                        )
                    })
                    .when_some(self.audio_snapshot.error.clone(), |this, error| {
                        this.child(
                            div()
                                .mb_6()
                                .p_4()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(0x74303b))
                                .bg(rgb(0x351b20))
                                .text_size(px(12.))
                                .line_height(px(18.))
                                .text_color(rgb(0xffb4be))
                                .child(error),
                        )
                    })
                    .child(body),
            )
            .into_any_element()
    }

    fn render_side_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.show_queue {
            let mut rows = Vec::new();
            for (index, item) in self.queue.clone().into_iter().enumerate() {
                let active = index == self.queue_index;
                let queued_id = item.id.clone();
                rows.push(
                    div()
                        .id(SharedString::from(format!("queue-{index}-{}", item.id)))
                        .flex()
                        .items_center()
                        .gap_3()
                        .p_2()
                        .rounded_lg()
                        .bg(if active { rgb(0x2a2023) } else { rgb(0x171719) })
                        .hover(|style| style.bg(rgb(0x252529)))
                        .cursor_pointer()
                        .child(Self::artwork(&item, 42., 4.))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(if active {
                                            rgb(0xff6c83)
                                        } else {
                                            rgb(0xe8e8ea)
                                        })
                                        .child(item.title.clone()),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .truncate()
                                        .text_size(px(11.))
                                        .text_color(rgb(0x77777e))
                                        .child(item.subtitle.clone()),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, _| {
                            if let Some(index) = this
                                .queue
                                .iter()
                                .position(|candidate| candidate.id == queued_id)
                            {
                                this.queue_index = index;
                                this.audio.load(this.queue[index].clone());
                            }
                        }))
                        .into_any_element(),
                );
            }
            Some(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .w(px(340.))
                    .h_full()
                    .pb(px(105.))
                    .border_l_1()
                    .border_color(rgb(0x2a2a2e))
                    .bg(rgb(0x171719))
                    .child(
                        div()
                            .px_5()
                            .py_5()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(0xf4f4f5))
                            .child("재생목록"),
                    )
                    .child(
                        div()
                            .id("queue-scroll")
                            .overflow_y_scroll()
                            .px_3()
                            .children(rows),
                    )
                    .into_any_element(),
            )
        } else if self.show_lyrics {
            let lyric_text = self
                .lyrics
                .as_ref()
                .map(|lyrics| lyrics.text.clone())
                .unwrap_or_else(|| "가사를 불러오는 중…".into());
            Some(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .w(px(400.))
                    .h_full()
                    .pb(px(105.))
                    .border_l_1()
                    .border_color(rgb(0x2a2a2e))
                    .bg(rgb(0x171719))
                    .child(
                        div()
                            .px_6()
                            .py_5()
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(0xf4f4f5))
                            .child("가사"),
                    )
                    .child(
                        div()
                            .id("lyrics-scroll")
                            .overflow_y_scroll()
                            .px_6()
                            .pb_8()
                            .text_size(px(17.))
                            .line_height(px(30.))
                            .text_color(rgb(0xd3d3d6))
                            .child(lyric_text),
                    )
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    fn render_auth_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.show_auth {
            return None;
        }

        let message = self.auth_message.clone();
        let content = if self.auth_status.authenticated {
            let avatar = if let Some(url) = self.auth_status.thumbnail.clone() {
                div()
                    .size(px(72.))
                    .rounded_full()
                    .overflow_hidden()
                    .child(img(url).size(px(72.)).object_fit(ObjectFit::Cover))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(72.))
                    .rounded_full()
                    .bg(rgb(0x34343a))
                    .text_size(px(28.))
                    .child("♪")
                    .into_any_element()
            };
            div()
                .flex()
                .flex_col()
                .items_center()
                .py_8()
                .child(avatar)
                .child(
                    div()
                        .mt_5()
                        .text_size(px(22.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(0xf7f7f8))
                        .child(if self.auth_status.name.is_empty() {
                            "YouTube Music 계정".to_owned()
                        } else {
                            self.auth_status.name.clone()
                        }),
                )
                .when(!self.auth_status.handle.is_empty(), |this| {
                    this.child(
                        div()
                            .mt_1()
                            .text_size(px(13.))
                            .text_color(rgb(0x929299))
                            .child(self.auth_status.handle.clone()),
                    )
                })
                .child(
                    div()
                        .id("logout")
                        .mt_7()
                        .px_5()
                        .h(px(38.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(0x5b3038))
                        .text_size(px(12.))
                        .text_color(rgb(0xff9cac))
                        .cursor_pointer()
                        .child(if self.auth_busy {
                            "처리 중…"
                        } else {
                            "로그아웃"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.logout(cx))),
                )
                .into_any_element()
        } else {
            let step = |number: &'static str, text: &'static str| {
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .flex_shrink_0()
                            .size(px(24.))
                            .rounded_full()
                            .bg(rgb(0x302328))
                            .text_size(px(11.))
                            .text_color(rgb(0xff8297))
                            .child(number),
                    )
                    .child(
                        div()
                            .pt_1()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(0xc4c4c9))
                            .child(text),
                    )
            };
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(step(
                    "1",
                    "시스템 브라우저에서 YouTube Music에 로그인한 뒤 개발자 도구의 Network 탭을 여세요.",
                ))
                .child(step(
                    "2",
                    "페이지를 새로고침하고 /browse POST 요청을 우클릭해 Copy → Copy as fetch (Node.js)를 누르세요.",
                ))
                .child(step(
                    "3",
                    "복사된 fetch 코드 전체를 아래에 붙여 넣으세요. JavaScript는 실행하지 않고 headers만 읽습니다.",
                ))
                .child(
                    div()
                        .id("open-music-login")
                        .mt_2()
                        .h(px(36.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_lg()
                        .bg(rgb(0x2a2a2f))
                        .hover(|style| style.bg(rgb(0x36363c)))
                        .text_size(px(12.))
                        .text_color(rgb(0xe8e8eb))
                        .cursor_pointer()
                        .child("시스템 브라우저에서 music.youtube.com 열기 ↗")
                        .on_click(cx.listener(|this, _, _, cx| this.open_music_login(cx))),
                )
                .child(
                    div()
                        .mt_2()
                        .h(px(175.))
                        .p_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0x3c3c42))
                        .bg(rgb(0x151517))
                        .child(
                            Input::new(&self.auth_input)
                                .appearance(false)
                                .bordered(false)
                                .focus_bordered(false)
                                .h_full(),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .text_color(rgb(0x77777e))
                        .child("비밀번호를 입력하는 화면이 아닙니다. fetch 코드는 검증 직후 입력창에서 지워집니다."),
                )
                .child(
                    div()
                        .id("submit-auth")
                        .mt_2()
                        .h(px(42.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(if self.auth_busy {
                            rgb(0x64303d)
                        } else {
                            rgb(0xff3157)
                        })
                        .hover(|style| style.bg(rgb(0xff4d6d)))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(px(13.))
                        .text_color(rgb(0xffffff))
                        .cursor_pointer()
                        .child(if self.auth_busy {
                            "확인하는 중…"
                        } else {
                            "붙여 넣은 fetch 코드로 로그인"
                        })
                        .on_click(
                            cx.listener(|this, _, window, cx| this.authenticate(window, cx)),
                        ),
                )
                .into_any_element()
        };

        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0x08080a))
                .child(
                    div()
                        .id("auth-card")
                        .w(px(650.))
                        .max_h(px(740.))
                        .overflow_y_scroll()
                        .p_7()
                        .rounded_xl()
                        .border_1()
                        .border_color(rgb(0x39393f))
                        .bg(rgb(0x1b1b1e))
                        .shadow_xl()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .mb_5()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(
                                            div()
                                                .text_size(px(22.))
                                                .font_weight(gpui::FontWeight::BOLD)
                                                .text_color(rgb(0xf7f7f8))
                                                .child(if self.auth_status.authenticated {
                                                    "계정"
                                                } else {
                                                    "YouTube Music 로그인"
                                                }),
                                        )
                                        .child(
                                            div()
                                                .mt_1()
                                                .text_size(px(12.))
                                                .text_color(rgb(0x85858c))
                                                .child("내장 WebView 없이 시스템 브라우저 세션을 연결합니다."),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("close-auth")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(34.))
                                        .rounded_full()
                                        .bg(rgb(0x2a2a2f))
                                        .hover(|style| style.bg(rgb(0x39393f)))
                                        .text_color(rgb(0xc8c8cc))
                                        .cursor_pointer()
                                        .child("×")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_auth = false;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .when_some(message, |this, message| {
                            this.child(
                                div()
                                    .mb_5()
                                    .p_3()
                                    .rounded_lg()
                                    .bg(rgb(0x292125))
                                    .text_size(px(12.))
                                    .line_height(px(18.))
                                    .text_color(rgb(0xffa8b7))
                                    .child(message),
                            )
                        })
                        .child(content),
                )
                .into_any_element(),
        )
    }

    fn render_update_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.show_update {
            return None;
        }

        let installing = matches!(self.update_state, UpdateState::Installing(_));
        let content = match &self.update_state {
            UpdateState::Idle | UpdateState::Checking => div()
                .py_10()
                .text_center()
                .text_size(px(14.))
                .text_color(rgb(0xc8c8cd))
                .child("GitHub Releases에서 최신 버전을 확인하는 중…")
                .into_any_element(),
            UpdateState::UpToDate => div()
                .py_9()
                .text_center()
                .child(
                    div()
                        .text_size(px(19.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(0xf5f5f6))
                        .child("최신 버전입니다"),
                )
                .child(
                    div()
                        .mt_2()
                        .text_size(px(12.))
                        .text_color(rgb(0x8f8f96))
                        .child(format!("Pocket Music v{}", env!("CARGO_PKG_VERSION"))),
                )
                .into_any_element(),
            UpdateState::Available(update) => {
                let notes = if update.notes.trim().is_empty() {
                    "이 릴리스에는 별도의 변경 기록이 없습니다.".to_owned()
                } else {
                    update.notes.chars().take(4000).collect()
                };
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .p_4()
                            .rounded_lg()
                            .bg(rgb(0x242429))
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(0xffffff))
                                    .child(format!("Pocket Music v{}", update.version)),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_size(px(12.))
                                    .text_color(rgb(0x96969d))
                                    .child(format!(
                                        "현재 v{} · 서명된 GitHub 릴리스",
                                        env!("CARGO_PKG_VERSION")
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .id("update-notes")
                            .mt_4()
                            .max_h(px(270.))
                            .overflow_y_scroll()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(0x34343a))
                            .bg(rgb(0x151517))
                            .text_size(px(12.))
                            .line_height(px(19.))
                            .text_color(rgb(0xc8c8cd))
                            .child(notes),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .mt_5()
                            .child(
                                div()
                                    .id("open-update-release")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .h(px(40.))
                                    .px_5()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(0x46464c))
                                    .hover(|style| style.bg(rgb(0x2c2c31)))
                                    .text_size(px(12.))
                                    .text_color(rgb(0xd6d6da))
                                    .cursor_pointer()
                                    .child("릴리스 페이지 ↗")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_update_release(cx)),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("install-update")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .h(px(40.))
                                    .px_6()
                                    .rounded_full()
                                    .bg(rgb(0xff3157))
                                    .hover(|style| style.bg(rgb(0xff4d6d)))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_size(px(12.))
                                    .text_color(rgb(0xffffff))
                                    .cursor_pointer()
                                    .child("업데이트하고 재시작")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.install_update(cx)),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            UpdateState::Installing(version) => div()
                .py_10()
                .text_center()
                .child(
                    div()
                        .text_size(px(18.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(0xf5f5f6))
                        .child(format!("v{version} 업데이트 설치 준비 중…")),
                )
                .child(
                    div()
                        .mt_2()
                        .text_size(px(12.))
                        .text_color(rgb(0x8f8f96))
                        .child("다운로드와 서명 검증이 끝나면 앱이 자동으로 재시작됩니다."),
                )
                .into_any_element(),
            UpdateState::Error(message) => div()
                .py_5()
                .child(
                    div()
                        .p_4()
                        .rounded_lg()
                        .bg(rgb(0x332127))
                        .text_size(px(12.))
                        .line_height(px(19.))
                        .text_color(rgb(0xffa9b8))
                        .child(format!("업데이트를 완료하지 못했습니다.\n\n{message}")),
                )
                .child(
                    div()
                        .id("retry-update")
                        .mt_4()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(40.))
                        .rounded_full()
                        .bg(rgb(0x2c2c31))
                        .hover(|style| style.bg(rgb(0x38383e)))
                        .text_size(px(12.))
                        .cursor_pointer()
                        .child("다시 확인")
                        .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(true, cx))),
                )
                .into_any_element(),
        };

        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0x08080a))
                .child(
                    div()
                        .w(px(610.))
                        .max_h(px(680.))
                        .p_7()
                        .rounded_xl()
                        .border_1()
                        .border_color(rgb(0x39393f))
                        .bg(rgb(0x1b1b1e))
                        .shadow_xl()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .mb_5()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(
                                            div()
                                                .text_size(px(22.))
                                                .font_weight(gpui::FontWeight::BOLD)
                                                .text_color(rgb(0xf7f7f8))
                                                .child("소프트웨어 업데이트"),
                                        )
                                        .child(
                                            div()
                                                .mt_1()
                                                .text_size(px(12.))
                                                .text_color(rgb(0x85858c))
                                                .child("Ed25519 서명을 검증한 공식 GitHub 릴리스만 설치합니다."),
                                        ),
                                )
                                .when(!installing, |this| {
                                    this.child(
                                        div()
                                            .id("close-update")
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .size(px(34.))
                                            .rounded_full()
                                            .bg(rgb(0x2a2a2f))
                                            .hover(|style| style.bg(rgb(0x39393f)))
                                            .text_color(rgb(0xc8c8cc))
                                            .cursor_pointer()
                                            .child("×")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show_update = false;
                                                cx.notify();
                                            })),
                                    )
                                }),
                        )
                        .child(content),
                )
                .into_any_element(),
        )
    }

    fn render_player(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = &self.audio_snapshot;
        let item = state.item.clone().unwrap_or_else(|| MediaItem {
            title: "재생할 음악을 선택하세요".into(),
            subtitle: "Pocket Music".into(),
            ..Default::default()
        });
        let playing = state.phase == PlaybackPhase::Playing;
        let liked = item
            .video_id
            .as_ref()
            .is_some_and(|id| self.liked.contains(id));
        let ratio = if state.duration.is_zero() {
            0.0
        } else {
            (state.position.as_secs_f64() / state.duration.as_secs_f64()).clamp(0.0, 1.0)
        };
        let mut seek_targets = Vec::new();
        for index in 0..64 {
            let fraction = (index + 1) as f64 / 64.0;
            seek_targets.push(
                div()
                    .id(SharedString::from(format!("seek-{index}")))
                    .h(px(4.))
                    .flex_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, _| {
                        let duration = this.audio_snapshot.duration;
                        this.audio.seek(duration.mul_f64(fraction));
                    }))
                    .into_any_element(),
            );
        }

        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(px(104.))
            .bg(rgb(0x1b1b1e))
            .border_t_1()
            .border_color(rgb(0x34343a))
            .shadow_lg()
            .child(
                div()
                    .id("player-progress")
                    .relative()
                    .h(px(4.))
                    .bg(rgb(0x424248))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(relative(ratio as f32))
                            .bg(rgb(0xff3157)),
                    )
                    .child(div().absolute().size_full().flex().children(seek_targets)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(99.))
                    .px_5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .w(px(350.))
                            .child(Self::artwork(&item, 58., 5.))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(13.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(rgb(0xf4f4f5))
                                            .child(item.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .mt_1()
                                            .truncate()
                                            .text_size(px(11.))
                                            .text_color(rgb(0x88888f))
                                            .child(item.subtitle.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .id("like")
                                    .p_2()
                                    .text_size(px(18.))
                                    .text_color(if liked { rgb(0xff3157) } else { rgb(0x8a8a91) })
                                    .cursor_pointer()
                                    .child(if liked { "♥" } else { "♡" })
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_like(cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .flex_1()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_5()
                                    .child(
                                        div()
                                            .id("shuffle")
                                            .text_color(if self.shuffle {
                                                rgb(0xff3157)
                                            } else {
                                                rgb(0x929299)
                                            })
                                            .cursor_pointer()
                                            .child("⤨")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.shuffle = !this.shuffle;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("previous")
                                            .text_size(px(22.))
                                            .text_color(rgb(0xd6d6da))
                                            .cursor_pointer()
                                            .child("◀")
                                            .on_click(cx.listener(|this, _, _, _| this.previous())),
                                    )
                                    .child(
                                        div()
                                            .id("play-pause")
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .size(px(42.))
                                            .rounded_full()
                                            .bg(rgb(0xf2f2f3))
                                            .text_color(rgb(0x151517))
                                            .text_size(px(17.))
                                            .cursor_pointer()
                                            .child(if state.phase == PlaybackPhase::Loading {
                                                "…"
                                            } else if playing {
                                                "Ⅱ"
                                            } else {
                                                "▶"
                                            })
                                            .on_click(
                                                cx.listener(|this, _, _, _| this.audio.toggle()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("next")
                                            .text_size(px(22.))
                                            .text_color(rgb(0xd6d6da))
                                            .cursor_pointer()
                                            .child("▶")
                                            .on_click(cx.listener(|this, _, _, _| this.next())),
                                    )
                                    .child(
                                        div()
                                            .id("repeat")
                                            .text_color(if self.repeat == RepeatMode::Off {
                                                rgb(0x929299)
                                            } else {
                                                rgb(0xff3157)
                                            })
                                            .cursor_pointer()
                                            .child(if self.repeat == RepeatMode::One {
                                                "↻¹"
                                            } else {
                                                "↻"
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.repeat = match this.repeat {
                                                    RepeatMode::Off => RepeatMode::All,
                                                    RepeatMode::All => RepeatMode::One,
                                                    RepeatMode::One => RepeatMode::Off,
                                                };
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(div().text_size(px(10.)).text_color(rgb(0x77777e)).child(
                                format!(
                                    "{}  /  {}",
                                    format_time(state.position.as_secs()),
                                    format_time(state.duration.as_secs())
                                ),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_4()
                            .w(px(350.))
                            .child(
                                div()
                                    .id("lyrics")
                                    .text_size(px(12.))
                                    .text_color(if self.show_lyrics {
                                        rgb(0xff6c83)
                                    } else {
                                        rgb(0xa0a0a7)
                                    })
                                    .cursor_pointer()
                                    .child("가사")
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_lyrics(cx))),
                            )
                            .child(
                                div()
                                    .id("queue")
                                    .text_size(px(17.))
                                    .text_color(if self.show_queue {
                                        rgb(0xff6c83)
                                    } else {
                                        rgb(0xa0a0a7)
                                    })
                                    .cursor_pointer()
                                    .child("☷")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_queue = !this.show_queue;
                                        this.show_lyrics = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("volume-down")
                                    .text_color(rgb(0xa0a0a7))
                                    .cursor_pointer()
                                    .child("−")
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.audio.set_volume(this.audio_snapshot.volume - 0.1)
                                    })),
                            )
                            .child(
                                div()
                                    .w(px(58.))
                                    .h(px(4.))
                                    .rounded_full()
                                    .bg(rgb(0x44444a))
                                    .child(
                                        div()
                                            .w(px(58. * state.volume))
                                            .h_full()
                                            .rounded_full()
                                            .bg(rgb(0xd5d5d8)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("volume-up")
                                    .text_color(rgb(0xa0a0a7))
                                    .cursor_pointer()
                                    .child("+")
                                    .on_click(cx.listener(|this, _, _, _| {
                                        this.audio.set_volume(this.audio_snapshot.volume + 0.1)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for PocketYtmApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .key_context("PocketYtm")
            .on_action(cx.listener(Self::on_toggle_playback))
            .on_action(cx.listener(Self::on_next_track))
            .on_action(cx.listener(Self::on_previous_track))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_check_for_updates))
            .flex()
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x101012))
            .text_color(rgb(0xeeeeef))
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_header(cx))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_center(cx))
                            .children(self.render_side_panel(cx)),
                    ),
            )
            .child(self.render_player(cx))
            .children(self.render_auth_modal(cx))
            .children(self.render_update_modal(cx))
    }
}

fn format_time(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn time_format_matches_player_conventions() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(65), "1:05");
        assert_eq!(format_time(3661), "1:01:01");
    }
}
