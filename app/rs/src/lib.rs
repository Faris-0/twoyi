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
    ( $name: tt, $method:tt, $signature:expr ) => {{
        jni::NativeMethod {
            name: jni::strings::JNIString::from(stringify!($name)).into(),
            sig: jni::strings::JNIString::from($signature).into(),
            fn_ptr: $method as *mut c_void,
        }
    }};
}

static RENDERER_STARTED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub unsafe fn renderer_init(
    mut env: JNIEnv,
    _clz: jclass,
    surface: jobject,
    loader: jstring,
    xdpi: jfloat,
    ydpi: jfloat,
    fps: jint,
) {
    debug!("renderer_init");

    let surface_obj = JObject::from_raw(surface);
    let window_ptr = ndk_sys::ANativeWindow_fromSurface(env.get_native_interface(), surface_obj.as_raw());

    let window_nonnull = match std::ptr::NonNull::new(window_ptr) {
        Some(x) => x,
        None => {
            error!("ANativeWindow_fromSurface was null!");
            return;
        }
    };

    let window = ndk::native_window::NativeWindow::from_ptr(window_nonnull);
    let width = window.width();
    let height = window.height();

    // OPTIMASI: Batasi ke 30 FPS untuk stabilitas GPU di Android 14
    let safe_fps = if fps > 30 { 30 } else { fps };

    info!(
        "renderer_init width: {}, height: {}, target_fps: {}, safe_fps: {}",
        width, height, fps, safe_fps
    );

    if RENDERER_STARTED.compare_exchange(false, true,
        Ordering::Acquire, Ordering::Relaxed).is_err() {
        let win = window.ptr().as_ptr() as *mut c_void;
        renderer_bindings::setNativeWindow(win);
        renderer_bindings::resetSubWindow(win, 0, 0, width, height, width, height, 1.0, 0.0);
    } else {
        // Izin file secara native
        let rootfs = "/data/data/io.twoyi/rootfs";
        let dev_input_path = format!("{}/dev/input", rootfs);

        if let Ok(entries) = fs::read_dir(&dev_input_path) {
            for entry in entries.flatten() {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o777));
            }
        }

        input::start_input_system(width, height);

        thread::spawn(move || {
            // Memberikan prioritas tinggi pada thread renderer
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -10); }

            let win = window.ptr().as_ptr() as *mut c_void;
            renderer_bindings::startOpenGLRenderer(
                win,
                width,
                height,
                xdpi as i32,
                ydpi as i32,
                safe_fps,
            );
        });

        let loader_obj = JObject::from_raw(loader);
        let loader_jstr = JString::from(loader_obj);
        let loader_path: String = env.get_string(&loader_jstr).unwrap().into();

        let working_dir = "/data/data/io.twoyi/rootfs";
        let log_path = "/data/data/io.twoyi/log.txt";

        if let Ok(outputs) = File::create(log_path) {
            let errors = outputs.try_clone().unwrap();

            // Gunakan 'nice' untuk menjalankan container
            let _ = Command::new("nice")
                .arg("-n")
                .arg("5")
                .arg("./init")
                .current_dir(working_dir)
                .env("TYLOADER", loader_path)
                .stdout(Stdio::from(outputs))
                .stderr(Stdio::from(errors))
                .spawn();
        }
    }
}

#[no_mangle]
pub unsafe fn renderer_reset_window(
    env: JNIEnv,
    _clz: jclass,
    surface: jobject,
    _top: jint,
    _left: jint,
    _width: jint,
    _height: jint,
) {
    let surface_obj = JObject::from_raw(surface);
    let window = ndk_sys::ANativeWindow_fromSurface(env.get_native_interface(), surface_obj.as_raw());
    renderer_bindings::resetSubWindow(window as *mut c_void, 0, 0, _width, _height, _width, _height, 1.0, 0.0);
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
                input::handle_touch(ev);
            }
        }
    }
}

#[no_mangle]
pub fn send_key_code(_env: JNIEnv, _clz: jclass, keycode: jint) {
    input::send_key_code(keycode);
}

unsafe fn register_natives(jvm: &JavaVM, class_name: &str, methods: &[NativeMethod]) -> jint {
    let mut env = jvm.get_env().unwrap();
    let jni_version = env.get_version().unwrap();
    let version: jint = jni_version.into();

    let clazz = match env.find_class(class_name) {
        Ok(clazz) => clazz,
        Err(e) => {
            error!("java class not found : {:?}", e);
            return JNI_ERR;
        }
    };

    let result = env.register_native_methods(&clazz, methods);

    if result.is_ok() {
        debug!("register_natives : succeed");
        version
    } else {
        error!("register_natives : failed ");
        JNI_ERR
    }
}

#[no_mangle]
#[allow(non_snake_case)]
unsafe fn JNI_OnLoad(jvm: JavaVM, _reserved: *mut c_void) -> jint {
    android_logger::init_once(
        Config::default()
            .with_max_level(LevelFilter::Warn)
            .with_tag("CLIENT_EGL"),
    );

    let class_name = "io/twoyi/Renderer";
    let jni_methods = [
        jni_method!(init, renderer_init, "(Landroid/view/Surface;Ljava/lang/String;FFI)V"),
        jni_method!(resetWindow, renderer_reset_window, "(Landroid/view/Surface;IIII)V"),
        jni_method!(removeWindow, renderer_remove_window, "(Landroid/view/Surface;)V"),
        jni_method!(handleTouch, handle_touch, "(Landroid/view/MotionEvent;)V"),
        jni_method!(sendKeycode, send_key_code, "(I)V"),
    ];

    register_natives(&jvm, class_name, jni_methods.as_ref())
}