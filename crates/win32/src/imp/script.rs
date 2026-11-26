//! Clicks and keys posted from inside the program, so it can be driven and
//! looked at without the screen. LINEWORK_SCRIPT holds steps separated by
//! semicolons:
//!
//!   wait MS          let that long pass
//!   click X Y        press and release the left button there in the main window
//!   move X Y         move the pointer there
//!   key CHORD        press a key, such as F1, ctrl+shift+p, escape, down
//!   type TEXT        type the text, a key for each character
//!   picture NAME     write a picture of every window, files starting NAME
//!   size W H         make the main window's content that large
//!   say TEXT         write the text to the error stream, to mark the place in a run's output
//!   quit             end the program
//!
//! Each event is made the way AppKit makes one and goes through the same loop
//! as a real one.
use crate::imp::window::{app, frames};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::NSEvent;
use objc2_foundation::{NSPoint, NSString};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct Script {
    steps: VecDeque<String>,
    next: Instant,
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
}

/// Whether steps are being played, with nobody at the screen to answer a question.
pub(crate) fn playing() -> bool {
    SCRIPT.with_borrow(|script| script.is_some())
}

pub(crate) fn load() {
    let Ok(text) = std::env::var("LINEWORK_SCRIPT") else { return };
    let steps = text.split(';').map(|step| step.trim().to_owned()).filter(|step| !step.is_empty()).collect();
    SCRIPT.replace(Some(Script { steps, next: Instant::now() + Duration::from_millis(800) }));
}

/// When the next step is due, so the loop does not sleep past it.
pub(crate) fn due() -> Option<Instant> {
    SCRIPT.with_borrow(|script| script.as_ref().filter(|script| !script.steps.is_empty()).map(|script| script.next))
}

/// Runs the steps whose time has come. Called each time the loop comes round.
pub(crate) fn run() {
    loop {
        let step = SCRIPT.with_borrow_mut(|script| {
            let script = script.as_mut()?;
            if Instant::now() < script.next {
                return None;
            }
            let step = script.steps.pop_front()?;
            // Every step is given time to take effect before the next.
            script.next = Instant::now() + Duration::from_millis(120);
            Some(step)
        });
        let Some(step) = step else { return };
        let (word, rest) = step.split_once(' ').unwrap_or((&step, ""));
        let numbers: Vec<f64> = rest.split_whitespace().filter_map(|part| part.parse().ok()).collect();
        match word {
            "wait" => SCRIPT.with_borrow_mut(|script| {
                if let Some(script) = script {
                    script.next = Instant::now() + Duration::from_millis(numbers.first().copied().unwrap_or(100.0) as u64);
                }
            }),
            "click" if numbers.len() == 2 => {
                pointer(1, numbers[0], numbers[1], 1);
                pointer(2, numbers[0], numbers[1], 1);
            }
            "move" if numbers.len() == 2 => pointer(5, numbers[0], numbers[1], 0),
            "key" => key(rest),
            "type" => {
                for letter in rest.chars() {
                    typed(letter);
                }
            }
            "picture" => crate::imp::message::pictures(rest),
            "size" if numbers.len() == 2 => {
                if let Some((_, main)) = frames().into_iter().find(|(class, _)| class == "LineworkMain") {
                    main.setContentSize(objc2_foundation::NSSize::new(numbers[0], numbers[1]));
                }
            }
            "say" => eprintln!("linework-script: {rest}"),
            "quit" => std::process::exit(0),
            _ => eprintln!("linework-win32: script step not understood: {step}"),
        }
    }
}

fn post(event: Option<Retained<NSEvent>>) {
    if let Some(event) = event {
        app().postEvent_atStart(&event, false);
    }
}

/// The window the steps act on: the key window, or the main one.
fn target() -> Option<Retained<objc2_app_kit::NSWindow>> {
    let focus = unsafe { crate::imp::window::GetFocus() };
    let working = crate::imp::window::find(focus).and_then(|window| window.view.window());
    working.or_else(|| app().keyWindow()).or_else(|| frames().into_iter().find(|(class, _)| class == "LineworkMain").map(|(_, frame)| frame))
}

