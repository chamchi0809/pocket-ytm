use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub handle: String,
    #[serde(default)]
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub video_id: Option<String>,
    #[serde(default)]
    pub browse_id: Option<String>,
    #[serde(default)]
    pub playlist_id: Option<String>,
    #[serde(default)]
    pub source_playlist_id: Option<String>,
    #[serde(default)]
    pub source_index: Option<usize>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
    #[serde(default)]
    pub available: Option<bool>,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub liked: bool,
}

impl MediaItem {
    pub fn playable(&self) -> bool {
        self.has_direct_video() || self.fallback_searchable()
    }

    pub fn has_direct_video(&self) -> bool {
        self.is_available() && self.video_id.as_ref().is_some_and(|id| !id.is_empty())
    }

    pub fn fallback_searchable(&self) -> bool {
        self.kind == "song"
            && !self.title.trim().is_empty()
            && self.video_id.as_ref().is_none_or(|id| id.is_empty())
    }

    pub fn fallback_search_query(&self) -> Option<String> {
        self.fallback_searchable().then(|| {
            format!("{} {}", self.title, self.subtitle.replace(" · ", " "))
                .trim()
                .to_owned()
        })
    }

    pub fn direct_audio_fallback_query(&self) -> Option<String> {
        self.has_direct_video().then(|| {
            format!(
                "{} {} official audio",
                self.title,
                self.subtitle.replace(" · ", " ")
            )
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
        })
    }

    pub fn is_available(&self) -> bool {
        self.available != Some(false)
    }

    pub fn browsable(&self) -> bool {
        match self.kind.as_str() {
            "artist" | "album" | "single" => {
                self.browse_id.as_ref().is_some_and(|id| !id.is_empty())
            }
            "playlist" => self
                .playlist_id
                .as_ref()
                .or(self.browse_id.as_ref())
                .is_some_and(|id| !id.is_empty()),
            _ => false,
        }
    }

    pub fn watch_url(&self) -> Option<String> {
        self.video_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://www.youtube.com/watch?v={id}"))
    }

    pub fn youtube_playlist_id(&self) -> Option<String> {
        self.playlist_id
            .as_deref()
            .or(self.browse_id.as_deref())
            .filter(|id| !id.is_empty())
            .map(|id| id.strip_prefix("VL").unwrap_or(id).to_owned())
    }

    pub fn canonical_source(&self) -> Option<(&str, usize)> {
        let playlist_id = self.source_playlist_id.as_deref()?;
        let playlist_id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
        Some((playlist_id, self.source_index?))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaSection {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub items: Vec<MediaItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowsePage {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub sections: Vec<MediaSection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WatchQueue {
    #[serde(default)]
    pub playlist_id: Option<String>,
    #[serde(default)]
    pub lyrics_browse_id: Option<String>,
    #[serde(default)]
    pub items: Vec<MediaItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playable_items_generate_standard_youtube_urls() {
        let item = MediaItem {
            video_id: Some("abc123".into()),
            available: Some(true),
            ..Default::default()
        };
        assert!(item.playable());
        assert_eq!(
            item.watch_url().as_deref(),
            Some("https://www.youtube.com/watch?v=abc123")
        );
    }

    #[test]
    fn unavailable_catalog_tracks_use_youtube_search_fallback() {
        let item = MediaItem {
            kind: "song".into(),
            title: "Track title".into(),
            subtitle: "Artist · Album".into(),
            video_id: None,
            available: Some(false),
            ..Default::default()
        };

        assert!(item.playable());
        assert!(item.fallback_searchable());
        assert_eq!(
            item.fallback_search_query().as_deref(),
            Some("Track title Artist Album")
        );
        assert!(!item.browsable());
    }

    #[test]
    fn direct_video_tracks_have_an_audio_only_fallback_query() {
        let item = MediaItem {
            kind: "song".into(),
            title: "Whiplash".into(),
            subtitle: "aespa · Whiplash".into(),
            video_id: Some("blocked-video".into()),
            available: Some(true),
            ..Default::default()
        };

        assert_eq!(
            item.direct_audio_fallback_query().as_deref(),
            Some("Whiplash aespa Whiplash official audio")
        );
    }

    #[test]
    fn artist_and_album_items_are_browsable() {
        for kind in ["artist", "album", "single"] {
            let item = MediaItem {
                kind: kind.into(),
                browse_id: Some("MPRE-or-channel".into()),
                available: Some(true),
                ..Default::default()
            };
            assert!(item.browsable());
        }
    }

    #[test]
    fn youtube_playlist_ids_drop_the_music_browse_prefix() {
        let item = MediaItem {
            kind: "playlist".into(),
            playlist_id: Some("VLRDCLAK5uy_example".into()),
            ..Default::default()
        };

        assert_eq!(
            item.youtube_playlist_id().as_deref(),
            Some("RDCLAK5uy_example")
        );
    }

    #[test]
    fn canonical_sources_preserve_collection_and_track_position() {
        let item = MediaItem {
            source_playlist_id: Some("VLOLAK5uy_exact-album".into()),
            source_index: Some(7),
            ..Default::default()
        };

        assert_eq!(item.canonical_source(), Some(("OLAK5uy_exact-album", 7)));
    }
}
