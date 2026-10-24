//! macOS integration: notifications and launch at login (both need the .app bundle, so they
//! no-op under `cargo run`), plus the menu bar item and Dock badge.

use block2::{DynBlock, RcBlock};
use futures::channel::mpsc::UnboundedSender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy, NSCellImagePosition, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSBundle, NSData, NSError, NSObject, NSObjectProtocol, NSSize,
    NSString,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

define_class!(
    #[unsafe(super = NSObject)]
    #[name = "OssdeckNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        /// The request identifier is the thread's URL.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &DynBlock<dyn Fn()>,
        ) {
            let url = response.notification().request().identifier().to_string();
            let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
            done.call(());
        }
    }
);

fn bundled() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

pub fn init_notifications() {
    if !bundled() {
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // The delegate property is weak; keep ours alive for the process lifetime.
    std::mem::forget(delegate);
    let handler = RcBlock::new(|_granted: Bool, _err: *mut NSError| {});
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &handler,
    );
}

pub fn notify(title: &str, subtitle: &str, body: &str, url: &str) {
    if !bundled() {
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setSubtitle(&NSString::from_str(subtitle));
    content.setBody(&NSString::from_str(body));
    content.setThreadIdentifier(&NSString::from_str(title));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(url),
        &content,
        None,
    );
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, None);
}

pub fn login_enabled() -> bool {
    bundled() && unsafe { SMAppService::mainAppService().status() } == SMAppServiceStatus::Enabled
}

pub fn set_login(on: bool) -> Result<(), String> {
    if !bundled() {
        return Err("launch at login needs the .app bundle".into());
    }
    let service = unsafe { SMAppService::mainAppService() };
    let r = unsafe {
        if on {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    r.map_err(|e| e.localizedDescription().to_string())
}

/// What the menu bar item asks the app to do.
pub enum TrayCmd {
    Open,
    Sync,
    Quit,
    Thread(String),
}

struct TargetIvars {
    tx: UnboundedSender<TrayCmd>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "OssdeckTrayTarget"]
    #[ivars = TargetIvars]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(open:))]
        fn open(&self, _sender: Option<&AnyObject>) {
            self.send(TrayCmd::Open);
        }

        #[unsafe(method(sync:))]
        fn sync(&self, _sender: Option<&AnyObject>) {
            self.send(TrayCmd::Sync);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            self.send(TrayCmd::Quit);
        }

        /// Thread items carry the thread id as their represented object.
        #[unsafe(method(thread:))]
        fn thread(&self, sender: Option<&NSMenuItem>) {
            if let Some(id) = sender.and_then(|s| s.representedObject()).and_then(|o| o.downcast::<NSString>().ok()) {
                self.send(TrayCmd::Thread(id.to_string()));
            }
        }
    }
);

impl Target {
    fn send(&self, cmd: TrayCmd) {
        let _ = self.ivars().tx.unbounded_send(cmd);
    }
}

const MENUBAR_PNG: &[u8] = include_bytes!("../assets/icon/menubar.png");
const MENUBAR_UNREAD_LIGHT_PNG: &[u8] = include_bytes!("../assets/icon/menubar-unread-light.png");
const MENUBAR_UNREAD_DARK_PNG: &[u8] = include_bytes!("../assets/icon/menubar-unread-dark.png");

/// Template images follow the menu bar colour automatically but can't carry colour, so the
/// blue-dot variants come in a light and a dark version.
fn menubar_image(png: &[u8], template: bool) -> Option<Retained<NSImage>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png))?;
    image.setSize(NSSize::new(18., 18.));
    image.setTemplate(template);
    Some(image)
}

/// Menu bar item with the unread count and a menu of the latest unread threads; also sets the Dock badge.
pub struct Tray {
    item: Retained<NSStatusItem>,
    target: Retained<Target>,
    mtm: MainThreadMarker,
    icon: Retained<NSImage>,
    unread_light: Retained<NSImage>,
    unread_dark: Retained<NSImage>,
}

impl Tray {
    pub fn new(tx: UnboundedSender<TrayCmd>) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let icon = menubar_image(MENUBAR_PNG, true)?;
        item.button(mtm)?.setImage(Some(&icon));
        item.button(mtm)?
            .setImagePosition(NSCellImagePosition::ImageLeft);
        let target = Target::alloc(mtm).set_ivars(TargetIvars { tx });
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };
        Some(Self {
            item,
            target,
            mtm,
            icon,
            unread_light: menubar_image(MENUBAR_UNREAD_LIGHT_PNG, false)?,
            unread_dark: menubar_image(MENUBAR_UNREAD_DARK_PNG, false)?,
        })
    }

    /// `status` is the sync state line; `threads` is (id, label) for the newest unread threads.
    pub fn update(&self, unread: usize, status: &str, threads: &[(String, String)]) {
        let mtm = self.mtm;
        if let Some(button) = self.item.button(mtm) {
            let title = if unread > 0 {
                format!(" {unread}")
            } else {
                String::new()
            };
            button.setTitle(&NSString::from_str(&title));
            let image = if unread == 0 {
                &self.icon
            } else {
                let names = NSArray::from_slice(&[unsafe { NSAppearanceNameAqua }, unsafe {
                    NSAppearanceNameDarkAqua
                }]);
                let best = button
                    .effectiveAppearance()
                    .bestMatchFromAppearancesWithNames(&names);
                if best.is_some_and(|n| n.isEqualToString(unsafe { NSAppearanceNameDarkAqua })) {
                    &self.unread_dark
                } else {
                    &self.unread_light
                }
            };
            button.setImage(Some(image));
        }
        let badge = (unread > 0).then(|| NSString::from_str(&unread.to_string()));
        NSApplication::sharedApplication(mtm)
            .dockTile()
            .setBadgeLabel(badge.as_deref());

        let menu = NSMenu::new(mtm);
        // Items without an action are shown greyed out, as info lines.
        let add = |title: &str, action: Option<_>, key: &str| {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    action,
                    &NSString::from_str(key),
                )
            };
            if action.is_some() {
                unsafe { item.setTarget(Some(&self.target)) };
            }
            menu.addItem(&item);
            item
        };
        add(
            &match unread {
                0 => "Inbox zero".to_owned(),
                n => format!("{n} unread"),
            },
            None,
            "",
        );
        add(status, None, "");
        if !threads.is_empty() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        for (id, label) in threads {
            let item = add(label, Some(sel!(thread:)), "");
            unsafe { item.setRepresentedObject(Some(&NSString::from_str(id))) };
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add("Open ossdeck", Some(sel!(open:)), "o");
        add("Sync Now", Some(sel!(sync:)), "r");
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add("Quit ossdeck", Some(sel!(quit:)), "q");
        self.item.setMenu(Some(&menu));
    }
}

/// Menu bar only while no window is open; back in the Dock (and cmd-tab) once one is.
pub fn set_dock_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    NSApplication::sharedApplication(mtm).setActivationPolicy(if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    });
}
