// Apple Music "now playing" and transport controls, through the Windows System
// Media Transport Controls (SMTC).
//
// Event driven: nothing polls. A dedicated thread subscribes to the session
// manager once; the WinRT thread pool calls us back when the session list, the
// track or the playback state changes, and each callback emits one snapshot on
// the existing `integration` event. No key, no network, no account — SMTC is how
// the Apple Music app already tells Windows what it is playing.

use std::sync::atomic::Ordering;
use std::sync::Mutex;

use base64::Engine;
use serde_json::{json, Value};
use tauri::AppHandle;
use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::{DataReader, IInputStream};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::integrations::{self, IntegrationUpdate};
use crate::log;

const ID: &str = "integration_applemusic";

/// Covers are a few tens of KB; anything past this is not a cover.
const MAX_ARTWORK_BYTES: u32 = 2_000_000;

struct Attached {
    session: Session,
    props_token: i64,
    playback_token: i64,
}

struct Inner {
    manager: Option<Manager>,
    attached: Option<Attached>,
    /// Cover for the current track, kept so every snapshot can carry it.
    track_key: String,
    artwork: Option<String>,
}

static INNER: Mutex<Inner> = Mutex::new(Inner {
    manager: None,
    attached: None,
    track_key: String::new(),
    artwork: None,
});

// ── Pure helpers (unit tested) ────────────────────────────────────────────────

/// The Apple Music app's AppUserModelId looks like `AppleInc.AppleMusicWin_…!App`.
fn is_apple_music(app_user_model_id: &str) -> bool {
    app_user_model_id.to_ascii_lowercase().contains("applemusic")
}

fn track_key(title: &str, artist: &str, album: &str) -> String {
    format!("{title}\u{1}{artist}\u{1}{album}")
}

/// `mime` may be a list ("image/jpeg,image/jpe,image/jpg", which is what Apple
/// Music reports) or carry parameters; only the first type belongs in a data URL.
fn data_url(mime: &str, bytes: &[u8]) -> String {
    let first = mime.split([',', ';']).next().unwrap_or("").trim();
    let mime = if first.starts_with("image/") { first } else { "image/jpeg" };
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn snapshot_json(
    has_session: bool,
    playing: bool,
    title: &str,
    artist: &str,
    album: &str,
    artwork: Option<&str>,
) -> Value {
    json!({
        "hasSession": has_session,
        "playing": playing,
        "title": title,
        "artist": artist,
        "album": album,
        "artwork": artwork,
    })
}

// ── WinRT plumbing ────────────────────────────────────────────────────────────

/// Threads we create ourselves join the multithreaded apartment. A failure just
/// means the thread already has one, which is fine.
fn init_apartment() {
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
}

fn find_session(manager: &Manager) -> Option<Session> {
    let sessions = manager.GetSessions().ok()?;
    sessions.into_iter().find(|s| {
        s.SourceAppUserModelId()
            .map(|id| is_apple_music(&id.to_string()))
            .unwrap_or(false)
    })
}

fn read_artwork(session: &Session) -> Option<String> {
    let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = u32::try_from(stream.Size().ok()?).ok()?;
    if size == 0 || size > MAX_ARTWORK_BYTES {
        return None;
    }
    let mime = stream.ContentType().map(|s| s.to_string()).unwrap_or_default();
    let reader = DataReader::CreateDataReader(&stream.cast::<IInputStream>().ok()?).ok()?;
    reader.LoadAsync(size).ok()?.get().ok()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(data_url(&mime, &bytes))
}

/// Builds the snapshot for the attached session, fetching the cover when the
/// track changed or the last attempt found none (Apple Music sometimes publishes
/// the thumbnail a moment after the title).
fn build_snapshot() -> Value {
    let session = {
        let inner = INNER.lock().unwrap();
        inner.attached.as_ref().map(|a| a.session.clone())
    };
    let Some(session) = session else {
        return snapshot_json(false, false, "", "", "", None);
    };

    let (title, artist, album) = match session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.get())
    {
        Ok(p) => (
            p.Title().map(|s| s.to_string()).unwrap_or_default(),
            p.Artist().map(|s| s.to_string()).unwrap_or_default(),
            p.AlbumTitle().map(|s| s.to_string()).unwrap_or_default(),
        ),
        Err(_) => (String::new(), String::new(), String::new()),
    };
    let playing = session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
        .map(|s| s == Status::Playing)
        .unwrap_or(false);

    let key = track_key(&title, &artist, &album);
    let needs_artwork = {
        let inner = INNER.lock().unwrap();
        inner.track_key != key || inner.artwork.is_none()
    };
    if needs_artwork && !title.is_empty() {
        let artwork = read_artwork(&session);
        let mut inner = INNER.lock().unwrap();
        inner.track_key = key;
        inner.artwork = artwork;
    }
    let artwork = INNER.lock().unwrap().artwork.clone();
    snapshot_json(true, playing, &title, &artist, &album, artwork.as_deref())
}

fn gated(app: &AppHandle) -> bool {
    integrations::PAUSED.load(Ordering::Relaxed) || !integrations::enabled(app, ID)
}

fn emit_snapshot(app: &AppHandle) {
    if gated(app) {
        return;
    }
    integrations::emit(
        app,
        IntegrationUpdate { id: ID, data: build_snapshot(), error: None, event: None },
    );
}

