#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "linux")]
            enable_media_permissions(app);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// WebKitGTK denies getUserMedia by default: the `enable-media-stream` setting
/// is off and the permission-request signal has no handler (wry has no hook
/// for it on Linux — it only handles media permissions on Android/Windows).
/// Without this, joining a voice channel fails with "The request is not
/// allowed by the user agent or the platform". Turn the setting on and
/// auto-grant permission requests so the voice call can access the mic.
#[cfg(target_os = "linux")]
fn enable_media_permissions(app: &tauri::App) {
    use tauri::Manager;
    use webkit2gtk::{PermissionRequestExt, SettingsExt, WebViewExt};
    if let Some(window) = app.get_webview_window("main") {
        let result = window.as_ref().with_webview(|platform| {
            let inner = platform.inner(); // webkit2gtk::WebView
            if let Some(settings) = inner.settings() {
                settings.set_enable_media_stream(true);
                // WebRTC is off by default in some WebKitGTK builds — without
                // this, `RTCPeerConnection` is undefined and mesh voice dies
                // with a ReferenceError the moment a second peer joins.
                settings.set_enable_webrtc(true);
            }
            inner.connect_permission_request(|_webview, request| {
                request.allow();
                true
            });
        });
        if let Err(err) = result {
            eprintln!("lumen: could not enable WebKitGTK media permissions: {err}");
        }
    }
}
