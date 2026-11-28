//! Captura de monitor via DXGI Desktop Duplication (sem borda amarela no Windows 10).
//! Entrega texturas D3D11 direto para o pipeline da GPU (`gpu::CopyRing` -> `gpu::Converter` ->
//! `mf::HwEncoder`), desenhando o cursor do mouse apenas no retângulo pequeno onde ele está.

use crate::capture::RawFrame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;

pub struct DxgiControl {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl DxgiControl {
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

/// Estado do cursor retornado pela DXGI (quando o Windows usa cursor em plano de hardware).
#[derive(Default)]
struct CursorState {
    visible: bool,
    x: i32,
    y: i32,
    shape: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    patch: Vec<u8>,
}

impl CursorState {
    unsafe fn update(&mut self, dup: &IDXGIOutputDuplication, frame: &DXGI_OUTDUPL_FRAME_INFO) {
        if frame.LastMouseUpdateTime != 0 {
            self.visible = frame.PointerPosition.Visible.as_bool();
            self.x = frame.PointerPosition.Position.x;
            self.y = frame.PointerPosition.Position.y;
        }
        if frame.PointerShapeBufferSize > 0 {
            self.shape.resize(frame.PointerShapeBufferSize as usize, 0);
            let mut req = 0u32;
            let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            if dup
                .GetFramePointerShape(
                    frame.PointerShapeBufferSize,
                    self.shape.as_mut_ptr().cast(),
                    &mut req,
                    &mut info,
                )
                .is_ok()
            {
                self.shape.truncate(req as usize);
                self.info = info;
            }
        }
    }

    fn dims(&self) -> (i32, i32) {
        let w = self.info.Width as i32;
        let h = if self.info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
            (self.info.Height / 2) as i32
        } else {
            self.info.Height as i32
        };
        (w, h)
    }

