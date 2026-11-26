//! A web view inside a window, for the document preview. Windows uses WebView2
//! there; this is WKWebView behind the few things the preview asks of it: pages
//! and pictures served by the program under its own address, messages from the
//! page, a say over where the page may go, and scripts run in it.
use crate::imp::window::{find, mtm};
use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSAutoresizingMaskOptions;
use objc2_foundation::{NSData, NSDictionary, NSError, NSObject, NSObjectProtocol, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKScriptMessage, WKScriptMessageHandler,
    WKURLSchemeHandler, WKURLSchemeTask, WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
    WKWebViewConfiguration, WKWebsiteDataStore,
};
use std::rc::Rc;

/// What the owner of a web view decides and is told.
pub struct Hooks {
    /// The bytes and kind of what is at an address under the view's scheme, if anything is.
    pub resource: Box<dyn Fn(&str) -> Option<(Vec<u8>, &'static str)>>,
    /// A message the page sent, as JSON.
    pub message: Box<dyn Fn(&str)>,
    /// A page finished loading, or failed to.
    pub loaded: Box<dyn Fn(bool)>,
    /// Whether the page, or a frame in it, may go to an address.
    pub allow: Box<dyn Fn(&str, bool) -> bool>,
}

pub struct WebView {
    view: Retained<WKWebView>,
    _bridge: Retained<Bridge>,
}

// The page reaches its host through `window.chrome.webview`, as in WebView2.
const HOST: &str = "window.chrome={webview:{postMessage:function(m){window.webkit.messageHandlers.lw.postMessage(JSON.stringify(m))}}};\
document.addEventListener('contextmenu',function(e){e.preventDefault()},true);";

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Bridge` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Rc<Hooks>]
    struct Bridge;

    unsafe impl NSObjectProtocol for Bridge {}

    unsafe impl WKURLSchemeHandler for Bridge {
        #[unsafe(method(webView:startURLSchemeTask:))]
        fn start(&self, _: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            serve(self.ivars(), task);
        }

        #[unsafe(method(webView:stopURLSchemeTask:))]
        fn stop(&self, _: &WKWebView, _: &ProtocolObject<dyn WKURLSchemeTask>) {}
    }

    unsafe impl WKScriptMessageHandler for Bridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn said(&self, _: &WKUserContentController, message: &WKScriptMessage) {
            let body: Retained<AnyObject> = unsafe { msg_send![message, body] };
            if let Ok(text) = body.downcast::<NSString>() {
                (self.ivars().message)(&text.to_string());
            }
        }
    }

    unsafe impl WKNavigationDelegate for Bridge {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn going(&self, _: &WKWebView, action: &WKNavigationAction, decide: &DynBlock<dyn Fn(WKNavigationActionPolicy)>) {
            let request: Retained<NSURLRequest> = unsafe { msg_send![action, request] };
            let address = request.URL().and_then(|url| url.absoluteString()).map(|text| text.to_string()).unwrap_or_default();
            let frame: Option<Retained<AnyObject>> = unsafe { msg_send![action, targetFrame] };
            let whole = frame.is_none_or(|frame| unsafe { msg_send![&*frame, isMainFrame] });
            let allowed = (self.ivars().allow)(&address, whole);
            decide.call((if allowed { WKNavigationActionPolicy::Allow } else { WKNavigationActionPolicy::Cancel },));
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn arrived(&self, _: &WKWebView, _: Option<&WKNavigation>) {
            (self.ivars().loaded)(true);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn failed(&self, _: &WKWebView, _: Option<&WKNavigation>, _: &NSError) {
            (self.ivars().loaded)(false);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn never_started(&self, _: &WKWebView, _: Option<&WKNavigation>, _: &NSError) {
            (self.ivars().loaded)(false);
        }
    }
);

/// Answers one request under the view's scheme.
fn serve(hooks: &Hooks, task: &ProtocolObject<dyn WKURLSchemeTask>) {
    let request: Retained<NSURLRequest> = unsafe { msg_send![task, request] };
    let Some(url) = request.URL() else { return };
    let address = url.absoluteString().map(|text| text.to_string()).unwrap_or_default();
    let (body, kind, code) = match (hooks.resource)(&address) {
        Some((body, kind)) => (body, kind, 200isize),
        None => (Vec::new(), "text/plain", 403),
    };
    let names = [NSString::from_str("Content-Type"), NSString::from_str("Cache-Control"), NSString::from_str("X-Content-Type-Options")];
    let values = [NSString::from_str(kind), NSString::from_str("no-store"), NSString::from_str("nosniff")];
    let headers: Retained<NSDictionary<NSString, NSString>> = NSDictionary::from_slices(&[&*names[0], &*names[1], &*names[2]], &[&*values[0], &*values[1], &*values[2]]);
    let response: Option<Retained<AnyObject>> = unsafe {
        let made: objc2::rc::Allocated<AnyObject> = msg_send![objc2::class!(NSHTTPURLResponse), alloc];
        msg_send![made, initWithURL: &*url, statusCode: code, HTTPVersion: &*NSString::from_str("HTTP/1.1"), headerFields: &*headers]
    };
    let Some(response) = response else { return };
    unsafe {
        let _: () = msg_send![task, didReceiveResponse: &*response];
        let _: () = msg_send![task, didReceiveData: &*NSData::with_bytes(&body)];
        let _: () = msg_send![task, didFinish];
    }
}

impl WebView {
    /// A web view filling the window `parent`, serving `scheme` through the hooks.
    pub fn new(parent: isize, scheme: &str, hooks: Hooks) -> Option<Self> {
        let window = find(parent as _)?;
        let mtm = mtm();
        let bridge: Retained<Bridge> = unsafe { msg_send![super(Bridge::alloc(mtm).set_ivars(Rc::new(hooks))), init] };
        let setup = unsafe { WKWebViewConfiguration::new(mtm) };
        unsafe {
            setup.setURLSchemeHandler_forURLScheme(Some(ProtocolObject::from_ref(&*bridge)), &NSString::from_str(scheme));
            // Nothing the page stores outlives the program.
            setup.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
            let page = setup.userContentController();
            page.addScriptMessageHandler_name(ProtocolObject::from_ref(&*bridge), &NSString::from_str("lw"));
            let host = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(WKUserScript::alloc(mtm), &NSString::from_str(HOST), WKUserScriptInjectionTime::AtDocumentStart, true);
            page.addUserScript(&host);
        }
        let view = unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), window.view.bounds(), &setup) };
        unsafe {
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*bridge)));
            // The window behind shows until the page has drawn itself.
            let _: () = msg_send![&*view, setValue: &*objc2_foundation::NSNumber::new_bool(false), forKey: &*NSString::from_str("drawsBackground")];
        }
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        view.setHidden(true);
        unsafe { window.view.addSubview(&view) };
        // Keys go to the page once the front end has had its look at them.
        window.native_keys.set(true);
        Some(Self { view, _bridge: bridge })
    }

    pub fn load(&self, address: &str) {
        let Some(url) = (unsafe { NSURL::URLWithString(&NSString::from_str(address)) }) else { return };
        unsafe { self.view.loadRequest(&NSURLRequest::requestWithURL(&url)) };
    }

    pub fn set_visible(&self, visible: bool) {
        self.view.setHidden(!visible);
    }

    /// Runs a script in the page. `done` is given what the script's last
    /// statement came to: a truth value as `true` or `false`, text as it is, a
    /// number in digits, nothing as `null`; or None when the script failed.
    pub fn run(&self, script: &str, done: impl Fn(Option<String>) + 'static) {
        let finished = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            if !error.is_null() {
                return done(None);
            }
            let Some(result) = (unsafe { result.as_ref() }) else { return done(Some("null".into())) };
            let class: Retained<NSString> = unsafe { msg_send![result, className] };
            let text = if class.to_string().contains("Boolean") {
                let truth: bool = unsafe { msg_send![result, boolValue] };
                truth.to_string()
            } else {
                let words: Retained<NSString> = unsafe { msg_send![result, description] };
                words.to_string()
            };
            done(Some(text));
        });
        unsafe { self.view.evaluateJavaScript_completionHandler(&NSString::from_str(script), Some(&finished)) };
    }

    /// Writes a picture of the page as shown to a PNG file, a moment from now:
    /// the page is drawn by another process, so a picture of the window's own
    /// layers does not include it.
    pub fn picture(&self, path: std::path::PathBuf) {
        let finished = RcBlock::new(move |image: *mut AnyObject, _: *mut NSError| {
            let Some(image) = (unsafe { image.as_ref() }) else { return };
            let tiff: Option<Retained<NSData>> = unsafe { msg_send![image, TIFFRepresentation] };
            let Some(tiff) = tiff else { return };
            let Some(pixels) = objc2_app_kit::NSBitmapImageRep::imageRepWithData(&tiff) else { return };
            let empty = NSDictionary::new();
            if let Some(png) = unsafe { pixels.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &empty) } {
                let _: bool = unsafe { msg_send![&*png, writeToFile: &*NSString::from_str(&path.to_string_lossy()), atomically: true] };
            }
        });
        let _: () = unsafe { msg_send![&*self.view, takeSnapshotWithConfiguration: std::ptr::null::<AnyObject>(), completionHandler: &*finished] };
    }

    /// Takes the view out of its window for good.
    pub fn close(&self) {
        unsafe {
            self.view.stopLoading();
            self.view.setNavigationDelegate(None);
            self.view.configuration().userContentController().removeScriptMessageHandlerForName(&NSString::from_str("lw"));
            self.view.removeFromSuperview();
        }
    }
}
