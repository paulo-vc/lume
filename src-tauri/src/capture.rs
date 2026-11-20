//! Captura nativa de tela/janela via Windows Graphics Capture.
//! Sem a barra "está compartilhando" do navegador e, no Windows 11, sem a borda amarela.

use serde::Serialize;
use std::sync::mpsc::SyncSender;
use std::time::Duration;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

#[derive(Serialize, Clone)]
pub struct Source {
    /// "m:<índice>" para monitor, "w:<hwnd>" para janela
    pub id: String,
    pub kind: &'static str,
    pub title: String,
    pub process: String,
    pub pid: u32,
}

pub fn list_sources() -> Vec<Source> {
    let mut out = Vec::new();
    if let Ok(ms) = Monitor::enumerate() {
        for (i, m) in ms.iter().enumerate() {
            let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
            out.push(Source {
                id: format!("m:{}", m.index().unwrap_or(i + 1)),
                kind: "monitor",
                title: format!("Tela {} · {w}×{h}", i + 1),
                process: String::new(),
                pid: 0,
            });
        }
    }
    let me = std::process::id();
    if let Ok(ws) = Window::enumerate() {
        for w in ws {
            let title = w.title().unwrap_or_default();
            let pid = w.process_id().unwrap_or(0);
            if title.trim().is_empty() || pid == me || !w.is_valid() {
                continue;
            }
            if w.width().unwrap_or(0) < 120 || w.height().unwrap_or(0) < 80 {
                continue;
            }
            out.push(Source {
                id: format!("w:{}", w.as_raw_hwnd() as isize),
                kind: "window",
                title,
                process: w.process_name().unwrap_or_default(),
                pid,
            });
        }
    }
    out
}

#[derive(Clone)]
pub struct RawFrame {
    /// pixels na memória (vazio quando o quadro está só na placa de vídeo)
    pub bgra: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub gpu: Option<crate::gpu::GpuFrame>,
}

pub struct Handler {
    tx: SyncSender<RawFrame>,
    scratch: Vec<u8>,
    /// o dispositivo da captura serve também para converter e codificar
    gpu_ok: bool,
    ring: crate::gpu::CopyRing,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = SyncSender<RawFrame>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let gpu_ok = crate::gpu::prepare_device(&ctx.device);
        Ok(Self { tx: ctx.flags, scratch: Vec::new(), gpu_ok, ring: crate::gpu::CopyRing::new() })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _c: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.gpu_ok && !crate::gpu::disabled() {
            // Só uma cópia de textura dentro da placa de vídeo; nada vem para a memória.
            let (w, h) = (frame.width() as usize, frame.height() as usize);
            let (dev, ctx) = (frame.device().clone(), frame.device_context().clone());
            if let Ok(g) = self.ring.copy(&dev, &ctx, frame.as_raw_texture()) {
                let _ = self.tx.try_send(RawFrame { bgra: vec![], width: w, height: h, gpu: Some(g) });
                return Ok(());
            }
        }
        let buf = frame.buffer()?;
        let (w, h) = (buf.width() as usize, buf.height() as usize);
        let data = buf.as_nopadding_buffer(&mut self.scratch).to_vec();
        // try_send: se o encoder estiver ocupado o quadro é descartado (nunca acumula atraso).
        let _ = self.tx.try_send(RawFrame { bgra: data, width: w, height: h, gpu: None });
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub type Control = CaptureControl<Handler, Box<dyn std::error::Error + Send + Sync>>;

pub fn start(source_id: &str, fps: u32, tx: SyncSender<RawFrame>) -> Result<Control, String> {
    let interval = MinimumUpdateIntervalSettings::Custom(Duration::from_micros(1_000_000 / fps.max(1) as u64));
    let err = |e: &dyn std::fmt::Display| format!("Falha ao iniciar a captura: {e}");
    if let Some(idx) = source_id.strip_prefix("m:") {
        let idx: usize = idx.parse().map_err(|_| "Tela inválida")?;
        let m = Monitor::from_index(idx).map_err(|e| err(&e))?;
        let s = Settings::new(
            m,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            tx,
        );
        Handler::start_free_threaded(s).map_err(|e| err(&e))
    } else if let Some(h) = source_id.strip_prefix("w:") {
        let h: isize = h.parse().map_err(|_| "Janela inválida")?;
        let w = Window::from_raw_hwnd(h as *mut std::ffi::c_void);
        if !w.is_valid() {
            return Err("A janela foi fechada".into());
        }
        let s = Settings::new(
            w,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Include,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            tx,
        );
        Handler::start_free_threaded(s).map_err(|e| err(&e))
    } else {
        Err("Fonte inválida".into())
    }
}

/// Reduz a resolução (média 2x2 quando possível, senão vizinho mais próximo). Mantém dimensões pares.
pub fn downscale(f: &RawFrame, max_h: usize) -> RawFrame {
    let (sw, sh) = (f.width, f.height);
    let (dw, dh) = crate::gpu::target_dims(sw, sh, max_h);
    if dw == sw && dh == sh {
        return f.clone();
    }
    let mut out = vec![0u8; dw * dh * 4];
    let half = sw == dw * 2 && sh == dh * 2;
    for y in 0..dh {
        let sy = y * sh / dh;
        for x in 0..dw {
            let o = (y * dw + x) * 4;
            if half {
                for c in 0..4 {
                    let p = |xx: usize, yy: usize| f.bgra[(yy * sw + xx) * 4 + c] as u32;
                    out[o + c] = ((p(2 * x, 2 * y) + p(2 * x + 1, 2 * y) + p(2 * x, 2 * y + 1) + p(2 * x + 1, 2 * y + 1)) / 4) as u8;
                }
            } else {
                let sx = x * sw / dw;
                let s = (sy * sw + sx) * 4;
                out[o..o + 4].copy_from_slice(&f.bgra[s..s + 4]);
            }
        }
    }
    RawFrame { bgra: out, width: dw, height: dh, gpu: None }
}
