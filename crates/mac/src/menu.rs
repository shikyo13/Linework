//! The menu bar, made from the command table the Windows program uses: the same
//! menus in the same order, each listing its commands in the table's order with
//! the table's shortcuts.
use crate::commands::{COMMANDS, Command};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSString, ns_string};

/// The order of the Windows menu bar.
const MENUS: [&str; 9] = ["File", "Edit", "Selection", "Search", "Go", "View", "Macro", "Tools", "Help"];

/// Each command's item carries its place in the table, counted from one, as its tag.
pub fn command(item: &NSMenuItem) -> Option<&'static Command> {
    COMMANDS.get((item.tag() as usize).checked_sub(1)?)
}

pub fn install(app: &NSApplication, target: &AnyObject, mtm: MainThreadMarker) {
    let bar = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!(""));
    // macOS always shows the program's own menu first; it holds only what the
    // system expects to find there.
    let own = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Linework"));
    own.addItem(&item(mtm, "About Linework", Some(sel!(orderFrontStandardAboutPanel:)), "", NSEventModifierFlags::empty()));
    own.addItem(&NSMenuItem::separatorItem(mtm));
    own.addItem(&item(mtm, "Hide Linework", Some(sel!(hide:)), "h", NSEventModifierFlags::Command));
    own.addItem(&item(mtm, "Quit Linework", Some(sel!(terminate:)), "q", NSEventModifierFlags::Command));
    attach(&bar, own, mtm);
    for name in MENUS {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(name));
        for (index, command) in COMMANDS.iter().enumerate().filter(|(_, command)| command.group == name) {
            let (key, modifiers) = shortcut(command.key);
            let entry = item(mtm, command.name, Some(sel!(command:)), &key, modifiers);
            entry.setTag(index as isize + 1);
            unsafe { entry.setTarget(Some(target)) };
            menu.addItem(&entry);
        }
        attach(&bar, menu, mtm);
    }
    app.setMainMenu(Some(&bar));
}

fn attach(bar: &NSMenu, menu: Retained<NSMenu>, mtm: MainThreadMarker) {
    let holder = NSMenuItem::new(mtm);
    holder.setSubmenu(Some(&menu));
    bar.addItem(&holder);
}

fn item(mtm: MainThreadMarker, title: &str, action: Option<objc2::runtime::Sel>, key: &str, modifiers: NSEventModifierFlags) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(modifiers);
    item
}

/// Turns the table's "Ctrl+Shift+O" into the key and the modifiers AppKit wants.
/// Ctrl stays the Control key and Alt is the Option key, so the same fingers
/// press the same chords on both systems.
fn shortcut(chord: &str) -> (String, NSEventModifierFlags) {
    let mut modifiers = NSEventModifierFlags::empty();
    let mut key = String::new();
    for part in chord.split('+').filter(|part| !part.is_empty()) {
        match part {
            "Ctrl" => modifiers |= NSEventModifierFlags::Control,
            "Alt" => modifiers |= NSEventModifierFlags::Option,
            "Shift" => modifiers |= NSEventModifierFlags::Shift,
            "Space" => key.push(' '),
            "Tab" => key.push('\t'),
            "Up" => key.push('\u{f700}'),
            "Down" => key.push('\u{f701}'),
            "Left" => key.push('\u{f702}'),
            "Right" => key.push('\u{f703}'),
            name => match name.strip_prefix('F').and_then(|number| number.parse::<u32>().ok()) {
                Some(number @ 1..=24) => key.extend(char::from_u32(0xf703 + number)),
                _ => key.push_str(&name.to_lowercase()),
            },
        }
    }
    if key.is_empty() {
        return (key, NSEventModifierFlags::empty());
    }
    (key, modifiers)
}
