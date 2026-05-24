#[cfg(target_os = "android")]
use android_activity::AndroidApp;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info),
    );

    let native_options = eframe::NativeOptions {
        android_app: Some(app),
        ..Default::default()
    };

    eframe::run_native(
        "FrenchTV",
        native_options,
        Box::new(|cc| Box::new(crate::app::App::new(cc))),
    )
    .expect("eframe failed");
}

#[cfg(target_os = "android")]
mod app;
#[cfg(target_os = "android")]
mod screens;
#[cfg(target_os = "android")]
mod player;
