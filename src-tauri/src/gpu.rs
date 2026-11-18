//! Caminho todo na placa de vídeo: a captura entrega uma textura BGRA, o processador de vídeo
//! da GPU reduz e converte para NV12, e o encoder lê essa textura direto. Nenhum pixel passa
//! pela CPU. Se qualquer peça falhar, a captura volta a mandar os pixels pela memória (caminho antigo).

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::Interface;
use windows::core::BOOL;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Se o caminho pela GPU falhou nesta sessão, a captura passa a mandar pixels pela CPU.
pub static BROKEN: AtomicBool = AtomicBool::new(false);

pub fn disabled() -> bool {
    BROKEN.load(Ordering::SeqCst) || std::env::var_os("LUME_CPU").is_some()
}

/// Textura de um quadro capturado (cópia própria, a da captura volta logo para o Windows).
#[derive(Clone)]
pub struct GpuFrame {
    pub tex: ID3D11Texture2D,
    pub dev: ID3D11Device,
}
// O dispositivo é protegido para uso por várias threads (SetMultithreadProtected).
unsafe impl Send for GpuFrame {}

/// Prepara o dispositivo da captura para ser usado também pelo conversor e pelo encoder.
pub fn prepare_device(dev: &ID3D11Device) -> bool {
    unsafe {
        let Ok(mt) = dev.cast::<windows::Win32::Graphics::Direct3D11::ID3D11Multithread>() else { return false };
        let _ = mt.SetMultithreadProtected(true);
        dev.cast::<ID3D11VideoDevice>().is_ok()
    }
}

/// Algumas texturas em rodízio: a da captura é copiada para cá na hora.
pub struct CopyRing {
    texs: Vec<ID3D11Texture2D>,
    next: usize,
    size: (u32, u32),
}

impl CopyRing {
    pub fn new() -> Self {
        Self { texs: vec![], next: 0, size: (0, 0) }
    }

    pub fn copy(&mut self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> windows::core::Result<GpuFrame> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut d);
            if self.size != (d.Width, d.Height) {
                self.texs.clear();
                self.size = (d.Width, d.Height);
            }
            if self.texs.len() < 4 {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: d.Width,
                    Height: d.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                dev.CreateTexture2D(&desc, None, Some(&mut t))?;
                self.texs.push(t.unwrap());
                self.next = self.texs.len() - 1;
            }
            let tex = self.texs[self.next].clone();
            self.next = (self.next + 1) % 4;
            ctx.CopyResource(&tex, src);
            Ok(GpuFrame { tex, dev: dev.clone() })
        }
    }
}

/// BGRA (qualquer tamanho) -> NV12 no tamanho do encoder, pelo processador de vídeo da GPU.
pub struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    en: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    outs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    src: (u32, u32),
    pub dst: (u32, u32),
}

impl Converter {
    pub fn new(dev: &ID3D11Device, src: (u32, u32), dst: (u32, u32)) -> windows::core::Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = dev.cast()?;
            let vctx: ID3D11VideoContext = dev.GetImmediateContext()?.cast()?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: src.0,
                InputHeight: src.1,
                OutputFrameRate: rate,
                OutputWidth: dst.0,
                OutputHeight: dst.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&cd)?;
            let vp = vdev.CreateVideoProcessor(&en, 0)?;
            // Entrada: RGB 0-255. Saída: YCbCr BT.601 16-235 (o mesmo do caminho pela CPU).
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 });
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            let full_src = RECT { left: 0, top: 0, right: src.0 as i32, bottom: src.1 as i32 };
            let full_dst = RECT { left: 0, top: 0, right: dst.0 as i32, bottom: dst.1 as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&full_src));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&full_dst));
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&full_dst));

            let mut outs = vec![];
            for _ in 0..4 {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: dst.0,
                    Height: dst.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                dev.CreateTexture2D(&desc, None, Some(&mut t))?;
                let t = t.unwrap();
                let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut v = None;
                vdev.CreateVideoProcessorOutputView(&t, &en, &od, Some(&mut v))?;
                outs.push((t, v.unwrap()));
            }
            let _ = D3D11_SRV_DIMENSION_UNKNOWN;
            Ok(Self { vdev, vctx, en, vp, outs, next: 0, src, dst })
        }
    }

    pub fn fits(&self, src: (u32, u32), dst: (u32, u32)) -> bool {
        self.src == src && self.dst == dst
    }

    /// Converte um quadro. Devolve a textura NV12 (em rodízio; vale até 3 quadros depois).
    pub fn convert(&mut self, input: &ID3D11Texture2D) -> windows::core::Result<ID3D11Texture2D> {
        unsafe {
            let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut iv = None;
            self.vdev.CreateVideoProcessorInputView(input, &self.en, &id, Some(&mut iv))?;
            let (tex, ov) = self.outs[self.next].clone();
            self.next = (self.next + 1) % self.outs.len();
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                pInputSurface: ManuallyDrop::new(iv),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.vp, &ov, 0, &streams);
            let [mut s] = streams;
            ManuallyDrop::drop(&mut s.pInputSurface);
            r?;
            Ok(tex)
        }
    }
}

/// Tamanho de saída: altura máxima `max_h` (0 = nativa), proporção mantida, dimensões pares.
pub fn target_dims(sw: usize, sh: usize, max_h: usize) -> (usize, usize) {
    let (dw, dh) = if max_h == 0 || sh <= max_h { (sw, sh) } else { (sw * max_h / sh, max_h) };
    (dw & !1, dh & !1)
}
