// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use jni::objects::{JString, JObject};
use jni::sys::{jclass, jfloat, jint, jobject, JNI_ERR, jstring};
use jni::JNIEnv;
use jni::{JavaVM, NativeMethod};
use log::{error, info, debug, LevelFilter};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use android_logger::Config;
use std::fs;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

mod input;
mod renderer_bindings;

macro_rules! jni_method {
    ($name:tt, $method:tt, $signature:expr) => {{
        jni::NativeMethod {
            name: jni::strings::JNIString::from(stringify!($name)).into(),
            sig: jni::strings::JNIString::from($signature).into(),
            fn_ptr: $method as *mut c_void,
        }
    }};
}

static RENDERER_STARTED: AtomicBool = AtomicBool::new(false);

fn setup_permissions() -> Result<(), std::io::Error> {
    let path = "/data/data/io.twoyi/rootfs/dev/input";
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        let meta = fs::metadata(&path)?;
        if meta.permissions().mode() != 0o777 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777))?;
        }
    }
    Ok(())
}

fn spawn_container_process(loader: String) -> Result<(), std::io::Error> {
    info!("Starting container: {}", loader);
    let dir = "/data/data/io.twoyi/rootfs";
    let log = "/data/data/io.twoyi/log.txt";
    let out = File::create(log)?;
    let err = out.try_clone()?;
    let init_path = format!("{}/init", dir);
    match std::fs::metadata(&init_path) {
        Ok(metadata) => {
            let mut perms = metadata.permissions();
            perms.set_mode(0o755);
            if let Err(e) = std::fs::set_permissions(&init_path, perms) {
                error!("Failed to set permissions on init: {:?}", e);
            } else {
                info!("Permissions set to 755 on init");
            }
        }
        Err(e) => error!("Failed to get metadata for init: {:?}", e),
    }
    match Command::new("nice").args(["-n", "5", "./init"]).current_dir(dir).env("TYLOADER", loader).stdout(Stdio::from(out)).stderr(Stdio::from(err)).spawn() {
        Ok(_) => info!("Container process spawned"),
        Err(e) => error!("Failed to spawn container: {:?}", e),
    }
    Ok(())
}

#[no_mangle]
pub unsafe fn renderer_init(mut env: JNIEnv, _clz: jclass, surface: jobject, loader: jstring, xdpi: jfloat, ydpi: jfloat, fps: jint) {
    debug!("renderer_init started");
    let surface_obj = JObject::from_raw(surface);
    let window_ptr = ndk_sys::ANativeWindow_fromSurface(env.get_native_interface(), surface_obj.as_raw());
    let window_nonnull = match std::ptr::NonNull::new(window_ptr) {
        Some(p) => p,
        None => {
            error!("Surface null atau tidak valid");
            let _ = env.throw_new("java/lang/RuntimeException", "Surface invalid");
            return;
        }
    };
    let window = ndk::native_window::NativeWindow::from_ptr(window_nonnull);
    let (w, h) = (window.width(), window.height());
    let safe_fps = fps.clamp(15, 60);
    if RENDERER_STARTED.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        let win_ptr = window.ptr().as_ptr() as *mut c_void;
        renderer_bindings::setNativeWindow(win_ptr);
        renderer_bindings::resetSubWindow(win_ptr, 0, 0, w, h, w, h, 1.0, 0.0);
        return;
    }
    if let Err(e) = setup_permissions() {
        error!("Setup permissions failed: {:?}", e);
        let _ = env.throw_new("java/lang/RuntimeException", "Permissions setup failed");
        return;
    }
    if let Err(e) = input::start_input_system(w, h) {
        error!("Failed to start input system: {:?}", e);
        let _ = env.throw_new("java/lang/RuntimeException", "Input system failed");
        return;
    }
    thread::spawn(move || {
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -10); }
        renderer_bindings::startOpenGLRenderer(window.ptr().as_ptr() as *mut c_void, w, h, xdpi as i32, ydpi as i32, safe_fps);
    });
    if let Ok(loader_str) = env.get_string(&JString::from_raw(loader)) {
        if let Err(e) = spawn_container_process(loader_str.into()) {
            error!("Spawn container failed: {:?}", e);
        }
    }
}

#[no_mangle]
pub unsafe fn renderer_reset_window(env: JNIEnv, _clz: jclass, surface: jobject, _top: jint, _left: jint, _w: jint, _h: jint) {
    let surface_obj = JObject::from_raw(surface);
    let window = ndk_sys::ANativeWindow_fromSurface(env.get_native_interface(), surface_obj.as_raw());
    renderer_bindings::resetSubWindow(window as *mut c_void, 0, 0, _w, _h, _w, _h, 1.0, 0.0);
}

#[no_mangle]
pub unsafe fn renderer_remove_window(env: JNIEnv, _clz: jclass, surface: jobject) {
    let surface_obj = JObject::from_raw(surface);
    let window = ndk_sys::ANativeWindow_fromSurface(env.get_native_interface(), surface_obj.as_raw());
    renderer_bindings::removeSubWindow(window as *mut c_void);
}

#[no_mangle]
pub unsafe fn handle_touch(mut env: JNIEnv, _clz: jclass, event: jobject) {
    if event.is_null() { return; }
    let event_obj = JObject::from_raw(event);
    if let Ok(ptr_field) = env.get_field(&event_obj, "mNativePtr", "J") {
        if let Ok(ptr_val) = ptr_field.j() {
            if let Some(nonptr) = std::ptr::NonNull::new(ptr_val as *mut ndk_sys::AInputEvent) {
                let ev = ndk::event::MotionEvent::from_ptr(nonptr);
                if let Err(e) = input::handle_touch(ev) {
                    error!("Handle touch failed: {:?}", e);
                }
            }
        }
    }
}

#[no_mangle]
pub fn send_key_code(_env: JNIEnv, _clz: jclass, keycode: jint) {
    if let Err(e) = input::send_key_code(keycode) {
        error!("Send key code failed: {:?}", e);
    }
}

unsafe fn register_natives(jvm: &JavaVM, class_name: &str, methods: &[NativeMethod]) -> jint {
    let mut env = jvm.get_env().unwrap();
    let version: jint = env.get_version().unwrap().into();
    let clazz = match env.find_class(class_name) {
        Ok(c) => c,
        Err(e) => {
            error!("Class not found: {:?}", e);
            return JNI_ERR;
        }
    };
    let result = env.register_native_methods(&clazz, methods);
    if result.is_ok() {
        debug!("Natives registered");
        version
    } else {
        error!("Register failed");
        JNI_ERR
    }
}

#[no_mangle]
#[allow(non_snake_case)]
unsafe fn JNI_OnLoad(jvm: JavaVM, _reserved: *mut c_void) -> jint {
    android_logger::init_once(Config::default().with_max_level(LevelFilter::Info).with_tag("CLIENT_EGL"));
    let class_name = "io/twoyi/Renderer";
    let methods = [
        jni_method!(init, renderer_init, "(Landroid/view/Surface;Ljava/lang/String;FFI)V"),
        jni_method!(resetWindow, renderer_reset_window, "(Landroid/view/Surface;IIII)V"),
        jni_method!(removeWindow, renderer_remove_window, "(Landroid/view/Surface;)V"),
        jni_method!(handleTouch, handle_touch, "(Landroid/view/MotionEvent;)V"),
        jni_method!(sendKeycode, send_key_code, "(I)V"),
    ];
    register_natives(&jvm, class_name, &methods)
}