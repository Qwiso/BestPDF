//! Reading and writing picture files through Windows Imaging Component (WIC).

use std::path::Path;
use std::rc::Rc;

use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARIANT, VT_R4};

use crate::overlay::Image;

#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Png,
    /// Quality 0.0–1.0.
    Jpeg(f32),
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg(_) => "jpg",
        }
    }
}

fn factory() -> windows::core::Result<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

/// Any image Windows can decode (PNG, JPEG, BMP, GIF, TIFF, …) as BGRA.
pub fn load(path: &Path) -> Result<Rc<Image>, String> {
    let fail = |e: windows::core::Error| format!("Could not read {}: {}", path.display(), e.message());
    unsafe {
        let wic = factory().map_err(fail)?;
        let decoder = wic
            .CreateDecoderFromFilename(&HSTRING::from(path.as_os_str()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .map_err(fail)?;
        let frame = decoder.GetFrame(0).map_err(fail)?;
        let converter = wic.CreateFormatConverter().map_err(fail)?;
        converter
            .Initialize(&frame, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(fail)?;
        let (mut w, mut h) = (0, 0);
        converter.GetSize(&mut w, &mut h).map_err(fail)?;
        if w == 0 || h == 0 || w > 20_000 || h > 20_000 {
            return Err(format!("{} is too large or empty", path.display()));
        }
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut bgra).map_err(fail)?;
        Ok(Image::new(w as i32, h as i32, bgra))
    }
}

/// Writes 32-bit BGRA pixels (alpha ignored for JPEG) to `path`.
pub fn save(path: &Path, w: i32, h: i32, bgra: &[u8], format: Format) -> Result<(), String> {
    let fail = |e: windows::core::Error| format!("Could not write {}: {}", path.display(), e.message());
    let (container, pixel_format): (GUID, GUID) = match format {
        Format::Png => (GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA),
        Format::Jpeg(_) => (GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR),
    };
    // JPEG has no alpha: pack to 24-bit BGR.
    let (pixels, stride) = match format {
        Format::Png => (bgra.to_vec(), w as u32 * 4),
        Format::Jpeg(_) => {
            let stride = (w as u32 * 3).div_ceil(4) * 4;
            let mut out = vec![0u8; (stride * h as u32) as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let s = (y * w as usize + x) * 4;
                    let d = y * stride as usize + x * 3;
                    out[d..d + 3].copy_from_slice(&bgra[s..s + 3]);
                }
            }
            (out, stride)
        }
    };
    unsafe {
        let wic = factory().map_err(fail)?;
        let stream = wic.CreateStream().map_err(fail)?;
        stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), GENERIC_WRITE.0).map_err(fail)?;
        let encoder = wic.CreateEncoder(&container, std::ptr::null()).map_err(fail)?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).map_err(fail)?;
        let (mut frame, mut props) = (None, None);
        encoder.CreateNewFrame(&mut frame, &mut props).map_err(fail)?;
        let frame = frame.ok_or_else(|| format!("Could not write {}", path.display()))?;
        if let (Format::Jpeg(quality), Some(props)) = (format, &props) {
            set_quality(props, quality);
        }
        frame.Initialize(props.as_ref()).map_err(fail)?;
        frame.SetSize(w as u32, h as u32).map_err(fail)?;
        let mut pf = pixel_format;
        frame.SetPixelFormat(&mut pf).map_err(fail)?;
        frame.WritePixels(h as u32, stride, &pixels).map_err(fail)?;
        frame.Commit().map_err(fail)?;
        encoder.Commit().map_err(fail)?;
    }
    Ok(())
}

unsafe fn set_quality(props: &IPropertyBag2, quality: f32) {
    let name = HSTRING::from("ImageQuality");
    let option = PROPBAG2 { pstrName: windows::core::PWSTR(name.as_ptr() as *mut u16), ..Default::default() };
    let mut value = VARIANT::default();
    (*value.Anonymous.Anonymous).vt = VT_R4;
    (*value.Anonymous.Anonymous).Anonymous.fltVal = quality.clamp(0.0, 1.0);
    let _ = props.Write(1, &option, &value);
}

/// JPEG-encodes BGRA pixels in memory (through a temp file).
pub fn jpeg_bytes(w: i32, h: i32, bgra: &[u8], quality: f32) -> Result<Vec<u8>, String> {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = std::env::temp_dir().join(format!("bestpdf-{}-{n}.jpg", std::process::id()));
    let result = save(&temp, w, h, bgra, Format::Jpeg(quality)).and_then(|()| std::fs::read(&temp).map_err(|e| e.to_string()));
    let _ = std::fs::remove_file(&temp);
    result
}
