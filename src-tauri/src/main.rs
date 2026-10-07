use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, time::Duration};
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

mod niimbot;

const CENTRAL_ENDPOINT: &str =
    "https://central.zeoz.com.ar/wp-json/gtc/v1/app/resolve";

const REMEMBER_ME_SCRIPT: &str = r#"
(() => {
  const activateRememberMe = () => {
    try {
      if (!window.location.pathname.includes('/wp-login.php')) return;
      const remember = document.getElementById('rememberme');
      if (remember) {
        remember.checked = true;
        remember.setAttribute('checked', 'checked');
      }
    } catch (_) {}
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', activateRememberMe, { once: true });
  } else {
    activateRememberMe();
  }
})();
"#;

#[cfg(target_os = "macos")]
const NATIVE_BRIDGE_SCRIPT: &str = r#"
(() => {
  const core = window.__TAURI__ && window.__TAURI__.core;
  if (!core || typeof core.invoke !== 'function') return;

  let connected = false;
  let callback = null;

  Object.defineProperty(window, 'ZEOZZGTPrintNativeCallback', {
    configurable: true,
    get() { return callback; },
    set(value) {
      callback = function (event) {
        const type = event && event.type;
        if (type === 'connected') connected = true;
        if (type === 'disconnected' || type === 'error') connected = false;
        return value(event);
      };
    }
  });

  const notifyError = (error) => {
    const message = error && error.message
      ? error.message
      : String(error || 'No se pudo completar la impresión Bluetooth.');
    if (callback) callback({ type: 'error', message });
  };

  window.ZGTNative = {
    appVersion: () => core.invoke('app_version'),
    printNiimbotB1Pro(dataUrl) {
      core.invoke('print_niimbot_b1_pro', { data_url: dataUrl }).catch(notifyError);
    },
    disconnectNiimbotB1Pro() {
      core.invoke('disconnect_niimbot_b1_pro').catch(notifyError);
    },
    isNiimbotB1ProConnected() {
      return connected;
    }
  };
})();
"#;

#[cfg(not(target_os = "macos"))]
const NATIVE_BRIDGE_SCRIPT: &str = "";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LinkState {
    code: String,
    workshop_name: String,
    site_url: String,
}

#[derive(Debug, Deserialize)]
struct CentralResponse {
    ok: bool,
    workshop_name: Option<String>,
    site_url: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResolvedWorkshop {
    workshop_name: String,
    site_url: String,
}

fn link_state_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("No se pudo acceder a los datos de ZGT: {error}"))?;

    fs::create_dir_all(&dir)
        .map_err(|error| format!("No se pudo preparar la carpeta de ZGT: {error}"))?;

    Ok(dir.join("link.json"))
}

fn normalize_site_url(value: &str) -> Result<String, String> {
    let parsed = Url::parse(value.trim())
        .map_err(|_| "La URL del taller no es válida.".to_string())?;

    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err("La URL del taller no es segura.".to_string());
    }

    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

#[tauri::command]
async fn resolve_link(code: String) -> Result<ResolvedWorkshop, String> {
    let normalized_code = code.trim().to_uppercase();

    if normalized_code.len() != 13 || !normalized_code.starts_with("ZGT-") {
        return Err("Revisá el formato del código.".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|_| "No pudimos iniciar la conexión con ZGT.".to_string())?;

    let response = client
        .post(CENTRAL_ENDPOINT)
        .json(&serde_json::json!({ "code": normalized_code }))
        .send()
        .await
        .map_err(|_| {
            "No pudimos conectar con ZGT. Revisá tu conexión e intentá nuevamente.".to_string()
        })?;

    let status = response.status();

    let payload: CentralResponse = response
        .json()
        .await
        .map_err(|_| "Central devolvió una respuesta no válida.".to_string())?;

    if !status.is_success() || !payload.ok {
        return Err(payload
            .message
            .unwrap_or_else(|| "El código de vinculación no es válido.".to_string()));
    }

    let site_url = payload
        .site_url
        .as_deref()
        .ok_or_else(|| "Central no devolvió la URL del taller.".to_string())
        .and_then(normalize_site_url)?;

    Ok(ResolvedWorkshop {
        workshop_name: payload
            .workshop_name
            .unwrap_or_else(|| "Taller".to_string()),
        site_url,
    })
}

#[tauri::command]
fn load_link(app: tauri::AppHandle) -> Result<Option<LinkState>, String> {
    let path = link_state_path(&app)?;

    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)
        .map_err(|error| format!("No se pudo leer la vinculación guardada: {error}"))?;

    let state = serde_json::from_str::<LinkState>(&content)
        .map_err(|error| format!("La vinculación guardada no es válida: {error}"))?;

    Ok(Some(state))
}

#[tauri::command]
fn save_link(app: tauri::AppHandle, state: LinkState) -> Result<(), String> {
    let path = link_state_path(&app)?;
    let safe_state = LinkState {
        code: state.code.trim().to_uppercase(),
        workshop_name: state.workshop_name.trim().to_string(),
        site_url: normalize_site_url(&state.site_url)?,
    };

    let content = serde_json::to_string_pretty(&safe_state)
        .map_err(|error| format!("No se pudo preparar la vinculación: {error}"))?;

    fs::write(path, content)
        .map_err(|error| format!("No se pudo guardar la vinculación: {error}"))
}

#[tauri::command]
fn clear_link(app: tauri::AppHandle) -> Result<(), String> {
    let path = link_state_path(&app)?;

    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("No se pudo borrar la vinculación: {error}"))?;
    }

    Ok(())
}

#[tauri::command]
fn open_workshop(window: WebviewWindow, url: String) -> Result<(), String> {
    let base = normalize_site_url(&url)?;
    let target = Url::parse(&format!("{base}/wp-admin/"))
        .map_err(|_| "No se pudo abrir el taller.".to_string())?;

    window
        .navigate(target)
        .map_err(|error| format!("No se pudo abrir el taller: {error}"))
}

fn main() {
    tauri::Builder::default()
        .manage(niimbot::NiimbotState::default())
        .invoke_handler(tauri::generate_handler![
            resolve_link,
            load_link,
            save_link,
            clear_link,
            open_workshop,
            niimbot::app_version,
            niimbot::print_niimbot_b1_pro,
            niimbot::disconnect_niimbot_b1_pro,
            niimbot::is_niimbot_b1_pro_connected
        ])
        .setup(|app| {
            let initialization_script =
                format!("{REMEMBER_ME_SCRIPT}\n{NATIVE_BRIDGE_SCRIPT}");

            WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("ZGT")
            .inner_size(1280.0, 840.0)
            .min_inner_size(900.0, 600.0)
            .center()
            .resizable(true)
            .initialization_script(&initialization_script)
            .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ZGT Desktop");
}
