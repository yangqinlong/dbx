use std::sync::Arc;

use tauri::State;

use dbx_core::agent_manager::{AgentDriverInfo, AgentManager, AgentRegistry, InstalledDriver};
use dbx_core::connection::AppState;

const REGISTRY_PATH: &str = "https://github.com/t8y2/dbx-agents/releases/latest/download/agent-registry.json";

#[tauri::command]
pub async fn list_installed_agents(state: State<'_, Arc<AppState>>) -> Result<Vec<AgentDriverInfo>, String> {
    let am = &state.agent_manager;
    let local_state = am.load_state();
    let registry = fetch_registry().await.ok();

    let agent_types = [
        ("dameng", "达梦 DM8"),
        ("kingbase", "人大金仓 KingbaseES"),
        ("vastbase", "Vastbase"),
        ("goldendb", "GoldenDB"),
    ];

    Ok(agent_types
        .iter()
        .map(|(key, label)| {
            let installed = am.is_driver_installed(key);
            let local = local_state.installed_drivers.get(*key);
            let remote = registry.as_ref().and_then(|r| r.drivers.get(*key));
            AgentDriverInfo {
                db_type: key.to_string(),
                label: label.to_string(),
                version: remote.map(|r| r.version.clone()).unwrap_or_default(),
                size: remote.map(|r| r.jar.size).unwrap_or(0),
                installed,
                installed_version: local.map(|l| l.version.clone()),
                update_available: match (local, remote) {
                    (Some(l), Some(r)) => l.version != r.version,
                    _ => false,
                },
            }
        })
        .collect())
}

#[tauri::command]
pub async fn install_agent(state: State<'_, Arc<AppState>>, db_type: String) -> Result<(), String> {
    let am = &state.agent_manager;
    let registry = fetch_registry().await?;

    if !am.is_jre_installed() {
        let platform = AgentManager::current_platform();
        let jre_info =
            registry.jre.platforms.get(platform).ok_or_else(|| format!("No JRE available for platform: {platform}"))?;
        let jre_archive = am.base_dir().join("jre-download.tar.gz");
        download_with_proxy(&jre_info.url, &jre_archive).await?;
        extract_archive(&jre_archive, &am.base_dir().join("jre"))?;
        std::fs::remove_file(&jre_archive).ok();
    }

    let driver = registry.drivers.get(&db_type).ok_or_else(|| format!("Unknown driver type: {db_type}"))?;
    let jar_path = am.driver_jar_path(&db_type);
    download_with_proxy(&driver.jar.url, &jar_path).await?;

    let mut local_state = am.load_state();
    local_state.jre_version = Some(registry.jre.version.clone());
    local_state.installed_drivers.insert(
        db_type,
        InstalledDriver { version: driver.version.clone(), installed_at: chrono::Utc::now().to_rfc3339() },
    );
    am.save_state(&local_state)?;
    Ok(())
}

#[tauri::command]
pub async fn uninstall_agent(state: State<'_, Arc<AppState>>, db_type: String) -> Result<(), String> {
    let am = &state.agent_manager;
    let jar_path = am.driver_jar_path(&db_type);
    if jar_path.exists() {
        std::fs::remove_file(&jar_path).map_err(|e| e.to_string())?;
    }
    let driver_dir = jar_path.parent().unwrap();
    if driver_dir.exists() {
        std::fs::remove_dir_all(driver_dir).map_err(|e| e.to_string())?;
    }
    let mut local_state = am.load_state();
    local_state.installed_drivers.remove(&db_type);
    am.save_state(&local_state)?;
    Ok(())
}

#[tauri::command]
pub async fn check_jre_installed(state: State<'_, Arc<AppState>>) -> Result<bool, String> {
    Ok(state.agent_manager.is_jre_installed())
}

#[tauri::command]
pub async fn reinstall_jre(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let am = &state.agent_manager;
    let jre_dir = am.base_dir().join("jre");
    if jre_dir.exists() {
        std::fs::remove_dir_all(&jre_dir).map_err(|e| format!("Failed to remove old JRE: {e}"))?;
    }
    let registry = fetch_registry().await?;
    let platform = AgentManager::current_platform();
    let jre_info =
        registry.jre.platforms.get(platform).ok_or_else(|| format!("No JRE available for platform: {platform}"))?;
    let jre_archive = am.base_dir().join("jre-download.tar.gz");
    download_with_proxy(&jre_info.url, &jre_archive).await?;
    extract_archive(&jre_archive, &jre_dir)?;
    std::fs::remove_file(&jre_archive).ok();
    let mut local_state = am.load_state();
    local_state.jre_version = Some(registry.jre.version.clone());
    am.save_state(&local_state)?;
    Ok(())
}

async fn fetch_registry() -> Result<AgentRegistry, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;
    let mut last_err = String::new();
    for proxy in dbx_core::GITHUB_PROXIES {
        let url = format!("{proxy}{REGISTRY_PATH}");
        match client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "dbx-agent-manager")
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => {
                return resp.json().await.map_err(|e| format!("Failed to parse registry: {e}"));
            }
            Err(e) => {
                last_err = format!("{e}");
            }
        }
    }
    Err(format!("Failed to fetch agent registry: {last_err}"))
}

async fn download_with_proxy(url: &str, dest: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;
    let mut last_err = String::new();
    for proxy in dbx_core::GITHUB_PROXIES {
        let full_url = format!("{proxy}{url}");
        log::info!("[agent] downloading from {full_url}");
        match client
            .get(&full_url)
            .header(reqwest::header::USER_AGENT, "dbx-agent-manager")
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => {
                let bytes = resp.bytes().await.map_err(|e| format!("Download read failed: {e}"))?;
                std::fs::write(dest, &bytes).map_err(|e| format!("Failed to write file: {e}"))?;
                return Ok(());
            }
            Err(e) => {
                last_err = format!("{e}");
                log::warn!("[agent] download failed from {full_url}: {last_err}");
            }
        }
    }
    Err(format!("Failed to download {url}: {last_err}"))
}

fn extract_archive(archive: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    use std::process::Command;
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let status = Command::new("tar")
        .args(["xzf", &archive.to_string_lossy(), "-C", &dest.to_string_lossy(), "--strip-components=1"])
        .status()
        .map_err(|e| format!("Failed to extract archive: {e}"))?;
    if !status.success() {
        return Err("Failed to extract JRE archive".to_string());
    }
    Ok(())
}
