// Extra window, plugin and config-profile options (fraxy/settings-options)
use std::fs;
use std::path::PathBuf;

use crate::config::{Config, get_config, read_config_file, write_config_file};
use crate::log;
use crate::util::paths::{get_config_dir, get_plugin_dir};

// Built from shelter-plugins and copied here by scripts/bundle-plugins.sh
const BUNDLED_PLUGINS: &[(&str, &str, &str)] = &[
  (
    "Dorion Settings",
    include_str!("../../injection/bundled/dorion-settings/plugin.js"),
    include_str!("../../injection/bundled/dorion-settings/plugin.json"),
  ),
  (
    "Dorion Helpers",
    include_str!("../../injection/bundled/dorion-helpers/plugin.js"),
    include_str!("../../injection/bundled/dorion-helpers/plugin.json"),
  ),
  (
    "Inline CSS",
    include_str!("../../injection/bundled/inline-css/plugin.js"),
    include_str!("../../injection/bundled/inline-css/plugin.json"),
  ),
];

#[tauri::command]
pub fn get_bundled_plugins() -> Vec<(String, String, String)> {
  BUNDLED_PLUGINS
    .iter()
    .map(|(n, js, m)| (n.to_string(), js.to_string(), m.to_string()))
    .collect()
}

/// Apply the window options saved in the config. Called once the window exists.
pub fn apply_window_options(window: &tauri::WebviewWindow<crate::Runtime>) {
  let config = get_config();

  if config.always_on_top.unwrap_or(false) {
    window.set_always_on_top(true).unwrap_or_default();
  }

  reapply_opacity(window);
}

/// tao rewrites the extended window style on decoration changes, which drops the layered style.
pub fn reapply_opacity(window: &tauri::WebviewWindow<crate::Runtime>) {
  let opacity = get_config().window_opacity.unwrap_or(100);
  if opacity < 100 {
    set_window_opacity(window.clone(), opacity);
  }
}

#[tauri::command]
pub fn set_always_on_top(win: tauri::WebviewWindow<crate::Runtime>, value: bool) {
  win
    .set_always_on_top(value)
    .unwrap_or_else(|e| log!("Failed to set always on top: {:?}", e));
}

#[tauri::command]
pub fn set_window_opacity(win: tauri::WebviewWindow<crate::Runtime>, value: u8) {
  // Too low and the window becomes impossible to find again
  let value = value.clamp(20, 100);

  #[cfg(target_os = "windows")]
  {
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::UI::WindowsAndMessaging::{
      GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SetLayeredWindowAttributes, SetWindowLongPtrW,
      WS_EX_LAYERED,
    };

    let Ok(hwnd) = win.hwnd() else {
      log!("Failed to get window handle for opacity");
      return;
    };
    // HWND is not Send, carry the raw pointer value to the main thread
    let raw = hwnd.0 as isize;

    // Queued behind any pending tao style change, so ours is applied last
    let res = win.run_on_main_thread(move || unsafe {
      let hwnd = HWND(raw as *mut core::ffi::c_void);
      let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
      let layered = WS_EX_LAYERED.0 as isize;

      if value >= 100 {
        // Drop the layered style entirely so 100% is exactly the original window
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !layered);
        return;
      }

      SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | layered);
      let alpha = (value as u32 * 255 / 100) as u8;
      if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) {
        log!("Failed to set window opacity: {:?}", e);
      }
    });

    if let Err(e) = res {
      log!("Failed to schedule window opacity: {:?}", e);
    }
  }

  #[cfg(not(target_os = "windows"))]
  log!("Window opacity is only supported on Windows (requested {value}%)");
}

/// Ask WebView2 to trim memory while the window is hidden or minimized.
#[cfg(target_os = "windows")]
pub fn update_memory_target(win: &tauri::Window<crate::Runtime>) {
  use tauri::Manager;
  use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    ICoreWebView2_19,
  };
  use windows::core::Interface;

  if !get_config().low_memory_when_hidden.unwrap_or(false) {
    return;
  }

  let hidden = !win.is_visible().unwrap_or(true) || win.is_minimized().unwrap_or(false);

  // Resized fires continuously while dragging, only act when the state flips
  static LAST_HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
  if LAST_HIDDEN.swap(hidden, std::sync::atomic::Ordering::Relaxed) == hidden {
    return;
  }

  let Some(webview) = win.get_webview_window("main") else {
    return;
  };

  webview
    .with_webview(move |wv| unsafe {
      let level = if hidden {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
      } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
      };

      let res = wv
        .controller()
        .CoreWebView2()
        .and_then(|c| c.cast::<ICoreWebView2_19>())
        .and_then(|c| c.SetMemoryUsageTargetLevel(level));

      match res {
        Ok(_) => log!(
          "WebView2 memory target set to {}",
          if hidden { "low" } else { "normal" }
        ),
        Err(e) => log!("Failed to set WebView2 memory target: {:?}", e),
      }
    })
    .unwrap_or_default();
}

