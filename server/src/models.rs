use serde::{Deserialize, Deserializer, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::auth::Role;

pub fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OidcBinding {
    pub claim_value: String,
    pub role: Role,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Image,
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }
}

#[derive(Deserialize)]
pub struct MediaInput {
    pub folder_id: String,
    pub name: String,
    pub kind: MediaKind,
    pub sha256: String,
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub codec: Option<String>,
    pub hdr: Option<bool>,
    pub frame_rate: Option<f64>,
    pub source_filename: Option<String>,
    pub thumbnail_sha256: Option<String>,
}

#[derive(Deserialize)]
pub struct MediaPatch {
    pub name: Option<String>,
    pub folder_id: Option<String>,
}

#[derive(Serialize, FromRow, Clone, Debug)]
pub struct Media {
    pub id: String,
    pub library_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub sha256: String,
    pub mime: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub codec: Option<String>,
    pub hdr: Option<bool>,
    pub frame_rate: Option<f64>,
    pub source_filename: Option<String>,
    pub thumbnail_sha256: Option<String>,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_at: Option<String>,
    pub created_by: Option<String>,
    pub created_at: String,
    #[sqlx(skip)]
    pub missing: bool,
    #[sqlx(skip)]
    pub access: &'static str,
}

impl Media {
    pub fn with_access(mut self, access: &'static str) -> Self {
        self.missing = self.missing_at.is_some();
        self.access = access;
        self
    }
}

