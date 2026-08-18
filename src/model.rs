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
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
    #[serde(default)]
    pub explicit: bool,
}

impl MediaItem {
    pub fn playable(&self) -> bool {
        self.video_id.as_ref().is_some_and(|id| !id.is_empty())
    }

    pub fn watch_url(&self) -> Option<String> {
        self.video_id
            .as_ref()
            .map(|id| format!("https://music.youtube.com/watch?v={id}"))
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
    fn playable_items_generate_music_urls() {
        let item = MediaItem {
            video_id: Some("abc123".into()),
            ..Default::default()
        };
        assert!(item.playable());
        assert_eq!(
            item.watch_url().as_deref(),
            Some("https://music.youtube.com/watch?v=abc123")
        );
    }
}