/// A pointer event at a place counted from the top left of the window's content.
fn pointer(kind: usize, x: f64, y: f64, clicks: isize) {
    // The place is counted in the main window; a popup of ours lying over it
    // there takes the event, as it would from the real pointer.
    let Some(main) = frames().into_iter().find(|(class, _)| class == "LineworkMain").map(|(_, frame)| frame) else { return };
    let height = main.contentView().map_or(0.0, |view| view.frame().size.height);
    let on_screen = main.convertPointToScreen(NSPoint::new(x, height - y));
    let over = frames().into_iter().map(|(_, frame)| frame).find(|frame| {
        frame.isVisible() && frame.windowNumber() != main.windowNumber() && objc2_foundation::NSPointInRect(on_screen, frame.frame())
    });
    let frame = over.unwrap_or(main);
    let place = frame.convertPointFromScreen(on_screen);
    let event: Option<Retained<NSEvent>> = unsafe {
        msg_send![class!(NSEvent), mouseEventWithType: kind, location: place, modifierFlags: 0usize, timestamp: 0.0f64,
            windowNumber: frame.windowNumber(), context: std::ptr::null::<AnyObject>(), eventNumber: 0isize, clickCount: clicks, pressure: 1.0f32]
    };
    post(event);
}

/// The Mac key number and the character for a key's name.
fn named(name: &str) -> Option<(u16, String)> {
    let special = |code: u16, unit: u32| Some((code, char::from_u32(unit).map(String::from).unwrap_or_default()));
    match name {
        "return" | "enter" => special(0x24, 13),
        "tab" => special(0x30, 9),
        "space" => special(0x31, 32),
        "backspace" => special(0x33, 127),
        "escape" | "esc" => special(0x35, 27),
        "delete" => special(0x75, 0xF728),
        "home" => special(0x73, 0xF729),
        "end" => special(0x77, 0xF72B),
        "pageup" => special(0x74, 0xF72C),
        "pagedown" => special(0x79, 0xF72D),
        "left" => special(0x7B, 0xF702),
        "right" => special(0x7C, 0xF703),
        "down" => special(0x7D, 0xF701),
        "up" => special(0x7E, 0xF700),
        _ => {
            if let Some(number) = name.strip_prefix('f').and_then(|number| number.parse::<u32>().ok()) {
                const CODES: [u16; 12] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F];
                return CODES.get(number as usize - 1).and_then(|code| special(*code, 0xF703 + number));
            }
            let letter = name.chars().next().filter(|_| name.chars().count() == 1)?;
            Some((code_of(letter)?, letter.to_string()))
        }
    }
}

/// The key that types a character on a US keyboard, for letters, digits and the
/// marks the front end binds.
fn code_of(letter: char) -> Option<u16> {
    const KEYS: &str = "asdfhgzxcv\u{0}bqweryt123465=97-80]ou[ip\u{0}lj'k;\\,/nm.";
    let lower = letter.to_ascii_lowercase();
    match lower {
        ' ' => Some(0x31),
        '`' => Some(0x32),
        '\u{0}' => None,
        _ => KEYS.chars().position(|key| key == lower).map(|at| at as u16),
    }
}

fn key_event(code: u16, text: &str, plain: &str, flags: usize) {
    let Some(frame) = target() else { return };
    for kind in [10usize, 11] {
        let event: Option<Retained<NSEvent>> = unsafe {
            msg_send![class!(NSEvent), keyEventWithType: kind, location: NSPoint::new(0.0, 0.0), modifierFlags: flags, timestamp: 0.0f64,
                windowNumber: frame.windowNumber(), context: std::ptr::null::<AnyObject>(), characters: &*NSString::from_str(text),
                charactersIgnoringModifiers: &*NSString::from_str(plain), isARepeat: false, keyCode: code]
        };
        post(event);
    }
}

fn key(chord: &str) {
    let mut flags = 0usize;
    let mut found = None;
    for part in chord.split('+').map(|part| part.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "shift" => flags |= 1 << 17,
            "ctrl" | "control" => flags |= 1 << 18,
            "alt" | "option" => flags |= 1 << 19,
            "cmd" | "command" => flags |= 1 << 20,
            name => found = named(name),
        }
    }
    let Some((code, text)) = found else { return eprintln!("linework-win32: no such key: {chord}") };
    // With Control held a letter arrives as its control character, as from a keyboard.
    let sent = match text.chars().next() {
        Some(letter) if flags & (1 << 18) != 0 && letter.is_ascii_alphabetic() => char::from(letter.to_ascii_lowercase() as u8 - b'a' + 1).to_string(),
        _ => text.clone(),
    };
    key_event(code, &sent, &text, flags);
}

fn typed(letter: char) {
    let shift = if letter.is_uppercase() { 1 << 17 } else { 0 };
    // A character with no key of its own on a US keyboard is sent on a key the
    // front end gives no meaning to.
    key_event(code_of(letter).unwrap_or(0x0A), &letter.to_string(), &letter.to_string(), shift);
}
