use android_activity::AndroidApp;
use frenchetv_core::StreamUrl;
use jni::objects::JValue;

/// Launch PlayerActivity with the given stream URL.
/// ExoPlayer inside PlayerActivity handles playback and Widevine DRM.
pub fn launch_player(app: &AndroidApp, stream: &StreamUrl) {
    let vm = match unsafe { jni::JavaVM::from_raw(app.vm_as_ptr() as *mut _) } {
        Ok(vm) => vm,
        Err(e) => {
            log::error!("JNI: failed to get JavaVM: {}", e);
            return;
        }
    };
    let mut env = match vm.attach_current_thread() {
        Ok(e) => e,
        Err(e) => {
            log::error!("JNI: attach_current_thread failed: {}", e);
            return;
        }
    };
    let activity = unsafe { jni::objects::JObject::from_raw(app.activity_as_ptr() as *mut _) };

    if let Err(e) = do_launch_player(&mut env, &activity, stream) {
        log::error!("JNI: launch_player failed: {:?}", e);
    }
}

fn do_launch_player(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
    stream: &StreamUrl,
) -> jni::errors::Result<()> {
    let intent_class = env.find_class("android/content/Intent")?;
    let player_class = env.find_class("com/frenchetv/PlayerActivity")?;
    let intent = env.new_object(
        &intent_class,
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[
            JValue::Object(activity),
            JValue::Object(&player_class.into()),
        ],
    )?;

    // putExtra("stream_url", url)
    let k = env.new_string("stream_url")?;
    let v = env.new_string(stream.url.as_str())?;
    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&k.into()), JValue::Object(&v.into())],
    )?;

    // putExtra("auth_header", value) if present
    if let Some(auth) = &stream.auth_header {
        let k2 = env.new_string("auth_header")?;
        let v2 = env.new_string(auth)?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&k2.into()), JValue::Object(&v2.into())],
        )?;
    }

    // putExtra("license_url", value) for DRM
    if let Some(prot) = &stream.protection {
        let k3 = env.new_string("license_url")?;
        let v3 = env.new_string(&prot.la_url)?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&k3.into()), JValue::Object(&v3.into())],
        )?;
    }

    // activity.startActivity(intent)
    env.call_method(
        activity,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[JValue::Object(&intent)],
    )?;

    Ok(())
}
