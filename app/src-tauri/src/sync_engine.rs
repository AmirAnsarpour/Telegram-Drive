use crate::commands::utils::resolve_peer;
use crate::commands::TelegramState;
use crate::db::DbConnection;
use grammers_client::types::{InputMessage, Media, Peer};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub enabled: bool,
    pub local_path: String,
    pub interval_seconds: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { enabled: false, local_path: String::new(), interval_seconds: 30 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub running: bool,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
    pub uploaded: u32,
    pub downloaded: u32,
}

pub struct SyncRuntime {
    running: AtomicBool,
    status: tokio::sync::RwLock<SyncStatus>,
}

impl Default for SyncRuntime {
    fn default() -> Self {
        Self {
            running: AtomicBool::new(false),
            status: tokio::sync::RwLock::new(SyncStatus {
                running: false, last_sync: None, last_error: None, uploaded: 0, downloaded: 0,
            }),
        }
    }
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("sync.json"))
}

pub fn load_config(app: &AppHandle) -> SyncConfig {
    config_path(app).ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(app: &AppHandle, config: &SyncConfig) -> Result<(), String> {
    let path = config_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| {
        if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }
    }).collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']);
    if cleaned.is_empty() { "Untitled".into() } else { cleaned.into() }
}

fn configured_folders(db: &DbConnection) -> Result<Vec<(i64, String)>, String> {
    let conn = db.lock().map_err(|_| "Database lock failed".to_string())?;
    let mut stmt = conn.prepare("SELECT channel_id, name FROM folder_metadata ORDER BY display_order, name")
        .map_err(|e| e.to_string())?;
    let mut folders = Vec::new();
    while let Ok(sqlite::State::Row) = stmt.next() {
        folders.push((
            stmt.read::<i64, _>(0).map_err(|e| e.to_string())?,
            stmt.read::<String, _>(1).map_err(|e| e.to_string())?,
        ));
    }
    Ok(folders)
}