fn valid_name(name: &str) -> bool {
  !name.is_empty()
    && name.len() <= 64
    && name
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' || c == '.')
    && !name.starts_with('.')
}

#[tauri::command]
pub fn import_plugin(name: String, contents: String) -> Result<(), String> {
  let name = name.trim().to_string();
  if !valid_name(&name) || !name.ends_with(".js") {
    return Err(
      "Invalid plugin file name (letters, digits, - _ . and space, ending in .js)".into(),
    );
  }

  let path = get_plugin_dir().join(&name);
  if path.exists() {
    return Err(format!("A plugin named {name} already exists"));
  }

  fs::write(&path, contents).map_err(|e| format!("Failed to write plugin: {e}"))?;
  log!("Imported plugin {}", path.display());

  Ok(())
}

fn snapshot_dir() -> PathBuf {
  let dir = get_config_dir().join("config_profiles");
  fs::create_dir_all(&dir).unwrap_or_default();
  dir
}

fn snapshot_path(name: &str) -> Result<PathBuf, String> {
  if !valid_name(name) {
    return Err("Invalid profile name (letters, digits, - _ . and space)".into());
  }
  Ok(snapshot_dir().join(format!("{name}.json")))
}

#[tauri::command]
pub fn list_config_snapshots() -> Vec<String> {
  let mut names: Vec<String> = fs::read_dir(snapshot_dir())
    .map(|entries| {
      entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter_map(|n| n.strip_suffix(".json").map(|s| s.to_string()))
        .collect()
    })
    .unwrap_or_default();
  names.sort();
  names
}

#[tauri::command]
pub fn save_config_snapshot(name: String) -> Result<(), String> {
  let path = snapshot_path(name.trim())?;
  fs::write(path, read_config_file()).map_err(|e| format!("Failed to save profile: {e}"))
}

#[tauri::command]
pub fn load_config_snapshot(name: String) -> Result<(), String> {
  let path = snapshot_path(name.trim())?;
  let contents = fs::read_to_string(path).map_err(|e| format!("Failed to read profile: {e}"))?;
  import_config(contents)
}

/// Write a full config, refusing invalid input. write_config_file would silently reset to defaults.
#[tauri::command]
pub fn import_config(contents: String) -> Result<(), String> {
  validate_config(&contents)?;
  write_config_file(contents);
  Ok(())
}

fn validate_config(contents: &str) -> Result<(), String> {
  let is_object = serde_json::from_str::<serde_json::Value>(contents)
    .map(|v| v.is_object())
    .unwrap_or(false);

  if !is_object {
    return Err("Not a valid config file".into());
  }

  serde_json::from_str::<Config>(contents).map_err(|e| format!("Not a valid config file: {e}"))?;
  Ok(())
}

#[tauri::command]
pub fn delete_config_snapshot(name: String) -> Result<(), String> {
  let path = snapshot_path(name.trim())?;
  fs::remove_file(path).map_err(|e| format!("Failed to delete profile: {e}"))
}

/// Write the current config or the Dorion plugin list into the Downloads folder, returns the path.
#[tauri::command]
pub fn export_to_downloads(kind: String) -> Result<String, String> {
  let contents = match kind.as_str() {
    "config" => read_config_file(),
    "plugins" => serde_json::to_string_pretty(&crate::injection::plugin::get_plugin_list())
      .map_err(|e| format!("Failed to serialize plugin list: {e}"))?,
    _ => return Err(format!("Unknown export kind: {kind}")),
  };

  let dir = dirs::download_dir().ok_or("No Downloads folder found")?;
  let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M");
  let path = dir.join(format!("dorion-{kind}-{stamp}.json"));

  fs::write(&path, contents).map_err(|e| format!("Failed to write export: {e}"))?;
  Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
  use super::{valid_name, validate_config};

  #[test]
  fn configs() {
    assert!(validate_config("{}").is_ok());
    assert!(validate_config(r#"{"zoom":"1.25","always_on_top":true,"unknown_key":1}"#).is_ok());
    assert!(validate_config(r#"{"zoom":1.25}"#).is_err());
    assert!(validate_config(r#"{"window_opacity":300}"#).is_err());
    assert!(validate_config("[]").is_err());
    assert!(validate_config("null").is_err());
    assert!(validate_config("not json").is_err());
  }

  #[test]
  fn names() {
    assert!(valid_name("gaming"));
    assert!(valid_name("my plugin.js"));
    assert!(!valid_name(""));
    assert!(!valid_name("../evil"));
    assert!(!valid_name("a/b"));
    assert!(!valid_name("a\\b"));
    assert!(!valid_name(".hidden"));
    assert!(!valid_name("C:evil"));
    assert!(!valid_name(&"x".repeat(65)));
  }
}
