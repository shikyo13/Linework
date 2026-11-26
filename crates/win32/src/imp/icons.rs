//! Stand-ins for the pictures Windows draws from its Segoe MDL2 Assets font,
//! which only Windows has. Each is a few thin strokes on a 16-unit square, the
//! grid and weight those glyphs are drawn on.
use objc2_app_kit::{NSBezierPath, NSLineCapStyle, NSLineJoinStyle};
use objc2_foundation::{NSPoint, NSRect, NSSize};

enum Part {
    /// An open run of straight lines.
    Line(&'static [(f32, f32)]),
    /// A closed outline.
    Shape(&'static [(f32, f32)]),
    /// A circle's outline: centre and radius.
    Ring(f32, f32, f32),
    /// A filled dot: centre and radius.
    Dot(f32, f32, f32),
}
use Part::*;

fn parts(code: u16) -> &'static [Part] {
    match code {
        // Add
        0xE710 => &[Line(&[(8., 2.5), (8., 13.5)]), Line(&[(2.5, 8.), (13.5, 8.)])],
        // Cancel, and the window's close button
        0xE711 | 0xE8BB => &[Line(&[(3., 3.), (13., 13.)]), Line(&[(13., 3.), (3., 13.)])],
        // More
        0xE712 => &[Dot(3., 8., 1.), Dot(8., 8., 1.), Dot(13., 8., 1.)],
        // Search
        0xE721 => &[Ring(9.5, 6.5, 4.5), Line(&[(6.3, 9.7), (2., 14.)])],
        // Keyboard
        0xE765 => &[
            Shape(&[(1.5, 4.), (14.5, 4.), (14.5, 12.), (1.5, 12.)]),
            Dot(4., 6.5, 0.6), Dot(6.7, 6.5, 0.6), Dot(9.3, 6.5, 0.6), Dot(12., 6.5, 0.6),
            Line(&[(4.5, 9.5), (11.5, 9.5)]),
        ],
        // Back and forward
        0xE76B => &[Line(&[(10., 2.5), (4.5, 8.), (10., 13.5)])],
        0xE76C => &[Line(&[(6., 2.5), (11.5, 8.), (6., 13.5)])],
        // Home
        0xE80F => &[
            Line(&[(1.5, 8.), (8., 2.), (14.5, 8.)]),
            Line(&[(3.5, 6.5), (3.5, 14.), (6.5, 14.), (6.5, 9.5), (9.5, 9.5), (9.5, 14.), (12.5, 14.), (12.5, 6.5)]),
        ],
        // Copy
        0xE8C8 => &[
            Shape(&[(5., 4.5), (13., 4.5), (13., 14.5), (5., 14.5)]),
            Line(&[(3., 11.5), (3., 1.5), (10.5, 1.5)]),
        ],
        // The window's minimize, maximize and restore buttons
        0xE921 => &[Line(&[(3., 8.), (13., 8.)])],
        0xE922 => &[Shape(&[(3., 3.), (13., 3.), (13., 13.), (3., 13.)])],
        0xE923 => &[
            Shape(&[(3., 5.), (11., 5.), (11., 13.), (3., 13.)]),
            Line(&[(5., 5.), (5., 3.), (13., 3.), (13., 11.), (11., 11.)]),
        ],
        // A list of items
        0xE179 => &[
            Dot(3., 4., 1.), Dot(3., 8., 1.), Dot(3., 12., 1.),
            Line(&[(6., 4.), (14., 4.)]), Line(&[(6., 8.), (14., 8.)]), Line(&[(6., 12.), (14., 12.)]),
        ],
        _ => &[Shape(&[(3., 3.), (13., 3.), (13., 13.), (3., 13.)])],
    }
}

/// Draws one picture centred in `area`, `size` points across, in the color given.
/// The caller has made the target the current drawing context.
pub(crate) fn draw(code: u16, area: NSRect, size: f64, ink: u32) {
    let unit = size / 16.0;
    let left = area.origin.x + (area.size.width - size) / 2.0;
    let top = area.origin.y + (area.size.height - size) / 2.0;
    let at = |x: f32, y: f32| NSPoint::new(left + x as f64 * unit, top + y as f64 * unit);
    crate::imp::gdi::color(ink).set();
    for part in parts(code) {
        let path = NSBezierPath::bezierPath();
        path.setLineWidth(unit.max(1.0));
        path.setLineCapStyle(NSLineCapStyle::Round);
        path.setLineJoinStyle(NSLineJoinStyle::Round);
        match *part {
            Line(points) | Shape(points) => {
                for (index, &(x, y)) in points.iter().enumerate() {
                    if index == 0 { path.moveToPoint(at(x, y)) } else { path.lineToPoint(at(x, y)) }
                }
                if matches!(part, Shape(_)) {
                    path.closePath();
                }
                path.stroke();
            }
            Ring(x, y, radius) | Dot(x, y, radius) => {
                let (centre, radius) = (at(x, y), radius as f64 * unit);
                let bounds = NSRect::new(NSPoint::new(centre.x - radius, centre.y - radius), NSSize::new(radius * 2.0, radius * 2.0));
                path.appendBezierPathWithOvalInRect(bounds);
                if matches!(part, Ring(..)) { path.stroke() } else { path.fill() }
            }
        }
    }
}