async fn sync_peer(
    client: &grammers_client::Client,
    peer: &Peer,
    local_dir: &Path,
) -> Result<(u32, u32), String> {
    tokio::fs::create_dir_all(local_dir).await.map_err(|e| e.to_string())?;
    let mut remote = Vec::<(i32, String, u64, Media)>::new();
    let mut messages = client.iter_messages(peer);
    while let Some(message) = messages.next().await.map_err(|e| e.to_string())? {
        if let Some(media) = message.media() {
            let (name, size) = match &media {
                Media::Document(doc) => {
                    let caption = message.text();
                    let name = if caption.is_empty() { doc.name().to_string() } else { caption.to_string() };
                    (safe_name(&name), doc.size() as u64)
                }
                Media::Photo(_) => (format!("Photo-{}.jpg", message.id()), 0),
                _ => continue,
            };
            remote.push((message.id(), name, size, media));
        }
    }

    let mut downloaded = 0;
    for (_id, name, size, media) in &remote {
        let target = local_dir.join(name);
        let matches = tokio::fs::metadata(&target).await.map(|m| *size == 0 || m.len() == *size).unwrap_or(false);
        if matches { continue; }
        if target.exists() {
            let conflict = local_dir.join(format!("{}.local-conflict-{}", name, chrono::Utc::now().timestamp()));
            tokio::fs::rename(&target, conflict).await.map_err(|e| e.to_string())?;
        }
        let partial = target.with_extension(format!("{}.telegram-part", target.extension().and_then(|e| e.to_str()).unwrap_or("tmp")));
        let mut file = tokio::fs::File::create(&partial).await.map_err(|e| e.to_string())?;
        let mut iter = client.iter_download(media);
        while let Some(chunk) = iter.next().await.transpose() {
            file.write_all(&chunk.map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        tokio::fs::rename(partial, target).await.map_err(|e| e.to_string())?;
        downloaded += 1;
    }

    let mut uploaded = 0;
    let mut entries = tokio::fs::read_dir(local_dir).await.map_err(|e| e.to_string())?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let meta = entry.metadata().await.map_err(|e| e.to_string())?;
        if !meta.is_file() { continue; }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(".telegram-part") || remote.iter().any(|(_, n, s, _)| n == &name && *s == meta.len()) {
            continue;
        }
        let mut file = tokio::fs::File::open(entry.path()).await.map_err(|e| e.to_string())?;
        let _upload_guard = crate::commands::fs::telegram_upload_guard().lock().await;
        let uploaded_file = client.upload_stream(&mut file, meta.len() as usize, name.clone())
            .await.map_err(|e| e.to_string())?;
        client.send_message(peer, InputMessage::new().file(uploaded_file))
            .await.map_err(|e| e.to_string())?;
        uploaded += 1;
    }
    Ok((uploaded, downloaded))
}

pub async fn run_sync(app: AppHandle) -> Result<SyncStatus, String> {
    let runtime = app.state::<Arc<SyncRuntime>>().inner().clone();
    if runtime.running.swap(true, Ordering::SeqCst) {
        return Err("A sync is already running".into());
    }
    runtime.status.write().await.running = true;
    let result = async {
        let config = load_config(&app);
        if config.local_path.is_empty() { return Err("Choose a local sync folder first".into()); }
        let root = PathBuf::from(&config.local_path);
        tokio::fs::create_dir_all(&root).await.map_err(|e| e.to_string())?;
        let tg = app.state::<TelegramState>();
        let client = tg.client.lock().await.clone().ok_or("Telegram is not connected")?;
        let db = app.state::<DbConnection>();
        let mut total = (0, 0);
        let root_peer = resolve_peer(&client, None, &tg.peer_cache).await?;
        let counts = sync_peer(&client, &root_peer, &root.join("Saved Messages")).await?;
        total.0 += counts.0; total.1 += counts.1;
        for (id, name) in configured_folders(&db)? {
            let peer = resolve_peer(&client, Some(id), &tg.peer_cache).await?;
            let counts = sync_peer(&client, &peer, &root.join(safe_name(&name))).await?;
            total.0 += counts.0; total.1 += counts.1;
        }
        Ok::<_, String>(total)
    }.await;
    runtime.running.store(false, Ordering::SeqCst);
    let mut status = runtime.status.write().await;
    status.running = false;
    status.last_sync = Some(chrono::Utc::now().timestamp());
    match result {
        Ok((uploaded, downloaded)) => {
            status.uploaded = uploaded; status.downloaded = downloaded; status.last_error = None;
        }
        Err(error) => status.last_error = Some(error),
    }
    let snapshot = status.clone();
    let _ = app.emit("sync-status", &snapshot);
    Ok(snapshot)
}

#[tauri::command]
pub async fn cmd_get_sync_config(app: AppHandle) -> Result<SyncConfig, String> {
    Ok(load_config(&app))
}

#[tauri::command]
pub async fn cmd_set_sync_config(config: SyncConfig, app: AppHandle) -> Result<SyncConfig, String> {
    if config.interval_seconds < 15 { return Err("Sync interval must be at least 15 seconds".into()); }
    if config.enabled && config.local_path.is_empty() { return Err("Choose a local folder".into()); }
    save_config(&app, &config)?;
    Ok(config)
}

#[tauri::command]
pub async fn cmd_sync_now(app: AppHandle) -> Result<SyncStatus, String> {
    run_sync(app).await
}

#[tauri::command]
pub async fn cmd_get_sync_status(runtime: State<'_, Arc<SyncRuntime>>) -> Result<SyncStatus, String> {
    Ok(runtime.status.read().await.clone())
}

pub fn start_background_sync(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let config = load_config(&app);
            let delay = config.interval_seconds.max(15);
            if config.enabled && !config.local_path.is_empty() {
                let _ = run_sync(app.clone()).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn creates_names_valid_on_macos_and_windows() {
        assert_eq!(safe_name("work/report:final?.pdf"), "work_report_final_.pdf");
        assert_eq!(safe_name("notes. "), "notes");
        assert_eq!(safe_name(""), "Untitled");
    }
}
