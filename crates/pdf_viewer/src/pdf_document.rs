//! A loaded PDF: page geometry, rasterization, and text extraction.
//!
//! On macOS pages are drawn by CoreGraphics (`CGPDFDocument`) into a BGRA
//! bitmap and text comes from PDFKit's `PDFDocument`. Neither framework is
//! exposed by the `core-graphics` crate, so the handful of C and Objective-C
//! entry points used are declared here. Elsewhere the type is a placeholder
//! with no pages.

use anyhow::{Context as _, Result};
use std::sync::Arc;

/// One rasterized page: premultiplied BGRA, `width * height * 4` bytes, the
/// layout gpui's `RenderImage` expects.
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[cfg(target_os = "macos")]
pub use macos::PdfDocument;

#[cfg(not(target_os = "macos"))]
pub use fallback::PdfDocument;

/// Shared access to the bytes for both frameworks; PDFKit reopens them.
pub type PdfBytes = Arc<Vec<u8>>;

#[cfg(target_os = "macos")]
mod macos {
    use super::{PdfBytes, RenderedPage};
    use anyhow::{Context as _, Result, anyhow, bail};
    use crate::pdf_text::MAX_PAGE_TEXT_BYTES;
    use core_foundation::{
        base::{CFIndex, CFRange, TCFType, kCFAllocatorDefault},
        data::CFData,
        string::{CFString, CFStringCreateWithSubstring, CFStringGetLength, CFStringRef},
    };
    use core_graphics::{
        base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst},
        color_space::CGColorSpace,
        context::CGContext,
        data_provider::CGDataProvider,
        geometry::{CGAffineTransform, CGPoint, CGRect, CGSize},
        sys,
    };
    use foreign_types::ForeignType as _;
    use objc::{class, msg_send, rc::autoreleasepool, runtime::Object, sel, sel_impl};
    use std::{
        os::raw::c_int,
        sync::{Arc, Mutex},
    };

    #[repr(C)]
    struct CGPDFDocumentOpaque {
        _private: [u8; 0],
    }
    #[repr(C)]
    struct CGPDFPageOpaque {
        _private: [u8; 0],
    }
    type CGPDFDocumentRef = *mut CGPDFDocumentOpaque;
    type CGPDFPageRef = *mut CGPDFPageOpaque;

    /// `CGPDFBox` value for the crop box, which is what viewers display.
    const CG_PDF_CROP_BOX: c_int = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPDFDocumentCreateWithProvider(provider: sys::CGDataProviderRef) -> CGPDFDocumentRef;
        fn CGPDFDocumentRelease(document: CGPDFDocumentRef);
        fn CGPDFDocumentGetNumberOfPages(document: CGPDFDocumentRef) -> usize;
        fn CGPDFDocumentIsUnlocked(document: CGPDFDocumentRef) -> bool;
        fn CGPDFDocumentGetPage(document: CGPDFDocumentRef, page_number: usize) -> CGPDFPageRef;
        fn CGPDFPageGetBoxRect(page: CGPDFPageRef, pdf_box: c_int) -> CGRect;
        fn CGPDFPageGetDrawingTransform(
            page: CGPDFPageRef,
            pdf_box: c_int,
            rect: CGRect,
            rotate: c_int,
            preserve_aspect_ratio: bool,
        ) -> CGAffineTransform;
        fn CGContextDrawPDFPage(context: sys::CGContextRef, page: CGPDFPageRef);
        fn CGBitmapContextCreate(
            data: *mut std::os::raw::c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: sys::CGColorSpaceRef,
            bitmap_info: u32,
        ) -> sys::CGContextRef;
    }

    // Only linked so `class!(PDFDocument)` resolves at runtime.
    #[link(name = "PDFKit", kind = "framework")]
    unsafe extern "C" {}

    /// Converts at most `max_bytes` worth of `string` to Rust, taking a
    /// substring first so a huge page never gets copied whole. A UTF-16 unit
    /// is at least one UTF-8 byte, so `max_bytes` units bound the result.
    unsafe fn bounded_string(string: CFStringRef, max_bytes: usize) -> String {
        let length = unsafe { CFStringGetLength(string) };
        let limit = CFIndex::try_from(max_bytes).unwrap_or(CFIndex::MAX);
        let text = if length > limit {
            let substring = unsafe {
                CFStringCreateWithSubstring(
                    kCFAllocatorDefault,
                    string,
                    CFRange::init(0, limit),
                )
            };
            if substring.is_null() {
                return String::new();
            }
            unsafe { CFString::wrap_under_create_rule(substring) }.to_string()
        } else {
            unsafe { CFString::wrap_under_get_rule(string) }.to_string()
        };
        crate::pdf_text::cap_page_text(text, max_bytes)
    }

    pub struct PdfDocument {
        bytes: PdfBytes,
        document: CGPDFDocumentRef,
        // Keeps the data provider (and so `bytes`) alive as long as the document.
        _provider: CGDataProvider,
        page_sizes: Vec<(f32, f32)>,
        /// CoreGraphics does not promise concurrent drawing from one
        /// `CGPDFDocument` is safe, so rasterization is serialized.
        draw_lock: Mutex<()>,
    }

    // The document is only touched through `draw_lock` or immutable reads.
    unsafe impl Send for PdfDocument {}
    unsafe impl Sync for PdfDocument {}

    impl Drop for PdfDocument {
        fn drop(&mut self) {
            unsafe { CGPDFDocumentRelease(self.document) };
        }
    }

    impl PdfDocument {
        pub const SUPPORTED: bool = true;

        pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
            let bytes: PdfBytes = Arc::new(bytes);
            let provider = CGDataProvider::from_buffer(bytes.clone());
            let document = unsafe { CGPDFDocumentCreateWithProvider(provider.as_ptr()) };
            if document.is_null() {
                bail!("not a PDF document");
            }
            if !unsafe { CGPDFDocumentIsUnlocked(document) } {
                unsafe { CGPDFDocumentRelease(document) };
                bail!("the PDF is password protected");
            }
            let page_count = unsafe { CGPDFDocumentGetNumberOfPages(document) };
            let page_sizes = (1..=page_count)
                .map(|number| {
                    let page = unsafe { CGPDFDocumentGetPage(document, number) };
                    if page.is_null() {
                        return (1.0, 1.0);
                    }
                    let rect = unsafe { CGPDFPageGetBoxRect(page, CG_PDF_CROP_BOX) };
                    let transform = unsafe {
                        CGPDFPageGetDrawingTransform(page, CG_PDF_CROP_BOX, rect, 0, true)
                    };
                    // Rotated pages swap their displayed width and height.
                    let rotated = transform.a.abs() < f64::EPSILON;
                    let (width, height) = if rotated {
                        (rect.size.height, rect.size.width)
                    } else {
                        (rect.size.width, rect.size.height)
                    };
                    (width.max(1.0) as f32, height.max(1.0) as f32)
                })
                .collect();
            Ok(Self {
                bytes,
                document,
                _provider: provider,
                page_sizes,
                draw_lock: Mutex::new(()),
            })
        }

        pub fn page_count(&self) -> usize {
            self.page_sizes.len()
        }

        /// Displayed size of every page in PDF points.
        pub fn page_sizes(&self) -> &[(f32, f32)] {
            &self.page_sizes
        }

        /// Draws page `index` (zero-based) at `scale` device pixels per point.
        pub fn render_page(&self, index: usize, scale: f32) -> Result<RenderedPage> {
            let (page_width, page_height) = self
                .page_sizes
                .get(index)
                .copied()
                .with_context(|| format!("page {} does not exist", index + 1))?;
            let (width, height) = crate::pdf_text::page_pixel_size((page_width, page_height), scale);
            if (width as f32) * (height as f32) > crate::pdf_text::MAX_PAGE_PIXELS {
                bail!("page {} would need {width}x{height} pixels at this zoom", index + 1);
            }
            let (width, height) = (width as usize, height as usize);
            let _guard = self
                .draw_lock
                .lock()
                .map_err(|_| anyhow!("PDF draw lock poisoned"))?;
            let page = unsafe { CGPDFDocumentGetPage(self.document, index + 1) };
            if page.is_null() {
                bail!("page {} could not be loaded", index + 1);
            }
            let color_space = CGColorSpace::create_device_rgb();
            // `CGContext::create_bitmap_context` asserts on a null result; a
            // page CoreGraphics will not allocate for must be an error, not a
            // panic on the render thread.
            let context_pointer = unsafe {
                CGBitmapContextCreate(
                    std::ptr::null_mut(),
                    width,
                    height,
                    8,
                    width * 4,
                    color_space.as_ptr(),
                    kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little,
                )
            };
            if context_pointer.is_null() {
                bail!(
                    "page {} could not get a {width}x{height} bitmap at this zoom",
                    index + 1
                );
            }
            let mut context = unsafe { CGContext::from_ptr(context_pointer) };
            let bitmap_rect = CGRect::new(
                &CGPoint::new(0.0, 0.0),
                &CGSize::new(width as f64, height as f64),
            );
            context.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
            context.fill_rect(bitmap_rect);
            // CGPDFPageGetDrawingTransform only ever scales a page down, so the
            // zoom is applied to the context and the transform is asked for a
            // 1:1 fit, which still handles the box origin and page rotation.
            context.scale(scale as f64, scale as f64);
            let page_rect = CGRect::new(
                &CGPoint::new(0.0, 0.0),
                &CGSize::new(page_width as f64, page_height as f64),
            );
            let transform = unsafe {
                CGPDFPageGetDrawingTransform(page, CG_PDF_CROP_BOX, page_rect, 0, true)
            };
            context.concat_ctm(transform);
            unsafe { CGContextDrawPDFPage(context.as_ptr(), page) };
            context.flush();
            let bytes_per_row = context.bytes_per_row();
            let data = context.data();
            let mut bgra = Vec::with_capacity(width * height * 4);
            for row in data.chunks(bytes_per_row).take(height) {
                bgra.extend_from_slice(&row[..width * 4]);
            }
            Ok(RenderedPage {
                width: width as u32,
                height: height as u32,
                bgra,
            })
        }

        /// Text of every page through PDFKit, in page order. Pages without
        /// extractable text (scans) yield empty strings.
        pub fn page_texts(&self) -> Result<Vec<String>> {
            let bytes = self.bytes.clone();
            let expected_pages = self.page_count();
            autoreleasepool(|| unsafe {
                let data = CFData::from_arc(bytes);
                let document: *mut Object = msg_send![class!(PDFDocument), alloc];
                let document: *mut Object =
                    msg_send![document, initWithData: data.as_concrete_TypeRef()];
                if document.is_null() {
                    bail!("PDFKit could not open the document");
                }
                let page_count: usize = msg_send![document, pageCount];
                let mut texts = Vec::with_capacity(page_count);
                let mut collected = 0;
                for index in 0..page_count {
                    // Past the cap, stop asking PDFKit for text at all.
                    if collected >= crate::pdf_text::MAX_TEXT_BYTES {
                        texts.push(String::new());
                        continue;
                    }
                    let page: *mut Object = msg_send![document, pageAtIndex: index];
                    let mut text = String::new();
                    if !page.is_null() {
                        let string: *mut Object = msg_send![page, string];
                        if !string.is_null() {
                            text = bounded_string(string as CFStringRef, MAX_PAGE_TEXT_BYTES);
                        }
                    }
                    collected += text.len();
                    texts.push(text);
                }
                let _: () = msg_send![document, release];
                texts.resize(expected_pages.max(texts.len()), String::new());
                Ok(crate::pdf_text::cap_text_total(
                    texts,
                    crate::pdf_text::MAX_TEXT_BYTES,
                ))
            })
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod fallback {
    use super::RenderedPage;
    use anyhow::{Result, bail};

    pub struct PdfDocument;

    impl PdfDocument {
        pub const SUPPORTED: bool = false;

        pub fn from_bytes(_bytes: Vec<u8>) -> Result<Self> {
            Ok(Self)
        }

        pub fn page_count(&self) -> usize {
            0
        }

        pub fn page_sizes(&self) -> &[(f32, f32)] {
            &[]
        }

        pub fn render_page(&self, _index: usize, _scale: f32) -> Result<RenderedPage> {
            bail!("PDF preview is macOS only")
        }

        pub fn page_texts(&self) -> Result<Vec<String>> {
            bail!("PDF text extraction is macOS only")
        }
    }
}

/// Loads a document from bytes already read by the project's `Fs`; the
/// frameworks never see a path or URL. Errors carry the file name.
pub fn load(bytes: Vec<u8>, name: &str) -> Result<PdfDocument> {
    crate::pdf_text::reject_oversized(bytes.len(), name)?;
    PdfDocument::from_bytes(bytes).with_context(|| format!("opening {name}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page 200x100pt PDF with a black square and the word "Hello",
    /// written by hand with a correct cross-reference table.
    pub(crate) fn minimal_pdf() -> Vec<u8> {
        let content = "0 0 0 rg 10 10 80 80 re f BT /F1 24 Tf 100 40 Td (Hello) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn garbage_is_rejected_or_empty() {
        let result = load(b"not a pdf".to_vec(), "junk.pdf");
        if PdfDocument::SUPPORTED {
            assert!(result.is_err());
        } else {
            assert_eq!(result.expect("placeholder loads").page_count(), 0);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn renders_page_one_of_a_generated_pdf_and_extracts_its_text() {
        let document = load(minimal_pdf(), "tiny.pdf").expect("generated PDF should parse");
        assert_eq!(document.page_count(), 1);
        assert_eq!(document.page_sizes(), &[(200.0, 100.0)]);

        let page = document.render_page(0, 2.0).expect("page 1 should render");
        assert_eq!((page.width, page.height), (400, 200));
        assert_eq!(page.bgra.len(), 400 * 200 * 4);
        let pixel_at = |x: usize, y: usize| {
            // CoreGraphics rows run bottom-up relative to PDF space; the
            // bitmap origin is the top-left corner.
            let offset = (y * 400 + x) * 4;
            &page.bgra[offset..offset + 4]
        };
        assert_eq!(pixel_at(100, 100), &[0, 0, 0, 255], "inside the black square");
        assert_eq!(pixel_at(390, 10), &[255, 255, 255, 255], "blank corner stays white");

        let texts = document.page_texts().expect("PDFKit should read the page");
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("Hello"), "got {:?}", texts[0]);

        assert!(document.render_page(1, 1.0).is_err());
    }
}
