//! System cursor shape and position on macOS (§6 rule 5).
//!
//! The capture stream is configured without the cursor; this module reads
//! the current system cursor (`NSCursor.currentSystemCursor`) and pointer
//! location so the Controller can draw the pointer itself.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::Arc;

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::CursorImage;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Point {
    x: f64,
    y: f64,
}

// SAFETY: matches the layout and type encoding of CGPoint / NSPoint.
unsafe impl Encode for Point {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl RefEncode for Point {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Size {
    w: f64,
    h: f64,
}

// SAFETY: matches CGSize / NSSize.
unsafe impl Encode for Size {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RectC {
    origin: Point,
    size: Size,
}

// SAFETY: matches CGRect / NSRect.
unsafe impl Encode for RectC {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[Point::ENCODING, Size::ENCODING]);
}
unsafe impl RefEncode for RectC {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// Opaque `CGImage`, typed so objc2 can check the method signature.
#[repr(C)]
struct CgImage {
    _private: [u8; 0],
}

// SAFETY: `CGImageRef` is encoded as a pointer to an opaque struct.
unsafe impl RefEncode for CgImage {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(img: *const c_void) -> usize;
    fn CGImageGetHeight(img: *const c_void) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(cs: *mut c_void);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        w: usize,
        h: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        cs: *mut c_void,
        info: u32,
    ) -> *mut c_void;
    fn CGContextDrawImage(ctx: *mut c_void, rect: RectC, img: *const c_void);
    fn CGContextRelease(ctx: *mut c_void);
    fn CGEventCreate(src: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(ev: *mut c_void) -> Point;
    fn CFRelease(cf: *const c_void);
}

/// kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big → RGBA bytes.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);
/// Cursors bigger than this are clipped (protects the channel).
const MAX_DIM: usize = 256;

/// Pointer location in global display points.
pub fn location() -> (f64, f64) {
    // SAFETY: event created and released here.
    unsafe {
        let ev = CGEventCreate(std::ptr::null());
        if ev.is_null() {
            return (0.0, 0.0);
        }
        let p = CGEventGetLocation(ev);
        CFRelease(ev);
        (p.x, p.y)
    }
}

/// Current system cursor image, in pixels (straight alpha RGBA).
pub fn current_image() -> Option<Arc<CursorImage>> {
    autoreleasepool(|_| {
        // SAFETY: AppKit class methods returning autoreleased objects,
        // used inside the pool; CG objects are released below.
        unsafe {
            let cursor: *mut AnyObject = msg_send![class!(NSCursor), currentSystemCursor];
            if cursor.is_null() {
                return None;
            }
            let image: *mut AnyObject = msg_send![cursor, image];
            if image.is_null() {
                return None;
            }
            let hot: Point = msg_send![cursor, hotSpot];
            let size: Size = msg_send![image, size];
            let cg: *const CgImage = msg_send![
                image,
                CGImageForProposedRect: std::ptr::null_mut::<RectC>(),
                context: std::ptr::null_mut::<AnyObject>(),
                hints: std::ptr::null_mut::<AnyObject>()
            ];
            if cg.is_null() {
                return None;
            }
            let cg: *const c_void = cg.cast();
            let (w, h) = (
                CGImageGetWidth(cg).min(MAX_DIM),
                CGImageGetHeight(cg).min(MAX_DIM),
            );
            if w == 0 || h == 0 {
                return None;
            }
            let mut rgba = vec![0u8; w * h * 4];
            let cs = CGColorSpaceCreateDeviceRGB();
            let ctx = CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                cs,
                RGBA_PREMULTIPLIED,
            );
            CGColorSpaceRelease(cs);
            if ctx.is_null() {
                return None;
            }
            CGContextDrawImage(
                ctx,
                RectC {
                    origin: Point::default(),
                    size: Size {
                        w: w as f64,
                        h: h as f64,
                    },
                },
                cg,
            );
            CGContextRelease(ctx);
            unpremultiply(&mut rgba);
            // Hotspot is in points; the image is in pixels.
            let k = if size.w > 0.0 { w as f64 / size.w } else { 1.0 };
            Some(Arc::new(CursorImage::new(
                w as u32,
                h as u32,
                (hot.x * k).round().max(0.0) as u32,
                (hot.y * k).round().max(0.0) as u32,
                rgba,
                k as f32,
            )))
        }
    })
}

fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}
