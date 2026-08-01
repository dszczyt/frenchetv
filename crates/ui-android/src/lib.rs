#[cfg(target_os = "android")]
use android_activity::AndroidApp;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    let native_options = eframe::NativeOptions {
        android_app: Some(app.clone()),
        ..Default::default()
    };

    // eframe's `IntegrationInfo` no longer carries `android_app` (as of 0.30) —
    // capture it from here instead, where `android_main` already has it.
    eframe::run_native(
        "FrenchTV",
        native_options,
        Box::new(move |cc| Ok(Box::new(crate::app::App::new(cc, app)))),
    )
    .expect("eframe failed");
}

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod player;
#[cfg(target_os = "android")]
mod screens;