pub const MEDIA_COLUMNS: &str = "id, library_id, folder_id, name, path, kind, sha256, mime, width, height, duration_ms, codec, hdr, frame_rate, source_filename, thumbnail_sha256, size, missing_at, created_by, created_at";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum TransitionType {
    Cut,
    Dissolve,
    Push,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Transition {
    #[serde(rename = "type")]
    pub kind: TransitionType,
    pub duration_ms: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum DefaultThen {
    Auto,
    Hold,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum DefaultWaiting {
    HoldLastFrame,
    Loop,
    Black,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum DefaultClipAudio {
    Mute,
    Play,
    DuckBed,
    PauseBed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct PlaylistDefaults {
    pub then: DefaultThen,
    pub while_waiting: DefaultWaiting,
    pub transition: Transition,
    pub image_duration_ms: u64,
    pub clip_audio: DefaultClipAudio,
}

impl Default for PlaylistDefaults {
    fn default() -> Self {
        Self {
            then: DefaultThen::Auto,
            while_waiting: DefaultWaiting::HoldLastFrame,
            transition: Transition { kind: TransitionType::Dissolve, duration_ms: 1000 },
            image_duration_ms: 10000,
            clip_audio: DefaultClipAudio::DuckBed,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Video,
    Image,
    Sequence,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AdvanceMode {
    Inherit,
    Auto,
    Hold,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum WaitingDisplay {
    Inherit,
    HoldLastFrame,
    Loop,
    Black,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ClipAudio {
    Inherit,
    Mute,
    Play,
    DuckBed,
    PauseBed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlaylistItem {
    pub id: Option<Uuid>,
    pub kind: ItemKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<Vec<Uuid>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub then: Option<AdvanceMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub while_waiting: Option<WaitingDisplay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_audio: Option<ClipAudio>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
}

fn d_volume() -> f64 {
    0.8
}
fn d_duck() -> f64 {
    0.2
}
fn d_fade() -> u64 {
    1500
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bed {
    pub id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub start_item_id: Uuid,
    pub end_item_id: Uuid,
    pub tracks: Vec<Uuid>,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default = "d_volume")]
    pub volume: f64,
    #[serde(default = "d_duck")]
    pub duck_volume: f64,
    #[serde(default = "d_fade")]
    pub fade_ms: u64,
}

#[derive(Deserialize)]
pub struct PlaylistInput {
    pub team_id: String,
    pub name: String,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(rename = "loop")]
    pub looping: Option<bool>,
    pub defaults: Option<PlaylistDefaults>,
    #[serde(default)]
    pub items: Vec<PlaylistItem>,
    #[serde(default)]
    pub beds: Vec<Bed>,
}

#[derive(Deserialize)]
pub struct PlaylistUpdate {
    pub base_version: i64,
    pub name: String,
    #[serde(rename = "loop")]
    pub looping: bool,
    pub defaults: PlaylistDefaults,
    pub items: Vec<PlaylistItem>,
    pub beds: Vec<Bed>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Playlist {
    pub id: String,
    pub team_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    pub tag_ids: Vec<String>,
    pub name: String,
    pub version: i64,
    #[serde(rename = "loop")]
    pub looping: bool,
    pub defaults: PlaylistDefaults,
    pub items: Vec<PlaylistItem>,
    pub beds: Vec<Bed>,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
}

#[derive(FromRow)]
pub struct PlaylistRow {
    pub id: String,
    pub team_id: String,
    pub folder_id: Option<String>,
    pub name: String,
    pub version: i64,
    #[sqlx(rename = "loop")]
    pub looping: bool,
    pub defaults: String,
    pub items: String,
    pub beds: String,
    pub updated_at: String,
    pub updated_by: Option<String>,
}

pub const PLAYLIST_COLUMNS: &str =
    "id, team_id, folder_id, name, version, loop, defaults, items, beds, updated_at, updated_by";

impl PlaylistRow {
    pub fn into_playlist(self, tag_ids: Vec<String>) -> Result<Playlist, serde_json::Error> {
        Ok(Playlist {
            id: self.id,
            team_id: self.team_id,
            folder_id: self.folder_id,
            tag_ids,
            name: self.name,
            version: self.version,
            looping: self.looping,
            defaults: serde_json::from_str(&self.defaults)?,
            items: serde_json::from_str(&self.items)?,
            beds: serde_json::from_str(&self.beds)?,
            updated_at: self.updated_at,
            updated_by: self.updated_by,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    Offline,
    Idle,
    Caching,
    Playing,
    Holding,
    Blackout,
    Error,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CacheProgress {
    pub ready: i64,
    pub total: i64,
    pub bytes_ready: i64,
    pub bytes_total: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScreenStatus {
    pub state: ScreenState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holding: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blackout: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl ScreenStatus {
    pub fn simple(state: ScreenState) -> Self {
        Self {
            state,
            playlist_version: None,
            item_id: None,
            item_index: None,
            holding: None,
            cache: None,
            app_version: None,
            os_version: None,
            model: None,
            message: None,
            step_index: None,
            step_count: None,
            item_count: None,
            position_ms: None,
            duration_ms: None,
            blackout: None,
            updated_at: None,
        }
    }
}

#[derive(Serialize)]
pub struct Screen {
    pub id: String,
    pub name: String,
    pub tag_ids: Vec<String>,
    pub team_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_playlist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    pub override_: Option<ScreenOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<ScreenPlacement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub output_keys: Vec<String>,
    pub online: bool,
    pub status: ScreenStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

#[derive(FromRow)]
pub struct ScreenRow {
    pub id: String,
    pub name: String,
    pub fallback_playlist_id: Option<String>,
    pub timezone: Option<String>,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub override_set: bool,
    pub override_playlist_id: Option<String>,
    pub override_until: Option<String>,
    pub override_set_at: Option<String>,
    pub override_set_by: Option<String>,
    pub override_set_by_name: Option<String>,
    pub override_team_id: Option<String>,
    pub placement_plan_id: Option<String>,
    pub placement_x: Option<f64>,
    pub placement_y: Option<f64>,
    pub node_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScreenPlacement {
    pub floor_plan_id: String,
    pub x: f64,
    pub y: f64,
}

pub const SCREEN_COLUMNS: &str = "id, name, fallback_playlist_id, timezone, last_seen_at, created_at, override_set, \
     override_playlist_id, override_until, override_set_at, override_set_by, override_set_by_name, override_team_id, \
     placement_plan_id, placement_x, placement_y, node_id";

#[derive(Serialize, Clone, Debug)]
pub struct ScreenOverride {
    pub playlist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    pub set_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_by_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
}

#[derive(Deserialize)]
pub struct ScreenOverrideInput {
    pub playlist_id: Option<String>,
    #[serde(default)]
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub until_next_change: bool,
}

#[derive(Deserialize)]
pub struct ScreenPatch {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub fallback_playlist_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub timezone: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub placement: Option<Option<ScreenPlacement>>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CommandName {
    Go,
    Next,
    Prev,
    Blackout,
    Reload,
    Identify,
    Jump,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScreenCommand {
    pub name: CommandName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Display,
    External,
    BuiltIn,
    Decklink,
    Virtual,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputRole {
    Program,
    Control,
    Confidence,
    #[default]
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Output {
    pub key: String,
    pub kind: OutputKind,
    pub name: String,
    /// Legacy mirror of the output's assignment job; hardware reports may omit it.
    #[serde(default)]
    pub role: OutputRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device: Option<String>,
}

#[derive(FromRow)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub os_version: Option<String>,
    pub app_version: Option<String>,
    pub outputs: String,
    pub single: bool,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub mode: String,
    pub assignments_version: i64,
    pub applied_assignments_version: i64,
}

pub const NODE_COLUMNS: &str = "id, name, model, os_version, app_version, outputs, single, created_at, last_seen_at, mode, assignments_version, applied_assignments_version";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OutputStatus {
    pub key: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_frames: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_frames: Option<i64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct NodeStatus {
    pub outputs: Vec<OutputStatus>,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    pub outputs: Vec<Output>,
    pub screen_ids: Vec<String>,
    pub mode: String,
    pub assignments_version: i64,
    pub applied_assignments_version: i64,
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<NodeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    pub created_at: String,
}
