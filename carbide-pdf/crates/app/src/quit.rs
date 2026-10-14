//! Quitting from outside the window.
//!
//! On macOS, Dock > Quit, logging out, restarting or shutting down, an AppleScript `quit` and the
//! application menu's Quit all end the app through `-[NSApplication terminate:]`. winit's app
//! delegate has no `applicationShouldTerminate:`, so AppKit goes ahead without asking, and winit
//! then closes the window without a close request: the question about unsaved changes would never
//! be asked. So at launch the delegate class winit registers is given that method. While a
//! document has unsaved changes it cancels the termination (logging out or shutting down then
//! stops, and macOS says this app stopped it), brings the window back, and has the window put up
//! the quit question; once that is answered the window closes, and the app with it. With nothing
//! unsaved the termination goes ahead, and the documents are let go in `on_exit`.

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;

use eframe::egui;

/// Whether some open document has unsaved changes, as of the last frame.
static UNSAVED: AtomicBool = AtomicBool::new(false);
/// Set once quitting was agreed to: a termination asked for after that goes ahead.
static CONFIRMED: AtomicBool = AtomicBool::new(false);
/// Set when the system asked to quit and was told to wait for the quit question.
static REQUESTED: AtomicBool = AtomicBool::new(false);
/// Woken up so the next frame puts up the quit question.
#[cfg(target_os = "macos")]
static CONTEXT: OnceLock<egui::Context> = OnceLock::new();

/// Whether the app may end now when the system asks it to: when nothing is unsaved, or once the
/// user has answered the quit question.
pub fn may_terminate(unsaved: bool, confirmed: bool) -> bool {
    confirmed || !unsaved
}

/// Records, every frame, whether some document has unsaved changes.
pub fn set_unsaved(unsaved: bool) {
    UNSAVED.store(unsaved, Ordering::SeqCst);
}

/// Records that quitting was agreed to.
pub fn set_confirmed() {
    CONFIRMED.store(true, Ordering::SeqCst);
}

/// True, once, after the system asked to quit and was told to wait.
pub fn take_request() -> bool {
    REQUESTED.swap(false, Ordering::SeqCst)
}

/// Gives winit's app delegate its `applicationShouldTerminate:`. Returns false when that cannot be
/// done (no delegate, or one that already answers it), in which case the caller keeps the
/// application menu's Quit on the window's close button at least.
#[cfg(target_os = "macos")]
pub fn install(ctx: &egui::Context) -> bool {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{sel, MainThreadMarker};
    use objc2_app_kit::NSApplication;

    type ShouldTerminate = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize;

    let _ = CONTEXT.set(ctx.clone());
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = app.delegate() else { return false };
    let object: &AnyObject = (*delegate).as_ref();
    let class: *const AnyClass = object.class();
    // SAFETY: the function has the signature the selector's type encoding names ("Q@:@": an
    // NSUInteger reply, the receiver, the selector, the sender), and is never unloaded.
    // class_addMethod adds nothing when the class already has the method.
    let added = unsafe {
        let imp: Imp = std::mem::transmute::<ShouldTerminate, Imp>(should_terminate);
        objc2::ffi::class_addMethod(class as *mut AnyClass, sel!(applicationShouldTerminate:), imp, c"Q@:@".as_ptr())
    };
    if !added.as_bool() {
        return false;
    }
    // AppKit notes which delegate methods there are when its delegate is set, so it is set again.
    app.setDelegate(Some(&delegate));
    true
}

#[cfg(not(target_os = "macos"))]
pub fn install(_ctx: &egui::Context) -> bool {
    false
}

/// `-[NSApplicationDelegate applicationShouldTerminate:]`.
#[cfg(target_os = "macos")]
extern "C-unwind" fn should_terminate(_this: *mut objc2::runtime::AnyObject, _cmd: objc2::runtime::Sel, _sender: *mut objc2::runtime::AnyObject) -> usize {
    const TERMINATE_CANCEL: usize = 0;
    const TERMINATE_NOW: usize = 1;
    if may_terminate(UNSAVED.load(Ordering::SeqCst), CONFIRMED.load(Ordering::SeqCst)) {
        return TERMINATE_NOW;
    }
    REQUESTED.store(true, Ordering::SeqCst);
    show_windows();
    if let Some(ctx) = CONTEXT.get() {
        ctx.request_repaint();
    }
    TERMINATE_CANCEL
}

/// Brings the app forward with its windows out of the Dock, so the quit question is seen.
#[cfg(target_os = "macos")]
fn show_windows() {
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{msg_send, MainThreadMarker};
    use objc2_app_kit::NSApplication;

    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: standard NSApplication, NSArray and NSWindow messages, sent on the main thread with
    // the argument and return types they are declared with.
    unsafe {
        let windows: *mut AnyObject = msg_send![&*app, windows];
        let count: usize = if windows.is_null() { 0 } else { msg_send![windows, count] };
        for index in 0..count {
            let window: *mut AnyObject = msg_send![windows, objectAtIndex: index];
            let minimized: Bool = msg_send![window, isMiniaturized];
            if minimized.as_bool() {
                let _: () = msg_send![window, deminiaturize: std::ptr::null_mut::<AnyObject>()];
            }
        }
        let _: () = msg_send![&*app, activateIgnoringOtherApps: Bool::YES];
    }
}
