//! "Remember me" credential storage, bridged to `CredentialStore.kt`.
//!
//! The Kotlin side seals the values with EncryptedSharedPreferences, whose key
//! lives in the AndroidKeyStore. Nothing sensitive is written from Rust — this
//! module only marshals strings across JNI.
//!
//! Every call degrades to "nothing stored" rather than failing: a missing
//! keystore entry or an API-21/22 device must not break login.

use android_activity::AndroidApp;
use jni::objects::{JObject, JString, JValue};

pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Run `f` with an attached JNI env and the activity object.
fn with_env<T>(
    app: &AndroidApp,
    what: &str,
    f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<T>,
) -> Option<T> {
    let vm = match unsafe { jni::JavaVM::from_raw(app.vm_as_ptr() as *mut _) } {
        Ok(vm) => vm,
        Err(e) => {
            log::error!("JNI: failed to get JavaVM: {}", e);
            return None;
        }
    };
    let mut env = match vm.attach_current_thread() {
        Ok(e) => e,
        Err(e) => {
            log::error!("JNI: attach_current_thread failed: {}", e);
            return None;
        }
    };
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as *mut _) };
    match f(&mut env, &activity) {
        Ok(v) => Some(v),
        Err(e) => {
            log::error!("JNI: {} failed: {:?}", what, e);
            // A pending Java exception would poison every later JNI call.
            // Describe first — it prints the Java stack trace to logcat, which
            // is the only way to see what actually threw.
            let _ = env.exception_describe();
            let _ = env.exception_clear();
            None
        }
    }
}

const CLASS: &str = "com.frenchetv.CredentialStore";

/// Resolve an application class by name.
///
/// `JNIEnv::find_class` cannot be used here: on a thread attached with
/// `AttachCurrentThread` it resolves against the *system* class loader, whose
/// DexPathList covers only `/system/lib`, so every class from this APK comes
/// back as ClassNotFoundException. Going through the activity's own class
/// loader works from any thread.
fn load_app_class<'a>(
    env: &mut jni::JNIEnv<'a>,
    activity: &JObject,
    name: &str,
) -> jni::errors::Result<jni::objects::JClass<'a>> {
    let activity_class = env.call_method(activity, "getClass", "()Ljava/lang/Class;", &[])?;
    let loader = env.call_method(
        activity_class.l()?,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    )?;
    let j_name = env.new_string(name)?;
    let class = env.call_method(
        loader.l()?,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&j_name)],
    )?;
    Ok(class.l()?.into())
}

/// True when the device can store credentials at all (API 23+).
pub fn is_available(app: &AndroidApp) -> bool {
    with_env(app, "isAvailable", |env, activity| {
        let class = load_app_class(env, activity, CLASS)?;
        env.call_static_method(class, "isAvailable", "()Z", &[])?
            .z()
    })
    .unwrap_or(false)
}

pub fn save(app: &AndroidApp, username: &str, password: &str) {
    with_env(app, "save", |env, activity| {
        let class = load_app_class(env, activity, CLASS)?;
        let j_user = env.new_string(username)?;
        let j_pass = env.new_string(password)?;
        env.call_static_method(
            class,
            "save",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)V",
            &[
                JValue::Object(activity),
                JValue::Object(&j_user),
                JValue::Object(&j_pass),
            ],
        )?;
        Ok(())
    });
}

pub fn load(app: &AndroidApp) -> Option<Credentials> {
    let blob: String = with_env(app, "load", |env, activity| {
        let class = load_app_class(env, activity, CLASS)?;
        let out = env.call_static_method(
            class,
            "load",
            "(Landroid/content/Context;)Ljava/lang/String;",
            &[JValue::Object(activity)],
        )?;
        let s: JString = out.l()?.into();
        let owned: String = env.get_string(&s)?.into();
        Ok(owned)
    })?;

    // "username\npassword"; empty means nothing stored.
    let (username, password) = blob.split_once('\n')?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some(Credentials {
        username: username.to_string(),
        password: password.to_string(),
    })
}

pub fn clear(app: &AndroidApp) {
    with_env(app, "clear", |env, activity| {
        let class = load_app_class(env, activity, CLASS)?;
        env.call_static_method(
            class,
            "clear",
            "(Landroid/content/Context;)V",
            &[JValue::Object(activity)],
        )?;
        Ok(())
    });
}