/// Drops the old session's handlers, finds Apple Music's current session, hooks
/// it, and emits. Runs at start and whenever Windows says the session list moved.
fn attach(app: &AppHandle) {
    let manager = INNER.lock().unwrap().manager.clone();
    let Some(manager) = manager else { return };

    {
        let mut inner = INNER.lock().unwrap();
        if let Some(old) = inner.attached.take() {
            let _ = old.session.RemoveMediaPropertiesChanged(old.props_token);
            let _ = old.session.RemovePlaybackInfoChanged(old.playback_token);
        }
        inner.track_key.clear();
        inner.artwork = None;
    }

    if let Some(session) = find_session(&manager) {
        let on_props = {
            let app = app.clone();
            TypedEventHandler::new(move |_, _| {
                emit_snapshot(&app);
                Ok(())
            })
        };
        let on_playback = {
            let app = app.clone();
            TypedEventHandler::new(move |_, _| {
                emit_snapshot(&app);
                Ok(())
            })
        };
        match (
            session.MediaPropertiesChanged(&on_props),
            session.PlaybackInfoChanged(&on_playback),
        ) {
            (Ok(props_token), Ok(playback_token)) => {
                INNER.lock().unwrap().attached = Some(Attached { session, props_token, playback_token });
            }
            _ => log::line("media: could not subscribe to the Apple Music session"),
        }
    }
    emit_snapshot(app);
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Starts the listener thread. Safe to call once, from `setup`.
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        init_apartment();
        let manager = match Manager::RequestAsync().and_then(|op| op.get()) {
            Ok(m) => m,
            Err(err) => {
                log::line(format!("media: SMTC unavailable: {err}"));
                return;
            }
        };
        INNER.lock().unwrap().manager = Some(manager.clone());

        let handler_app = app.clone();
        let on_sessions = TypedEventHandler::new(move |_, _| {
            attach(&handler_app);
            Ok(())
        });
        if let Err(err) = manager.SessionsChanged(&on_sessions) {
            log::line(format!("media: could not watch sessions: {err}"));
            return;
        }
        attach(&app);
    });
}

/// Re-emits the current state after Pause or a Settings change, because nothing
/// was emitted while the integration was off. Work runs on its own thread: the
/// WinRT calls block.
pub fn refresh(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        init_apartment();
        emit_snapshot(&app);
    });
}

/// Transport control for the island's buttons: `prev`, `toggle` or `next`.
pub fn control(action: &str) -> Result<(), String> {
    init_apartment();
    let manager = Manager::RequestAsync()
        .and_then(|op| op.get())
        .map_err(|e| format!("Media controls unavailable: {e}"))?;
    let session = find_session(&manager).ok_or_else(|| "Apple Music isn't playing".to_string())?;
    let op = match action {
        "prev" => session.TrySkipPreviousAsync(),
        "toggle" => session.TryTogglePlayPauseAsync(),
        "next" => session.TrySkipNextAsync(),
        other => return Err(format!("Unknown media action: {other}")),
    };
    match op.and_then(|o| o.get()) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Apple Music refused that command".into()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_apple_music_app_only() {
        assert!(is_apple_music("AppleInc.AppleMusicWin_nzyj5cx40ttqa!App"));
        assert!(is_apple_music("applemusic.exe"));
        assert!(!is_apple_music("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"));
        assert!(!is_apple_music("chrome.exe"));
        assert!(!is_apple_music(""));
    }

    #[test]
    fn a_track_key_changes_with_any_field() {
        let base = track_key("Song", "Artist", "Album");
        assert_eq!(base, track_key("Song", "Artist", "Album"));
        assert_ne!(base, track_key("Song 2", "Artist", "Album"));
        assert_ne!(base, track_key("Song", "Other", "Album"));
        assert_ne!(base, track_key("Song", "Artist", "Other"));
        // The separator keeps ("ab","c") and ("a","bc") apart.
        assert_ne!(track_key("ab", "c", ""), track_key("a", "bc", ""));
    }

    #[test]
    fn data_url_encodes_and_falls_back_to_jpeg() {
        assert_eq!(data_url("image/png", &[1, 2, 3]), "data:image/png;base64,AQID");
        assert_eq!(data_url("", &[1, 2, 3]), "data:image/jpeg;base64,AQID");
        assert_eq!(data_url("text/html", &[1, 2, 3]), "data:image/jpeg;base64,AQID");
    }

    #[test]
    fn data_url_keeps_only_the_first_type_of_a_list() {
        // Apple Music reports exactly this; a comma would end the media type and
        // leave the browser decoding "image/jpe,image/jpg;base64," as data.
        assert_eq!(
            data_url("image/jpeg,image/jpe,image/jpg", &[1, 2, 3]),
            "data:image/jpeg;base64,AQID"
        );
        assert_eq!(data_url("image/png; charset=x", &[1, 2, 3]), "data:image/png;base64,AQID");
    }

    #[test]
    fn snapshot_has_the_fields_the_island_reads() {
        let v = snapshot_json(true, true, "T", "A", "B", Some("data:x"));
        assert_eq!(v["hasSession"], true);
        assert_eq!(v["playing"], true);
        assert_eq!(v["title"], "T");
        assert_eq!(v["artwork"], "data:x");
        assert!(snapshot_json(false, false, "", "", "", None)["artwork"].is_null());
    }
}
