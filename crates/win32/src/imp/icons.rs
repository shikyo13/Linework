//! Stand-ins for the pictures Windows draws from its Segoe MDL2 Assets font,
//! which only Windows has. Each is a few thin strokes on a 16-unit square, the
//! grid and weight those glyphs are drawn on, reaching the square's edges as they do.
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
        // Warning: a triangle with an exclamation mark
        0xE7BA => &[
            Shape(&[(8., 1.), (16., 15.), (0., 15.)]),
            Line(&[(8., 6.), (8., 10.)]),
            Dot(8., 12.5, 0.8),
        ],
        // Add
        0xE710 => &[Line(&[(8., 0.), (8., 16.)]), Line(&[(0., 8.), (16., 8.)])],
        // Cancel
        0xE711 => &[Line(&[(1., 1.), (15., 15.)]), Line(&[(15., 1.), (1., 15.)])],
        // More
        0xE712 => &[Dot(2., 8., 1.1), Dot(8., 8., 1.1), Dot(14., 8., 1.1)],
        // Search
        0xE721 => &[Ring(10., 6., 5.5), Line(&[(6., 10.), (0., 16.)])],
        // Keyboard
        0xE765 => &[
            Shape(&[(0., 3.), (16., 3.), (16., 13.), (0., 13.)]),
            Dot(3., 6., 0.6), Dot(5.5, 6., 0.6), Dot(8., 6., 0.6), Dot(10.5, 6., 0.6), Dot(13., 6., 0.6),
            Dot(3., 8.3, 0.6), Dot(5.5, 8.3, 0.6), Dot(8., 8.3, 0.6), Dot(10.5, 8.3, 0.6), Dot(13., 8.3, 0.6),
            Line(&[(4., 10.7), (12., 10.7)]),
        ],
        // Back and forward
        0xE76B => &[Line(&[(11., 1.), (4., 8.), (11., 15.)])],
        0xE76C => &[Line(&[(5., 1.), (12., 8.), (5., 15.)])],
        // Home
        0xE80F => &[
            Line(&[(0., 8.), (8., 0.), (16., 8.)]),
            Line(&[(2.5, 6.), (2.5, 16.), (6.5, 16.), (6.5, 10.), (9.5, 10.), (9.5, 16.), (13.5, 16.), (13.5, 6.)]),
        ],
        // Copy
        0xE8C8 => &[
            Shape(&[(4.5, 4.), (14.5, 4.), (14.5, 16.), (4.5, 16.)]),
            Line(&[(1.5, 12.), (1.5, 0.), (11., 0.)]),
        ],
        // The window's minimize, maximize, restore and close buttons
        0xE921 => &[Line(&[(0., 8.), (16., 8.)])],
        0xE922 => &[Shape(&[(0., 0.), (16., 0.), (16., 16.), (0., 16.)])],
        0xE923 => &[
            Shape(&[(0., 3.5), (12.5, 3.5), (12.5, 16.), (0., 16.)]),
            Line(&[(3.5, 3.5), (3.5, 0.), (16., 0.), (16., 12.5), (12.5, 12.5)]),
        ],
        0xE8BB => &[Line(&[(0., 0.), (16., 16.)]), Line(&[(16., 0.), (0., 16.)])],
        // A list of items
        0xE179 => &[
            Dot(1.5, 3., 1.), Dot(1.5, 8., 1.), Dot(1.5, 13., 1.),
            Line(&[(5., 3.), (16., 3.)]), Line(&[(5., 8.), (16., 8.)]), Line(&[(5., 13.), (16., 13.)]),
        ],
        _ => &[Shape(&[(0., 0.), (16., 0.), (16., 16.), (0., 16.)])],
    }
}

/// Draws one picture centred in `area`, `size` points across, in the color given.
/// The caller has made the target the current drawing context.
///
/// A glyph of that font fills its square from edge to edge with strokes one
/// sixteenth of it wide, so a picture asked for at 10 is 10 across. Corners are
/// put on the centres of pixels, which keeps a one-pixel stroke one pixel wide.
pub(crate) fn draw(code: u16, area: NSRect, size: f64, ink: u32) {
    let pen = (size / 16.0).max(1.0);
    let left = (area.origin.x + (area.size.width - size) / 2.0).floor();
    let top = (area.origin.y + (area.size.height - size) / 2.0).floor();
    let place = |x: f32, y: f32| NSPoint::new(left + 0.5 + x as f64 / 16.0 * (size - 1.0), top + 0.5 + y as f64 / 16.0 * (size - 1.0));
    let at = |x: f32, y: f32| {
        let point = place(x, y);
        NSPoint::new(point.x.floor() + 0.5, point.y.floor() + 0.5)
    };
    crate::imp::gdi::color(ink).set();
    for part in parts(code) {
        let path = NSBezierPath::bezierPath();
        path.setLineWidth(pen);
        path.setLineCapStyle(NSLineCapStyle::Square);
        path.setLineJoinStyle(NSLineJoinStyle::Miter);
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
                let (centre, radius) = (place(x, y), radius as f64 / 16.0 * (size - 1.0));
                let bounds = NSRect::new(NSPoint::new(centre.x - radius, centre.y - radius), NSSize::new(radius * 2.0, radius * 2.0));
                path.appendBezierPathWithOvalInRect(bounds);
                if matches!(part, Ring(..)) { path.stroke() } else { path.fill() }
            }
        }
    }
}