    /// Compoe o cursor no retângulo visível dele sobre um buffer BGRA de tamanho (bw, bh)
    /// cuja origem no monitor é (bx, by).
    fn blend_onto(&self, buf: &mut [u8], bw: usize, bh: usize, bx: i32, by: i32) {
        let (cw, ch) = self.dims();
        if cw <= 0 || ch <= 0 || self.shape.is_empty() {
            return;
        }
        let pitch = self.info.Pitch as usize;
        let typ = self.info.Type;
        for row in 0..ch {
            let dy = self.y + row - by;
            if dy < 0 || dy >= bh as i32 {
                continue;
            }
            for col in 0..cw {
                let dx = self.x + col - bx;
                if dx < 0 || dx >= bw as i32 {
                    continue;
                }
                let di = (dy as usize * bw + dx as usize) * 4;
                if typ == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
                    let si = row as usize * pitch + col as usize * 4;
                    if si + 4 > self.shape.len() {
                        continue;
                    }
                    let (sb, sg, sr, sa) = (self.shape[si] as u32, self.shape[si + 1] as u32, self.shape[si + 2] as u32, self.shape[si + 3] as u32);
                    if sa == 0 {
                        continue;
                    }
                    if sa == 255 {
                        buf[di] = sb as u8;
                        buf[di + 1] = sg as u8;
                        buf[di + 2] = sr as u8;
                        buf[di + 3] = 255;
                    } else {
                        let inv = 255 - sa;
                        buf[di] = ((sb * sa + buf[di] as u32 * inv) / 255) as u8;
                        buf[di + 1] = ((sg * sa + buf[di + 1] as u32 * inv) / 255) as u8;
                        buf[di + 2] = ((sr * sa + buf[di + 2] as u32 * inv) / 255) as u8;
                        buf[di + 3] = 255;
                    }
                } else if typ == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
                    let si = row as usize * pitch + col as usize * 4;
                    if si + 4 > self.shape.len() {
                        continue;
                    }
                    if self.shape[si + 3] == 0 {
                        buf[di..di + 3].copy_from_slice(&self.shape[si..si + 3]);
                    } else {
                        buf[di] ^= self.shape[si];
                        buf[di + 1] ^= self.shape[si + 1];
                        buf[di + 2] ^= self.shape[si + 2];
                    }
                    buf[di + 3] = 255;
                } else if typ == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
                    let byte_idx = (col / 8) as usize;
                    let mask = 0x80u8 >> (col % 8);
                    let and_i = row as usize * pitch + byte_idx;
                    let xor_i = (row + ch) as usize * pitch + byte_idx;
                    if xor_i >= self.shape.len() {
                        continue;
                    }
                    let and_bit = (self.shape[and_i] & mask) != 0;
                    let xor_bit = (self.shape[xor_i] & mask) != 0;
                    match (and_bit, xor_bit) {
                        (false, false) => {
                            buf[di] = 0;
                            buf[di + 1] = 0;
                            buf[di + 2] = 0;
                        }
                        (false, true) => {
                            buf[di] = 255;
                            buf[di + 1] = 255;
                            buf[di + 2] = 255;
                        }
                        (true, true) => {
                            buf[di] ^= 255;
                            buf[di + 1] ^= 255;
                            buf[di + 2] ^= 255;
                        }
                        (true, false) => {}
                    }
                    buf[di + 3] = 255;
                }
            }
        }
    }

    /// Desenha o cursor sobre uma textura GPU (`tex`) copiando só o retângulo pequeno dele (~32×32).
    unsafe fn draw_on_gpu(&mut self, dev: &ID3D11Device, ctx: &ID3D11DeviceContext, tex: &ID3D11Texture2D, fw: u32, fh: u32) {
        if !self.visible || self.shape.is_empty() {
            return;
        }
        let (cw, ch) = self.dims();
        if cw <= 0 || ch <= 0 {
            return;
        }
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + cw).min(fw as i32);
        let y1 = (self.y + ch).min(fh as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let bw = (x1 - x0) as u32;
        let bh = (y1 - y0) as u32;

        let need_new = self.staging.as_ref().is_none_or(|(_, sw, sh)| *sw < bw || *sh < bh);
        if need_new {
            let sw = bw.max(64);
            let sh = bh.max(64);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: sw,
                Height: sh,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut st = None;
            if dev.CreateTexture2D(&desc, None, Some(&mut st)).is_err() {
                return;
            }
            self.staging = st.map(|t| (t, sw, sh));
        }
        let Some((st, _, _)) = &self.staging else { return };
        let src_box = D3D11_BOX { left: x0 as u32, top: y0 as u32, front: 0, right: x1 as u32, bottom: y1 as u32, back: 1 };
        ctx.CopySubresourceRegion(st, 0, 0, 0, 0, tex, 0, Some(&src_box));

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
            return;
        }
        let row_bytes = bw as usize * 4;
        let mut patch = std::mem::take(&mut self.patch);
        patch.resize(row_bytes * bh as usize, 0);
        let src_ptr = mapped.pData as *const u8;
        for row in 0..bh as usize {
            std::ptr::copy_nonoverlapping(src_ptr.add(row * mapped.RowPitch as usize), patch.as_mut_ptr().add(row * row_bytes), row_bytes);
        }
        ctx.Unmap(st, 0);

        self.blend_onto(&mut patch, bw as usize, bh as usize, x0, y0);
        ctx.UpdateSubresource(tex, 0, Some(&src_box), patch.as_ptr().cast(), row_bytes as u32, 0);
        self.patch = patch;
    }
}

/// Encontra o adaptador e a saída DXGI correspondentes a um `HMONITOR`.
unsafe fn find_output(target: HMONITOR) -> windows::core::Result<(IDXGIAdapter1, IDXGIOutput1)> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut ai = 0u32;
    while let Ok(adapter) = factory.EnumAdapters1(ai) {
        let mut oi = 0u32;
        while let Ok(output) = adapter.EnumOutputs(oi) {
            if let Ok(desc) = output.GetDesc() {
                if desc.Monitor == target {
                    return Ok((adapter, output.cast()?));
                }
            }
            oi += 1;
        }
        ai += 1;
    }
    Err(windows::core::Error::from_hresult(DXGI_ERROR_NOT_FOUND))
}

unsafe fn create_device(adapter: &IDXGIAdapter1) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut dev = None;
    let mut ctx = None;
    D3D11CreateDevice(
        adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        Some(&mut ctx),
    )?;
    Ok((dev.unwrap(), ctx.unwrap()))
}

