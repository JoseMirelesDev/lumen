pub mod voice;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager;
            let config = app.config().app.windows.first().expect("missing main window");
            let window = tauri::WebviewWindowBuilder::from_config(app.handle(), config)?.build()?;
            #[cfg(target_os = "linux")]
            enable_media_permissions(&window);
            app.manage(voice::VoiceClient::new(app.handle().clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            voice_join,
            voice_leave,
            voice_set_muted,
            voice_set_deafened,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
async fn voice_join(
    client: tauri::State<'_, voice::VoiceClient>,
    args: voice::VoiceJoinArgs,
) -> Result<(), String> {
    client.join(args).await
}

#[tauri::command]
async fn voice_leave(client: tauri::State<'_, voice::VoiceClient>) -> Result<(), String> {
    client.leave().await;
    Ok(())
}

#[tauri::command]
async fn voice_set_muted(
    client: tauri::State<'_, voice::VoiceClient>,
    muted: bool,
) -> Result<(), String> {
    client.set_muted(muted).await;
    Ok(())
}

#[tauri::command]
async fn voice_set_deafened(
    client: tauri::State<'_, voice::VoiceClient>,
    deafened: bool,
) -> Result<(), String> {
    client.set_deafened(deafened).await;
    Ok(())
}

/// WebKitGTK denies getUserMedia by default: the `enable-media-stream` setting
/// is off and the permission-request signal has no handler (wry has no hook
/// for it on Linux — it only handles media permissions on Android/Windows).
/// Without this, joining a voice channel fails with "The request is not
/// allowed by the user agent or the platform". Turn the setting on and
/// auto-grant permission requests so the voice call can access the mic.
#[cfg(target_os = "linux")]
fn enable_media_permissions(window: &tauri::WebviewWindow) {
    use webkit2gtk::{PermissionRequestExt, SettingsExt, WebViewExt};
    let result = window.as_ref().with_webview(|platform| {
        let inner = platform.inner(); // webkit2gtk::WebView
        if let Some(settings) = inner.settings() {
            settings.set_enable_media_stream(true);
            settings.set_enable_webrtc(true);
            settings.set_media_playback_requires_user_gesture(false);
        }
        inner.connect_permission_request(|_webview, request| {
            request.allow();
            true
        });
        // Tauri starts loading the initial URL while building its WebView.
        // Reload after enabling WebRTC so this document sees the new setting.
        inner.reload();
    });
    if let Err(err) = result {
        eprintln!("lumen: could not enable WebKitGTK media permissions: {err}");
    }
}
