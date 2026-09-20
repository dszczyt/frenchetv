#[cfg(target_os = "android")]
use android_activity::AndroidApp;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            // A fixed tag makes the app's own output greppable amid Fire OS
            // chatter: `adb logcat -s frenchetv`.
            .with_tag("frenchetv"),
    );

    // Without this a Rust panic on Android is completely silent. The default
    // hook writes to stderr, which the platform discards unless
    // `log.redirect-stdio` is set — and that needs root, so it is not
    // available on a retail Fire TV. The symptom is a black screen, a live
    // process and no explanation anywhere, which is not something you can
    // debug.
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        log::error!("PANIC at {location}: {msg}");
    }));

    log::info!("android_main: starting");

    let native_options = eframe::NativeOptions {
        android_app: Some(app.clone()),
        ..Default::default()
    };

    // eframe's `IntegrationInfo` no longer carries `android_app` (as of 0.30) —
    // capture it from here instead, where `android_main` already has it.
    eframe::run_native(
        "frenchetv",
        native_options,
        Box::new(move |cc| Ok(Box::new(crate::app::App::new(cc, app)))),
    )
    .expect("eframe failed");

    // Reaching here means the event loop returned rather than the process
    // being torn down, which is worth knowing about.
    log::info!("android_main: event loop exited");
}

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod credentials;
#[cfg(target_os = "android")]
mod player;
#[cfg(target_os = "android")]
mod screens;