unsafe fn readback_cpu(
    dev: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    staging: &mut Option<(ID3D11Texture2D, u32, u32)>,
    w: u32,
    h: u32,
) -> Option<Vec<u8>> {
    if staging.as_ref().is_none_or(|(_, sw, sh)| *sw != w || *sh != h) {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        dev.CreateTexture2D(&desc, None, Some(&mut t)).ok()?;
        *staging = t.map(|t| (t, w, h));
    }
    let (st, _, _) = staging.as_ref()?;
    ctx.CopyResource(st, tex);
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
    let row_bytes = w as usize * 4;
    let mut out = vec![0u8; row_bytes * h as usize];
    let src = mapped.pData as *const u8;
    for row in 0..h as usize {
        std::ptr::copy_nonoverlapping(src.add(row * mapped.RowPitch as usize), out.as_mut_ptr().add(row * row_bytes), row_bytes);
    }
    ctx.Unmap(st, 0);
    Some(out)
}

/// Inicia a captura do monitor `hmonitor` por DXGI Desktop Duplication (sem borda amarela).
/// Devolve `Err` se a DXGI não estiver disponível para este monitor (quem chama cai para WGC).
pub fn start(hmonitor: *mut std::ffi::c_void, fps: u32, tx: SyncSender<RawFrame>) -> Result<DxgiControl, String> {
    let target_raw = hmonitor as isize;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let min_gap = Duration::from_micros(850_000 / fps.max(1) as u64);

    let handle = std::thread::Builder::new()
        .name("lume-dxgi".into())
        .spawn(move || {
            let target = HMONITOR(target_raw as *mut std::ffi::c_void);
            let setup = unsafe {
                find_output(target).and_then(|(adapter, output1)| {
                    let (dev, ctx) = create_device(&adapter)?;
                    let dup = output1.DuplicateOutput(&dev)?;
                    Ok((dev, ctx, output1, dup))
                })
            };
            let (dev, ctx, output1, mut dup) = match setup {
                Ok(v) => {
                    let _ = ready_tx.send(Ok(()));
                    v
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.message()));
                    return;
                }
            };

            let gpu_ok = crate::gpu::prepare_device(&dev);
            let mut ring = crate::gpu::CopyRing::new();
            let mut cursor = CursorState::default();
            let mut cpu_staging: Option<(ID3D11Texture2D, u32, u32)> = None;
            let mut last_sent: Option<Instant> = None;

            while !stop2.load(Ordering::SeqCst) {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut res = None;
                let got = unsafe { dup.AcquireNextFrame(100, &mut info, &mut res) };
                match got {
                    Ok(()) => {
                        unsafe {
                            cursor.update(&dup, &info);
                            let now = Instant::now();
                            let due = last_sent.is_none_or(|t| now.duration_since(t) >= min_gap);
                            if due {
                                if let Some(tex) = res.and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                                    let mut d = D3D11_TEXTURE2D_DESC::default();
                                    tex.GetDesc(&mut d);
                                    let (w, h) = (d.Width as usize, d.Height as usize);
                                    if gpu_ok && !crate::gpu::disabled() {
                                        if let Ok(g) = ring.copy(&dev, &ctx, &tex) {
                                            cursor.draw_on_gpu(&dev, &ctx, &g.tex, d.Width, d.Height);
                                            let _ = tx.try_send(RawFrame { bgra: vec![], width: w, height: h, gpu: Some(g) });
                                            last_sent = Some(now);
                                        }
                                    } else if let Some(mut bgra) = readback_cpu(&dev, &ctx, &tex, &mut cpu_staging, d.Width, d.Height) {
                                        if cursor.visible {
                                            cursor.blend_onto(&mut bgra, w, h, 0, 0);
                                        }
                                        let _ = tx.try_send(RawFrame { bgra, width: w, height: h, gpu: None });
                                        last_sent = Some(now);
                                    }
                                }
                            }
                            let _ = dup.ReleaseFrame();
                        }
                    }
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                    Err(_) => {
                        // DXGI_ERROR_ACCESS_LOST (UAC, tela cheia exclusiva, troca de resolução): recria.
                        std::thread::sleep(Duration::from_millis(150));
                        if stop2.load(Ordering::SeqCst) {
                            break;
                        }
                        if let Ok(new_dup) = unsafe { output1.DuplicateOutput(&dev) } {
                            dup = new_dup;
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(DxgiControl { stop, handle }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            Err("DXGI não respondeu".into())
        }
    }
}
